//! The harness engine: one user ask -> model decides tool calls -> results are
//! appended -> until the model says done. Plain ReAct over a JSON envelope —
//! no bespoke pipeline, no phases. Works with any OpenAI-compatible endpoint.

use serde_json::{Value, json};

use crate::chat::{Chat, Message};
use crate::tools::Tools;
use crate::xmlfile::XmlDoc;

#[derive(Debug)]
pub struct TurnOutcome {
    /// The model's final reply to the user.
    pub reply: String,
    /// How many tool calls ran before done.
    pub tool_calls: usize,
    /// Raw assistant JSON envelopes (for debugging / trajectory).
    pub envelopes: Vec<String>,
}

#[derive(Debug)]
pub struct Harness {
    /// Max model turns (tool call + result) per user ask.
    pub max_turns: usize,
}

// Transcript is kept in memory for the duration of one ask; a new ask
// starts fresh (system + user + history rollup can come later — the xml
// file on disk is already the durable state).

impl Default for Harness {
    fn default() -> Self {
        Self { max_turns: 10 }
    }
}

fn system_prompt(doc: &XmlDoc) -> String {
    format!(
        r#"你是 drawio 图表的编辑 Agent。当前图表的唯一工件是文件 {path}，
已用规范格式保存（每个元素一行、属性实体已规范转义、行号稳定）。
当前文档共 {cells} 个带 id 的元素（含容器），最新渲染图为 {png}。

协作规则：
- 用户的每一轮消息都可能带「选中区段」（行号标注的 xml 片段），那表示
  用户只关心这些 cell —— 你的改动必须局限在它们所在的行区间内。
- 改文件前先用 read / locate 确认准确行号；xml 语法错误、id 重复、
  引用断掉的 edit 会被系统拒绝并回传错误。
- 涉及空间位置/连线的问题，先 view 渲染看图，再对照 locate 找 cell。
- 你的输出必须是**单个 JSON 信封**，不要输出其他文字（不要 markdown 代码块）：

工具协议（每轮只选一个）：
{specs}

信封例子：
{{"tool": "locate", "args": {{"query": "订单"}}}}
{{"tool": "edit", "args": {{"range": "cell:svc-a", "text": "<mxCell ...>…</mxCell>"}}}}
{{"reply": "已完成：把订单服务节点改为蓝色。", "done": true}}

注意 text 字段里的换行与引号要按 JSON 规则转义；range 优先用 cell:id，
行号会随编辑漂移，cell:id 不会。"#,
        path = doc.path.display(),
        cells = doc.cells.len(),
        png = doc.path.with_extension("png").display(),
        specs = Tools::tool_specs(),
    )
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
    // Tolerant scan: first '{' .. balanced last '}'.
    let start = trimmed.find('{').ok_or_else(|| format!("回复里没有 JSON 信封: {raw}"))?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in trimmed[start..].char_indices() {
        match c {
            '"' if !esc => in_str = !in_str,
            '\\' if in_str => esc = !esc,
            _ => esc = false,
        }
        if in_str {
            continue;
        }
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + i + 1;
                    let candidate = &trimmed[start..end];
                    return serde_json::from_str(candidate)
                        .map_err(|e| format!("JSON 解析失败: {e} in {candidate}"));
                }
            }
            _ => {}
        }
    }
    Err(format!("JSON 信封不完整: {raw}"))
}

impl Harness {
    /// Run one user ask to completion. `chat` is the live LLM; `tools` may
    /// be freshly created per ask (renderer is cached inside).
    pub async fn run(
        &self,
        chat: &mut dyn Chat,
        tools: &mut Tools,
        doc: &mut XmlDoc,
        user_text: &str,
        context: &str,
    ) -> Result<TurnOutcome, String> {
        let mut history: Vec<Message> = vec![Message::system(system_prompt(doc))];
        let full = if context.trim().is_empty() {
            user_text.to_string()
        } else {
            format!("{user_text}\n{context}")
        };
        history.push(Message::user(full));

        let mut envelopes = Vec::new();
        let mut tool_calls = 0usize;
        let mut last_err: Option<String> = None;

        for turn in 0..self.max_turns {
            let raw = match chat.complete(&history).await {
                Ok(r) => r,
                Err(e) => {
                    return Err(format!("LLM 调用失败（第 {turn} 轮）: {e}"));
                }
            };
            let env = match parse_envelope(&raw) {
                Ok(v) => v,
                Err(e) => {
                    // One retry with an explicit correction message, then give up.
                    if last_err.is_none() {
                        last_err = Some(e.clone());
                        history.push(Message::assistant(raw.clone()));
                        history.push(Message::user(format!(
                            "你的上一条输出不是合法信封: {e}\n请只输出一个 JSON 信封（不要代码块、不要多余文字）。"
                        )));
                        continue;
                    }
                    return Err(e);
                }
            };
            last_err = None;
            if let Some(reply) = env.get("reply").and_then(Value::as_str) {
                envelopes.push(raw);
                return Ok(TurnOutcome {
                    reply: reply.to_string(),
                    tool_calls,
                    envelopes,
                });
            }

            let name = env
                .get("tool")
                .and_then(Value::as_str)
                .ok_or_else(|| "信封缺 tool/reply 字段".to_string())?;
            let args = env.get("args").cloned().unwrap_or(json!({}));
            history.push(Message::assistant(raw));
            let result = match tools.run(doc, name, &args).await {
                Ok(text) => text,
                Err(text) => format!("工具执行失败: {text}"),
            };
            tool_calls += 1;
            history.push(Message::user(format!("[工具结果 {name}]\n{result}")));
        }
        Err(format!("达到最大轮数 {} 仍未完成", self.max_turns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_parses_plain_and_fenced() {
        let v = parse_envelope(r#"{"tool":"locate","args":{"query":"a"}}"#).unwrap();
        assert_eq!(v["tool"], "locate");
        let v = parse_envelope("```json\n{\"reply\":\"好\",\"done\":true}\n```").unwrap();
        assert_eq!(v["reply"], "好");
    }

    #[test]
    fn envelope_parses_with_surrounding_text() {
        let v = parse_envelope("好的，我来查：{\"tool\":\"locate\",\"args\":{\"query\":\"订单\"}} 请稍等").unwrap();
        assert_eq!(v["tool"], "locate");
    }

    #[test]
    fn envelope_rejects_garbage() {
        assert!(parse_envelope("抱歉我不知道").is_err());
        assert!(parse_envelope("{\"tool\": \"edit\"").is_err());
    }
}
