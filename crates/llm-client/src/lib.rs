//! LLM provider abstraction, usage tracking, and prompt templates.
//!
//! See crate-level README for status. Phase 2 work proceeds by TDD.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};

mod price;
mod provider;
mod retry;
mod session_usage;
mod transport;

pub mod prompt;

pub use price::{PriceBook, PriceEntry};
pub use prompt::*;
pub use provider::*;
pub use retry::{RetryPolicy, RetryingTransport, Sleeper, TokioSleeper};
pub use session_usage::SessionUsage;
pub use transport::*;

/// Token usage returned by every LLM call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}
