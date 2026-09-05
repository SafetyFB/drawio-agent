//! State-machine phases.

use serde::{Deserialize, Serialize};

/// Phases the Agent Loop moves through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LoopPhase {
    /// Initial state before any action has run.
    Pending,
    /// Codegen call to produce (or update) the diagram XML.
    Generate,
    /// Renderer producing a PNG from the current XML.
    Render,
    /// VLM visual review of the current PNG.
    Review,
    /// Patch: re-run codegen scoped to the cells flagged by the reviewer.
    Patch,
    /// Review returned verdict "pass" — loop converged.
    Done,
    /// `max_iterations` reached without converging; final_xml is the best
    /// attempt.
    Failed,
}

impl LoopPhase {
    /// Human-readable label used in trajectory events.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Generate => "generate",
            Self::Render => "render",
            Self::Review => "review",
            Self::Patch => "patch",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for LoopPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Live state of an Agent Loop run, useful for streaming updates and
/// debugging. Snapshotted at the end of [`run`] into [`AgentOutcome`].
///
/// [`run`]: crate::run::run
/// [`AgentOutcome`]: crate::AgentOutcome
#[derive(Debug, Clone)]
pub struct LoopState {
    pub phase: LoopPhase,
    pub iteration: u32,
    pub current_xml: Option<String>,
    pub last_verdict: Option<String>,
    pub last_issue_count: u32,
}
