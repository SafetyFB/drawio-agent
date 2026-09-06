//! Agent Loop: Generate (baseline) → Render → Fix state machine (v2).
//!
//! This crate orchestrates the capabilities (LLM codegen, renderer,
//! single-call multimodal fix) into a single loop that drives a session
//! from initial prompt to a model-confirmed diagram.
//!
//! v2 semantics: the separate Review phase is gone. Each round renders the
//! current XML and makes ONE multimodal call — the model sees the latest
//! render, self-reviews it, and edits the XML in the same response
//! (`{done, xml, issues}` envelope). Convergence is the model's `done`
//! flag plus deps-side validation (parse / no-op / scope merge).

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod deps;
pub mod runner;
pub mod phase;

pub use deps::{AgentDeps, FixError, FixOutcome, FixRequest};
pub use runner::run;
pub use phase::{LoopPhase, LoopState};

/// Callback invoked with each trajectory event as the loop records it,
/// used to stream progress to subscribers while the run is still in
/// flight (the events arrive in the order the loop records them).
pub type ProgressCb = Arc<dyn Fn(drawio_agent_trajectory::TrajectoryEvent) + Send + Sync>;

/// Configuration for a single Agent Loop run.
#[derive(Clone)]
pub struct AgentLoop {
    /// The user's ask. Kept stable across rounds and sent to the model
    /// every round, so a stateless model never loses the objective.
    pub prompt: String,
    /// Optional starting XML (skips the initial Generate phase when Some).
    pub initial_xml: Option<String>,
    /// Maximum fix rounds (render + single multimodal call each).
    /// Default 5.
    pub max_iterations: u32,
    /// Cells the user explicitly targeted (canvas selection). Non-empty
    /// switches every fix round into Plan-B scope mode: the model only
    /// sees/edits these cells. When empty, scoping falls back to the
    /// self-reported issue cells of the previous round.
    pub patch_cell_ids: Vec<String>,
    /// Optional reviewer focus checks, forwarded to every fix round.
    /// Default: empty (the model decides what to look for).
    pub review_checks: Vec<String>,
    /// Session memory (R2): summaries of earlier turns, injected into every
    /// fix round as background context so follow-up runs remember what the
    /// user already asked and what was already done. Kept stable across
    /// rounds (only the round-specific feedback notes change).
    pub memory: Vec<String>,
    /// Optional live-progress callback. Invoked (synchronously, in record
    /// order) for every trajectory event the loop records, before the run
    /// finishes.
    pub progress_cb: Option<ProgressCb>,
}

impl std::fmt::Debug for AgentLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoop")
            .field("prompt", &self.prompt)
            .field("initial_xml", &self.initial_xml)
            .field("max_iterations", &self.max_iterations)
            .field("patch_cell_ids", &self.patch_cell_ids)
            .field("review_checks", &self.review_checks)
            .field("memory", &self.memory)
            .field("progress_cb", &"<callback>")
            .finish()
    }
}

impl AgentLoop {
    /// Default: max 5 iterations, no specific patch cells or checks.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            initial_xml: None,
            max_iterations: 5,
            patch_cell_ids: Vec::new(),
            review_checks: Vec::new(),
            memory: Vec::new(),
            progress_cb: None,
        }
    }
}

/// Errors from the Agent Loop.
#[derive(Debug, Error)]
pub enum LoopError {
    #[error("LLM error during {phase}: {message}")]
    Llm { phase: LoopPhase, message: String },
    #[error("render error during {phase}: {message}")]
    Render { phase: LoopPhase, message: String },
    #[error("max iterations ({0}) reached without converging")]
    MaxIterations(u32),
    #[error("empty LLM response during {phase}")]
    EmptyResponse { phase: LoopPhase },
}

/// Final result of an Agent Loop run, regardless of outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutcome {
    /// The XML at termination (either the converged diagram or the best
    /// attempt after `max_iterations`).
    ///
    /// Serialized as `xml` so the wire shape stays short and friendly to
    /// the JS client (`result.xml`); Rust callers use the field directly
    /// via `final_xml`.
    #[serde(rename = "xml")]
    pub final_xml: String,
    /// How many fix rounds ran.
    pub iterations: u32,
    /// `true` when the final phase is `Done` (the last fix round reported
    /// `done=true`). Serialized as a field (not just a method) so the JS
    /// client can branch on it without re-deriving from `final_phase`.
    pub converged: bool,
    /// Final phase at termination (Done or Failed).
    pub final_phase: LoopPhase,
    /// Verdict of the most recent fix round ("pass" for done, "issues"
    /// otherwise; None if no fix round ever ran).
    pub last_verdict: Option<String>,
    /// Count of issues reported in the last fix round.
    pub last_issue_count: u32,
    /// `reasoning` from the most recent fix round (the model's own summary of
    /// what it changed), when the model provided one.
    #[serde(default)]
    pub last_reasoning: Option<String>,
    /// All trajectory events recorded during the run.
    pub trajectory: Vec<drawio_agent_trajectory::Event>,
}

impl AgentOutcome {
    /// `true` when the final phase is `Done` (verdict was "pass").
    /// Kept for backward compat; new code should use the `converged`
    /// field directly (it's serialized, unlike this method).
    pub fn converged(&self) -> bool {
        matches!(self.final_phase, LoopPhase::Done)
    }
}
