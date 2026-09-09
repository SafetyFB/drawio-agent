//! The harness engine: one user ask -> model decides tool calls -> results
//! are appended -> until the model says done. Plain ReAct over a JSON
//! envelope — no bespoke pipeline, no phases. Works with any
//! OpenAI-compatible endpoint.
//!
//! Accounting (R6): every LLM call's usage is parsed and accumulated into
//! [`SessionStats`]; cost follows the configured ¥/1M prices; an optional
//! budget stops the run before spending more. A configured context length
//! guards prompt size before each call.

use serde_json::json;

use std::sync::Arc;

use crate::chat::{Message, Usage};


/// Live progress events emitted by [`Harness::run`] as an ask unfolds —
/// powers R4 progress rendering (web stream / CLI live trace).
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// A model round is starting (index/max = rounds, not tool calls).
    Turn { index: usize },
    /// Raw assistant envelope the model just produced.
    ModelOutput { raw: String },
    /// About to execute a tool.
    Tool { name: String, args: String },
    /// Tool finished (text preview; image tools mark has_image).
    ToolResult { name: String, text: String, has_image: bool },
    /// One LLM call's usage + cost (¥ under configured prices).
    Usage { usage: Usage, cost_yuan: f64 },
    /// Ask finished with the model's final reply.
    Final { reply: String },
}

impl From<EngineEvent> for serde_json::Value {
    fn from(e: EngineEvent) -> Self {
        match e {
            EngineEvent::Turn { index } => json!({ "type": "turn", "index": index }),
            EngineEvent::ModelOutput { raw } => json!({ "type": "model", "preview": raw.chars().take(300).collect::<String>() }),
            EngineEvent::Tool { name, args } => json!({ "type": "tool", "name": name, "args": args.chars().take(200).collect::<String>() }),
            EngineEvent::ToolResult { name, text, has_image } => json!({ "type": "tool_result", "name": name, "preview": text.lines().next().unwrap_or("").chars().take(200).collect::<String>(), "has_image": has_image }),
            EngineEvent::Usage { usage, cost_yuan } => json!({ "type": "usage", "in": usage.input_tokens, "out": usage.output_tokens, "cost_yuan": cost_yuan }),
            EngineEvent::Final { reply } => json!({ "type": "reply", "reply": reply }),
        }
    }
}

/// Callback type for progress events (web stream / CLI trace).
pub type ProgressFn = Arc<dyn Fn(EngineEvent) + Send + Sync>;

#[derive(Debug)]
pub struct TurnOutcome {
    /// The model's final reply to the user.
    pub reply: String,
    /// How many tool calls ran before done.
    pub tool_calls: usize,
    /// Raw assistant JSON envelopes (for debugging / trajectory).
    pub envelopes: Vec<String>,
    /// Tokens spent by this ask (input + output, summed over calls).
    pub usage: Usage,
    /// ¥ cost of this ask under the configured prices.
    pub cost_yuan: f64,
}

/// Running totals across asks (kept by the caller: REPL / web session).
/// Also carries the rolling multi-turn memory (R5): the transcript of past
/// asks is re-injected into every new ask and trimmed to a token budget.
#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    pub usage: Usage,
    pub cost_yuan: f64,
    /// Past-ask transcript (user asks / assistant envelopes / tool results),
    /// trimmed to a token cap; images never survive (folded earlier).
    pub transcript: Vec<Message>,
}

impl SessionStats {
    pub fn add(&mut self, u: &Usage, cost: f64) {
        self.usage.add(u);
        self.cost_yuan += cost;
    }
}

#[derive(Debug)]
pub struct Harness {
    pub max_turns: usize,
    /// How many times a failed LLM call is retried before the ask aborts
    /// (providers 500 occasionally on large multimodal contexts).
    pub max_llm_retries: u32,
}

impl Default for Harness {
    fn default() -> Self {
        Self {
            max_turns: crate::config::default_max_turns(),
            max_llm_retries: 2,
        }
    }
}

/// Per-run knobs (snapshot of the config at ask time; hot config changes
/// take effect on the next ask).
#[derive(Debug, Clone)]
pub struct RunOpts {
    /// Fast path: `thinking: {type: disabled}`.
    pub no_think: bool,
    /// Configured context window in tokens (None = no guard).
    pub context_limit: Option<u64>,
    /// ¥ per 1M input tokens (0 = count only).
    pub price_input_per_m: f64,
    /// ¥ per 1M output tokens.
    pub price_output_per_m: f64,
    /// ¥ left of the session budget for this run. When the run's spend
    /// reaches this, the engine stops with a clear budget message.
    pub budget_remaining: f64,
    /// Max model rounds for this run (configurable in the settings panel).
    pub max_turns: usize,
    /// 旧版 mxGraph 画布回退模式（drawio webapp 不可用）：提示词附带
    /// 2018 viewer 的形状拼写约束。
    pub legacy_viewer: bool,
    /// Sampling temperature (`None` = chat default 0.2).
    pub temperature: Option<f64>,
}

impl Default for RunOpts {
    fn default() -> Self {
        Self {
            no_think: false,
            context_limit: None,
            price_input_per_m: 0.0,
            price_output_per_m: 0.0,
            budget_remaining: f64::INFINITY,
            max_turns: crate::config::default_max_turns(),
            legacy_viewer: false,
            temperature: None,
        }
    }
}

impl RunOpts {
    pub fn from_settings(s: &crate::config::LlmSettings) -> Self {
        Self {
            no_think: matches!(s.thinking, crate::config::ThinkingMode::NoThink),
            context_limit: s.context_length,
            price_input_per_m: s.price_input_per_m,
            price_output_per_m: s.price_output_per_m,
            budget_remaining: f64::INFINITY,
            max_turns: s.max_turns.max(1),
            legacy_viewer: false,
            temperature: s.temperature,
        }
    }
}

// Re-export the main run function from turn_loop
pub use crate::turn_loop::HarnessRunExt;