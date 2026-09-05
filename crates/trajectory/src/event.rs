//! Trajectory event variants.

use serde::{Deserialize, Serialize};

/// Tagged enum of everything the agent can record into a trajectory.
/// The `kind` field is the JSON tag, `data` is a free-form payload
/// (kept as `serde_json::Value` so future variants don't break older
/// export consumers).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrajectoryEvent {
    /// LLM call dispatched. `data` carries request metadata.
    LlmCallStarted {
        prompt_chars: usize,
        json_mode: bool,
    },
    /// LLM response received. `data` carries usage + finish reason.
    LlmCallCompleted {
        input_tokens: u64,
        output_tokens: u64,
        duration_ms: u64,
        finish_reason: Option<String>,
    },
    /// Render driver invoked.
    RenderStarted {
        scale: f64,
    },
    /// Render completed.
    RenderCompleted {
        bytes: usize,
        duration_ms: u64,
    },
    /// An action failed.
    Error {
        stage: String,
        message: String,
    },
    /// Arbitrary state transition marker (e.g. "phase: review").
    StateTransition {
        from: Option<String>,
        to: String,
    },
}

/// Short tag describing a variant (e.g. for filtering).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrajectoryEventKind {
    LlmCallStarted,
    LlmCallCompleted,
    RenderStarted,
    RenderCompleted,
    Error,
    StateTransition,
}

impl TrajectoryEvent {
    pub fn kind(&self) -> TrajectoryEventKind {
        match self {
            Self::LlmCallStarted { .. } => TrajectoryEventKind::LlmCallStarted,
            Self::LlmCallCompleted { .. } => TrajectoryEventKind::LlmCallCompleted,
            Self::RenderStarted { .. } => TrajectoryEventKind::RenderStarted,
            Self::RenderCompleted { .. } => TrajectoryEventKind::RenderCompleted,
            Self::Error { .. } => TrajectoryEventKind::Error,
            Self::StateTransition { .. } => TrajectoryEventKind::StateTransition,
        }
    }
}
