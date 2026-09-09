//! Memory management: trim transcript, fold old images/reads.

use crate::chat::{Message, Part};

/// Is this user-role message one the harness itself synthesized (tool
/// result / protocol correction) rather than a real user ask? Memory trimming
/// uses this: the transcript must never start mid-ask (a dangling tool
/// result or correction with its envelope already dropped).
fn is_synthetic_user(m: &Message) -> bool {
    match m.parts.first() {
        Some(Part::Text(t)) => {
            t.starts_with("[工具结果 ")
                || t.starts_with("你的上一条输出不是合法信封")
                || t.starts_with("信封缺少 tool 或 reply 字段")
                || t.starts_with('`')
        }
        _ => true, // image-only user message = view result = synthetic
    }
}

/// Trim `stats.transcript` to fit the memory cap: drop oldest messages
/// until the estimate fits — but only in whole-ask groups, so the surviving
/// transcript always starts at a real user ask (no orphaned tool results).
pub fn trim_memory(stats: &mut crate::engine::SessionStats, cap_tokens: u64) {
    while estimate_tokens(&stats.transcript) > cap_tokens && stats.transcript.len() > 2 {
        stats.transcript.remove(0);
        // Advance to the next ask boundary: keep dropping while the head
        // is not a real user ask (and we still have >2 messages to spare).
        while stats.transcript.len() > 2 {
            let head_is_ask = stats
                .transcript
                .first()
                .is_some_and(|m| m.role == "user" && !is_synthetic_user(m));
            if head_is_ask {
                break;
            }
            stats.transcript.remove(0);
        }
    }
}

/// Replace image parts in every message except the last one with a note,
/// keeping only the most recent screenshot in the transcript.
pub fn fold_old_images(history: &mut [Message]) {
    let last = history.len().saturating_sub(1);
    for (i, m) in history.iter_mut().enumerate() {
        if i == last {
            continue;
        }
        let mut has_image = false;
        for p in &m.parts {
            if matches!(p, Part::ImagePng(_)) {
                has_image = true;
                break;
            }
        }
        if has_image {
            m.parts.retain(|p| !matches!(p, Part::ImagePng(_)));
            m.parts.push(Part::Text("（该轮截图已折叠；如需再看请重新调用 view）".into()));
        }
    }
}

/// Fold older `read` tool results (the largest text payloads in long
/// sessions — up to 150 lines each) into a one-line stub, keeping the most
/// recent [`KEEP_READS`] results verbatim. Safe by design: line numbers
/// drift on every edit, so the prompt already tells the model not to trust
/// stale reads.
pub fn fold_old_reads(history: &mut [Message]) {
    const KEEP_READS: usize = 2;
    let read_idx: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.role == "user"
                && matches!(
                    m.parts.first(),
                    Some(Part::Text(t)) if t.starts_with("[工具结果 read]")
                )
        })
        .map(|(i, _)| i)
        .collect();
    if read_idx.len() <= KEEP_READS {
        return;
    }
    for &i in read_idx.iter().take(read_idx.len() - KEEP_READS) {
        let lines: usize = history[i]
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.lines().count()),
                _ => None,
            })
            .sum();
        history[i].parts = vec![Part::Text(format!(
            "（早前的 read 结果已折叠，约 {lines} 行；行号已随编辑漂移，如需请重新 read）"
        ))];
    }
}

/// Rough token estimate for prompt-size guarding (text chars ≈ 0.5 token
/// for CJK-heavy prompts; images billed by pixel area, since vision
/// providers commonly charge ~750px²/token — well above the old flat 900
/// for full-page screenshots, so the floor keeps the guard conservative).
pub fn estimate_tokens(msgs: &[Message]) -> u64 {
    let mut t = 0u64;
    for m in msgs {
        for p in &m.parts {
            match p {
                Part::Text(s) => t += (s.chars().count() as u64).div_ceil(2),
                Part::ImagePng(png) => {
                    let mut est = 900u64;
                    // PNG IHDR: big-endian width/height at bytes 16..24.
                    if png.len() >= 24 {
                        let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]) as u64;
                        let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]) as u64;
                        est = est.max(w * h / 750);
                    }
                    t += est;
                }
            }
        }
    }
    t
}

/// Default memory cap when no context_length is configured (tokens, estimate).
pub const DEFAULT_MEMORY_TOKENS: usize = 24_000;