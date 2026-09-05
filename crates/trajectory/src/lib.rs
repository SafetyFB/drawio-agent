//! Per-session execution log (DeepSeek Harness style trajectory).
//!
//! Phase 5 scaffold. Public API and exact event variants will be
//! solidified through TDD in subsequent iterations.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod event;
pub mod store;

pub use event::{TrajectoryEvent, TrajectoryEventKind};
pub use store::TrajectoryStore;

/// Stable session identifier used to scope a trajectory. Reuses the
/// `drawio_agent_server::SessionId` type in production; for now we
/// accept any `String` here so this crate stays server-agnostic.
pub type SessionKey = String;

/// Aggregated token usage across all events in a trajectory.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageSummary {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub llm_calls: u32,
    pub render_calls: u32,
    pub errors: u32,
}

impl UsageSummary {
    /// Empty / zero summary.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Add another summary into `self`.
    pub fn add(&mut self, other: &UsageSummary) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.llm_calls += other.llm_calls;
        self.render_calls += other.render_calls;
        self.errors += other.errors;
    }

    /// Add raw token counts from a single LLM response.
    pub fn add_llm_usage(&mut self, input: u64, output: u64) {
        self.input_tokens += input;
        self.output_tokens += output;
        self.llm_calls += 1;
    }

    /// Increment render-call counter.
    pub fn add_render_call(&mut self) {
        self.render_calls += 1;
    }

    /// Increment error counter.
    pub fn add_error(&mut self) {
        self.errors += 1;
    }

    /// Total tokens (input + output).
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// One event in the trajectory log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// UUIDv4 for stable identity.
    pub id: Uuid,
    /// Monotonic sequence number within a session (starts at 0).
    pub seq: u64,
    /// Wall-clock time the event was recorded.
    pub at: std::time::SystemTime,
    pub session_id: SessionKey,
    pub kind: TrajectoryEvent,
}
