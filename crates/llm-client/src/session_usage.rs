//! Per-session usage aggregation and budget guard.
//!
//! Tracks token usage keyed by model and by purpose, plus accumulated cost
//! against an optional budget.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::Usage;

/// Aggregate token usage and cost for a single session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionUsage {
    /// Token usage summed per model name.
    pub by_model: HashMap<String, Usage>,
    /// Token usage summed per purpose string.
    pub by_purpose: HashMap<String, Usage>,
    /// Accumulated cost of every recorded call.
    pub total_cost: f64,
    /// Optional session budget; `None` means no budget is enforced.
    pub budget: Option<f64>,
}

impl SessionUsage {
    /// A session with no budget, zero tokens, and zero cost.
    pub fn new() -> Self {
        Self::default()
    }

    /// A session with the given budget.
    pub fn with_budget(budget: f64) -> Self {
        Self {
            budget: Some(budget),
            ..Self::default()
        }
    }

    /// Record one call, aggregating its usage under `model` and `purpose`
    /// and adding `cost` to the session total.
    pub fn record(&mut self, model: &str, purpose: &str, usage: &Usage, cost: f64) {
        self.by_model
            .entry(model.to_string())
            .or_default()
            .add(usage);
        self.by_purpose
            .entry(purpose.to_string())
            .or_default()
            .add(usage);
        self.total_cost += cost;
    }

    /// Sum of input tokens across all recorded models.
    pub fn total_input(&self) -> u64 {
        self.by_model.values().map(|u| u.input_tokens).sum()
    }

    /// Sum of output tokens across all recorded models.
    pub fn total_output(&self) -> u64 {
        self.by_model.values().map(|u| u.output_tokens).sum()
    }

    /// Sum of input and output tokens across all recorded models.
    pub fn total_tokens(&self) -> u64 {
        self.total_input() + self.total_output()
    }

    /// Whether accumulated cost has reached or passed the budget.
    ///
    /// Always `false` when no budget is set.
    pub fn exceeds_budget(&self) -> bool {
        match self.budget {
            Some(budget) => self.total_cost >= budget,
            None => false,
        }
    }

    /// Remaining budget (`budget - total_cost`), or `None` when no budget
    /// is set. May be negative once cost exceeds the budget.
    pub fn budget_remaining(&self) -> Option<f64> {
        self.budget.map(|budget| budget - self.total_cost)
    }
}