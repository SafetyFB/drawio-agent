//! State-machine phases.

use serde::{Deserialize, Serialize};

/// Phases the Agent Loop moves through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LoopPhase {
    /// Initial state before any action has run.
    Pending,
    /// Codegen call to produce the initial diagram XML (only when the
    /// caller supplied no starting XML).
    Generate,
    /// Renderer producing a PNG from the current XML.
    Render,
    /// The v2 single-call fix round: the LLM sees the latest render and
    /// self-reviews + edits the XML in ONE multimodal call (review is no
    /// longer a separate phase).
    Fix,
    /// A fix round returned `done=true` (and its output was acceptable) —
    /// loop converged.
    Done,
    /// `max_iterations` reached without a `done=true` round; final_xml is
    /// the best attempt.
    Failed,
}

impl LoopPhase {
    /// Human-readable label used in trajectory events.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Generate => "generate",
            Self::Render => "render",
            Self::Fix => "fix",
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
    /// `reasoning` from the most recent fix round (model self-report).
    pub last_reasoning: Option<String>,
}
