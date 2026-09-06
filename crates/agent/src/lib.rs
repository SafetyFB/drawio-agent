//! Agent Loop: Generate → Render → Review → Patch state machine.
//!
//! This crate orchestrates the four dependent capabilities (LLM codegen,
//! renderer, visual reviewer, patcher) into a single loop that drives a
//! session from initial prompt to verified-correct diagram.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod deps;
pub mod runner;
pub mod phase;

pub use deps::AgentDeps;
pub use runner::run;
pub use phase::{LoopPhase, LoopState};

/// Callback invoked with each trajectory event as the loop records it,
/// used to stream progress to subscribers while the run is still in
/// flight (the events arrive in the order the loop records them).
pub type ProgressCb = Arc<dyn Fn(drawio_agent_trajectory::TrajectoryEvent) + Send + Sync>;

/// Configuration for a single Agent Loop run.
#[derive(Clone)]
pub struct AgentLoop {
    /// Initial prompt for the codegen step. Required when `initial_xml`
    /// is `None`; ignored otherwise.
    pub prompt: String,
    /// Optional starting XML (skips the initial Generate phase when Some).
    pub initial_xml: Option<String>,
    /// Maximum render-review-patch iterations. Default 5.
    pub max_iterations: u32,
    /// Cells to focus on during Patch (e.g. specific node IDs).
    pub patch_cell_ids: Vec<String>,
    /// Optional reviewer checks. Default: empty (VLM decides what to look for).
    pub review_checks: Vec<String>,
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
    pub final_xml: String,
    /// How many full Generate-Render-Review iterations ran.
    pub iterations: u32,
    /// Final phase at termination (Done or Failed).
    pub final_phase: LoopPhase,
    /// Verdict of the most recent review (None if review never ran).
    pub last_verdict: Option<String>,
    /// Count of review issues in the last review.
    pub last_issue_count: u32,
    /// All trajectory events recorded during the run.
    pub trajectory: Vec<drawio_agent_trajectory::Event>,
}

impl AgentOutcome {
    /// `true` when the final phase is `Done` (verdict was "pass").
    pub fn converged(&self) -> bool {
        matches!(self.final_phase, LoopPhase::Done)
    }
}
