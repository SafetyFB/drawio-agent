//! Context/budget guards for the engine loop.

use crate::config::LlmSettings;
use crate::engine::RunOpts;

/// Check context length guard.
pub fn check_context_limit(
    history: &[crate::chat::Message],
    opts: &RunOpts,
) -> Option<String> {
    if let Some(limit) = opts.context_limit {
        let est = crate::memory::estimate_tokens(history);
        if est > limit {
            return Some(format!(
                "上下文估算 {est} tokens 超过配置的 context_length={limit}。\
                 请调高 ⚙ 设置里的上下文长度，或开始新会话。"
            ));
        }
    }
    None
}

/// Check budget guard.
pub fn check_budget(
    spent: f64,
    opts: &RunOpts,
) -> Option<String> {
    if opts.budget_remaining.is_finite() && spent >= opts.budget_remaining {
        Some(format!(
            "会话预算已用尽（本次已花 ¥{spent:.4} ≥ 预算 ¥{:.2}）。\
             可在 ⚙ 设置里调高预算后继续。",
            opts.budget_remaining
        ))
    } else {
        None
    }
}

/// Build a minimal LlmSettings for cost calculation from RunOpts.
pub fn price_opts(o: &RunOpts) -> LlmSettings {
    LlmSettings {
        price_input_per_m: o.price_input_per_m,
        price_output_per_m: o.price_output_per_m,
        ..Default::default()
    }
}