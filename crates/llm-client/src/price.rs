//! Price book — token usage to currency conversion.
//!
//! Models are keyed by name and carry per-1k-token input/output prices.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::Usage;

/// Per-1k-token pricing for a single model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceEntry {
    pub input_per_1k: f64,
    pub output_per_1k: f64,
    pub currency: String,
}

impl Default for PriceEntry {
    fn default() -> Self {
        Self {
            input_per_1k: 0.0,
            output_per_1k: 0.0,
            currency: "USD".to_string(),
        }
    }
}

/// A collection of model prices keyed by model name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PriceBook {
    entries: HashMap<String, PriceEntry>,
}

impl PriceBook {
    /// An empty price book.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// A price book pre-populated with common models.
    pub fn default_book() -> Self {
        let mut book = Self::new();
        book.insert(
            "glm-4-flash".to_string(),
            PriceEntry {
                input_per_1k: 0.0001,
                output_per_1k: 0.0001,
                currency: "CNY".to_string(),
            },
        );
        book.insert(
            "qwen-vl-plus".to_string(),
            PriceEntry {
                input_per_1k: 0.0015,
                output_per_1k: 0.0045,
                currency: "CNY".to_string(),
            },
        );
        book.insert(
            "gpt-4o".to_string(),
            PriceEntry {
                input_per_1k: 0.0025,
                output_per_1k: 0.01,
                currency: "USD".to_string(),
            },
        );
        book
    }

    /// Insert or replace the price entry for a model.
    pub fn insert(&mut self, model: String, entry: PriceEntry) {
        self.entries.insert(model, entry);
    }

    /// Look up the price entry for a model.
    pub fn get(&self, model: &str) -> Option<&PriceEntry> {
        self.entries.get(model)
    }

    /// Cost of a usage sample in the entry's currency.
    ///
    /// Unknown models cost `0.0`; zero usage also yields `0.0`.
    pub fn cost(&self, model: &str, usage: &Usage) -> f64 {
        match self.entries.get(model) {
            Some(entry) => {
                (usage.input_tokens as f64 / 1000.0) * entry.input_per_1k
                    + (usage.output_tokens as f64 / 1000.0) * entry.output_per_1k
            }
            None => 0.0,
        }
    }
}