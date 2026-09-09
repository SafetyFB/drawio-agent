//! Envelope parsing: extract a single JSON tool call or reply from model output.

use serde_json::Value;

/// Action-ish verbs in the user's ask — when the model answers in plain text
/// without touching any tool, a reply to one of these probably means the
/// request was NOT carried out.
pub fn looks_like_action(s: &str) -> bool {
    [
        "改", "画", "加", "添加", "删", "连", "移", "调整", "新建", "创建", "修复",
        "设计", "生成", "布局", "优化", "重构", "换", "设置", "移动", "连线", "改色",
        "改名", "增", "去掉", "放大", "缩小", "对齐", "重新排列", "拖",
    ]
    .iter()
    .any(|k| s.contains(k))
}

/// Strip ```json fences if the model wrapped its answer in a code block.
pub fn strip_fences(raw: &str) -> String {
    let t = raw.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t)
        .trim();
    t.strip_suffix("```").unwrap_or(t).trim().to_string()
}

/// Parse a model reply into either a tool call or a final reply.
pub fn parse_envelope(raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim();
    let trimmed = trimmed
        .strip_suffix("```")
        .unwrap_or(trimmed)
        .trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Ok(v);
    }
    // Tolerant scan: walk every balanced '{…}' segment. Models sometimes
    // emit a reasoning JSON first and the real envelope second (or emit two
    // tool envelopes back to back) — prefer the first candidate that looks
    // like an envelope (has `tool`/`reply`), else take the first object.
    let mut candidates: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    let mut seg_start: Option<usize> = None;
    for (i, c) in trimmed.char_indices() {
        match c {
            '"' if !esc => in_str = !in_str,
            '\\' if in_str => esc = !esc,
            _ => esc = false,
        }
        if in_str {
            continue;
        }
        match c {
            '{' => {
                depth += 1;
                if seg_start.is_none() {
                    seg_start = Some(i);
                }
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(st) = seg_start.take() {
                        candidates.push(trimmed[st..i + 1].to_string());
                    }
                }
            }
            _ => {}
        }
    }
    if candidates.is_empty() {
        return Err(format!("JSON 信封不完整: {raw}"));
    }
    let envelope_like = |c: &str| c.contains("\"tool\"") || c.contains("\"reply\"");
    let pick = candidates
        .iter()
        .find(|c| envelope_like(c))
        .or_else(|| candidates.first())
        .expect("non-empty");
    serde_json::from_str(pick).map_err(|e| format!("JSON 解析失败: {e} in {pick}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_prefers_envelope_over_reasoning_json() {
        // reasoning JSON first, real envelope second
        let raw = r#"{"reasoning":"先看一下"}{"tool":"read","args":{"query":"a"}}"#;
        let v = parse_envelope(raw).unwrap();
        assert_eq!(v["tool"], "read");
        // two envelopes back to back -> first envelope wins
        let raw2 = r#"{"tool":"view","args":{}}{"tool":"read","args":{"query":"a"}}"#;
        let v2 = parse_envelope(raw2).unwrap();
        assert_eq!(v2["tool"], "view");
        // surrounding prose + single envelope
        let raw3 = "让我看看：{\"tool\":\"check\",\"args\":{}} 完毕";
        let v3 = parse_envelope(raw3).unwrap();
        assert_eq!(v3["tool"], "check");
    }

    #[test]
    fn looks_like_action_detects_request_verbs() {
        assert!(looks_like_action("把 svc-b 的颜色改成蓝色"));
        assert!(looks_like_action("加一个节点"));
        assert!(!looks_like_action("现在图里有几个节点？"));
        assert!(!looks_like_action("总结一下刚才做了什么"));
    }

    #[test]
    fn envelope_parses_plain_and_fenced() {
        let v = parse_envelope(r#"{"tool":"read","args":{"query":"a"}}"#).unwrap();
        assert_eq!(v["tool"], "read");
        let v = parse_envelope("```json\n{\"reply\":\"好\",\"done\":true}\n```").unwrap();
        assert_eq!(v["reply"], "好");
    }

    #[test]
    fn envelope_parses_with_surrounding_text() {
        let v = parse_envelope("好的，我来查：{\"tool\":\"read\",\"args\":{\"query\":\"订单\"}} 请稍等").unwrap();
        assert_eq!(v["tool"], "read");
    }

    #[test]
    fn envelope_rejects_garbage() {
        assert!(parse_envelope("抱歉我不知道").is_err());
        assert!(parse_envelope("{\"tool\": \"edit\"").is_err());
    }
}