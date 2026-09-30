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
///
/// 只折叠**大**结果（> [`FOLD_MIN_LINES`] 行）：query 命中只有几行，
/// 折叠它会把模型刚收集的信息挖掉——实测模型以 3 节点周期反复 query
/// 时，第 1 条命中总在第 3 条到达时被折叠，stub 还引导它「重新 read」，
/// 直接喂出烧尽 30 轮的死循环。小结果永久保留。
/// 折叠旧 read 结果。返回被折叠各次读取的标识（"range:1-12" /
/// "outline"）——turn_loop 用它给同参守卫发恢复信用：内容已从上下文
/// 消失，此时重读是 stub 明文允许的恢复动作，不是原地打转。
pub fn fold_old_reads(history: &mut [Message]) -> Vec<String> {
    const KEEP_READS: usize = 2;
    const FOLD_MIN_LINES: usize = 8;
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
    let line_counts: Vec<usize> = read_idx
        .iter()
        .map(|&i| {
            history[i]
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text(t) => Some(t.lines().count()),
                    _ => None,
                })
                .sum()
        })
        .collect();
    let keep: Vec<usize> = read_idx
        .iter()
        .zip(&line_counts)
        .rev()
        .filter(|(_, &n)| n > FOLD_MIN_LINES)
        .take(KEEP_READS)
        .map(|(&i, _)| i)
        .collect();
    let mut folded_idents: Vec<String> = Vec::new();
    for (&i, &n) in read_idx.iter().zip(&line_counts) {
        if keep.contains(&i) || n <= FOLD_MIN_LINES {
            continue;
        }
        // 折叠前抽取标识：range/cells 批量的首行是 "@file:a-b …"，
        // outline 是 "全图概览…"（query 命中无固定首行 → 不发信用）。
        let ident = history[i].parts.iter().find_map(|p| match p {
            Part::Text(t) => {
                // 首行是 "[工具结果 read]" 头，正文从第二行开始
                let first = t.lines().nth(1)?;
                if first.starts_with("全图概览") {
                    return Some("outline".to_string());
                }
                let rest = first.strip_prefix('@')?;
                let (_, ab) = rest.split_once(':')?;
                Some(format!("range:{}", ab.split_whitespace().next()?))
            }
            _ => None,
        });
        if let Some(id) = ident {
            folded_idents.push(id);
        }
        history[i].parts = vec![Part::Text(format!(
            "（早前的 read 结果已折叠，约 {n} 行；行号已随编辑漂移，如需请重新 read）"
        ))];
    }
    folded_idents
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::SessionStats;

    #[tokio::test]
    async fn memory_trim_keeps_newest_when_over_cap() {
        // Direct trim check: a transcript larger than the cap sheds its
        // oldest messages but keeps the newest ask intact.
        let mut stats = SessionStats::default();
        for i in 0..10 {
            let long = format!(
                "第 {i} 轮：{}",
                "这是一段很长的记忆内容，用来撑大 token 估算值。".repeat(300)
            );
            stats.transcript.push(Message::user(long));
            stats.transcript.push(Message::assistant("好。"));
        }
        let before = estimate_tokens(&stats.transcript);
        assert!(before > 10_000, "setup too small: {before}");
        trim_memory(&mut stats, 3_000);
        let after = estimate_tokens(&stats.transcript);
        // Either the cap is met, or we kept only the newest ask (user+reply)
        // because a single ask alone still exceeds the cap.
        assert!(after <= 3_000 || stats.transcript.len() <= 2, "trim failed: {after}");
        let newest_kept = stats.transcript.iter().any(|m| {
            matches!(&m.parts[0], Part::Text(t) if t.contains("第 9 轮"))
        });
        assert!(newest_kept, "最新一轮应保留");
        let oldest_gone = !stats.transcript.iter().any(|m| {
            matches!(&m.parts[0], Part::Text(t) if t.contains("第 0 轮"))
        });
        assert!(oldest_gone, "最老一轮应被裁剪");
    }

    #[test]
    fn memory_trim_never_starts_mid_ask() {
        // 裁剪必须落在 ask 边界：幸存的 transcript 首条应是真实用户
        // ask，而不是孤儿工具结果/纠错消息（其信封已被裁掉）。
        let mut stats = SessionStats::default();
        // ask 1: 大段内容（工具结果很长，撑爆 cap）
        stats.transcript.push(Message::user("第一问：把 a 改绿"));
        stats.transcript.push(Message::assistant(r#"{"tool":"edit","args":{}}"#));
        stats.transcript
            .push(Message::user(format!("[工具结果 edit]\n{}", "行".repeat(4_000))));
        stats.transcript.push(Message::assistant(r#"{"reply":"done","done":true}"#));
        // ask 2（将被保留）
        stats.transcript.push(Message::user("第二问：把 b 改蓝"));
        stats.transcript.push(Message::assistant(r#"{"reply":"ok","done":true}"#));
        let cap = estimate_tokens(&stats.transcript[4..]); // 恰好容得下 ask 2
        trim_memory(&mut stats, cap);
        assert!(
            !stats.transcript.is_empty(),
            "cap 至少容得下一轮，不应裁空"
        );
        let head = &stats.transcript[0];
        assert_eq!(head.role, "user", "首条应为用户 ask");
        match &head.parts[0] {
            Part::Text(t) => {
                assert_eq!(t, "第二问：把 b 改蓝", "首条应是第二问完整开头");
                assert!(!t.starts_with("[工具结果"), "不得以孤儿工具结果开头");
            }
            _ => panic!("应为文本 part"),
        }
        assert!(stats.transcript.len() == 2, "恰保留一轮 ask: {}", stats.transcript.len());
    }

    #[test]
    fn fold_old_reads_keeps_two_newest() {
        let history: Vec<Message> = vec![
            Message::user("看下图"),
            Message::assistant(r#"{"tool":"read","args":{"range":"1-5"}}"#),
            Message::user(format!("[工具结果 read]\n{}", "旧内容\n".repeat(100))),
            Message::assistant(r#"{"tool":"check","args":{}}"#),
            Message::user("[工具结果 check]\ncells=3"),
            Message::assistant(r#"{"tool":"read","args":{"range":"2-6"}}"#),
            Message::user(format!("[工具结果 read]\n{}", "较新内容\n".repeat(100))),
            Message::assistant(r#"{"tool":"read","args":{"range":"3-7"}}"#),
            Message::user(format!("[工具结果 read]\n{}", "最新内容\n".repeat(100))),
        ];
        let mut slice = history.clone();
        fold_old_reads(&mut slice);
        let folded = &slice[2];
        match &folded.parts[0] {
            Part::Text(t) => {
                assert!(t.contains("已折叠"), "最老的 read 应被折叠: {t}");
                assert!(!t.contains("旧内容"), "正文应被移除");
            }
            _ => panic!("应为文本 part"),
        }
        // 最近两条 read 结果保留原文。
        assert!(slice[6].parts.iter().any(|p| matches!(p, Part::Text(t) if t.contains("较新内容"))));
        assert!(slice[8].parts.iter().any(|p| matches!(p, Part::Text(t) if t.contains("最新内容"))));
        // 非 read 的工具结果不动。
        assert!(slice[4].parts.iter().any(|p| matches!(p, Part::Text(t) if t.contains("cells=3"))));
        // 不足 3 条 read 时不折叠。
        let mut small = history.clone();
        small.truncate(7); // 只含 2 条 read 结果
        fold_old_reads(&mut small);
        assert!(small[2].parts.iter().any(|p| matches!(p, Part::Text(t) if t.contains("旧内容"))));
    }

    #[test]
    fn fold_old_reads_never_folds_short_results() {
        // 短 read 结果（query 命中只有几行）必须原样保留：折叠会挖掉模型
        // 刚收集的信息并引导「重新 read」，实测喂出 3 节点周期 query 的
        // 死循环（烧尽 30 轮）。
        let mut history: Vec<Message> = Vec::new();
        for i in 0..6 {
            history.push(Message::assistant(&format!(r#"{{"tool":"read","args":{{"query":"q{i}"}}}}"#)));
            history.push(Message::user(format!("[工具结果 read]\n命中 1 个 cell:\ncell `n{i}` @1-2 geo=0,0 10x10")));
        }
        fold_old_reads(&mut history);
        for (k, m) in history.iter().enumerate().filter(|(i, _)| i % 2 == 1) {
            match &m.parts[0] {
                Part::Text(t) => assert!(
                    t.contains(&format!("n{}", (k - 1) / 2)),
                    "短结果不得折叠: idx{k}: {t}"
                ),
                _ => panic!("应为文本 part"),
            }
        }
    }

    #[test]
    fn estimate_tokens_uses_png_dimensions() {
        // 大图按像素面积估（不低于 900 下限）；小 png 字节仍回退 900。
        let png = minimal_png(1600, 1200);
        let est = estimate_tokens(&[Message::with_parts(
            "user",
            vec![Part::ImagePng(png)],
        )]);
        assert!(est >= 1600 * 1200 / 750, "应按面积估算: {est}");
        let small = estimate_tokens(&[Message::with_parts(
            "user",
            vec![Part::ImagePng(minimal_png(100, 80))],
        )]);
        assert_eq!(small, 900, "小图回退旧的下限估算");
    }

    /// PNG signature + IHDR 头（宽高 big-endian），足够 estimate 解析。
    fn minimal_png(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        v.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]); // bit depth etc.
        v
    }
}