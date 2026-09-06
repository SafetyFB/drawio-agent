//! The harness engine: one user ask -> model decides tool calls -> results are
//! appended -> until the model says done. Plain ReAct over a JSON envelope —
//! no bespoke pipeline, no phases. Works with any OpenAI-compatible endpoint.

use serde_json::{Value, json};

use crate::chat::{Chat, Message, Part};
use crate::tools::{ToolOutput, Tools};
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
                Ok(out) => out,
                Err(text) => ToolOutput::text(format!("工具执行失败: {text}")),
            };
            tool_calls += 1;
            let mut parts = vec![Part::Text(format!("[工具结果 {name}]\n{}", result.text))];
            if let Some(png) = result.image_png {
                parts.push(Part::ImagePng(png));
            }
            history.push(Message::with_parts("user", parts));
            // Keep vision tokens bounded: once a newer screenshot arrives,
            // older ones in the transcript become "folded" text (the model
            // already acted on them; re-view if needed).
            fold_old_images(&mut history);
        }
        Err(format!("达到最大轮数 {} 仍未完成", self.max_turns))
    }
}

/// Replace image parts in every message except the last one with a note,
/// keeping only the most recent screenshot in the transcript.
fn fold_old_images(history: &mut [Message]) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmlfile::XmlDoc;
    use std::collections::VecDeque;

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

    /// Scripted fake: returns envelopes in order, records every transcript
    /// snapshot it was sent.
    struct FakeChat {
        script: VecDeque<String>,
        snapshots: Vec<Vec<Message>>,
    }

    impl FakeChat {
        fn new(script: Vec<&str>) -> Self {
            Self {
                script: script.into_iter().map(String::from).collect(),
                snapshots: Vec::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl Chat for FakeChat {
        async fn complete(&mut self, messages: &[Message]) -> Result<String, crate::chat::ChatError> {
            self.snapshots.push(messages.to_vec());
            self.script
                .pop_front()
                .ok_or(crate::chat::ChatError::Empty)
        }
    }

    fn contains_image_parts(msgs: &[Message]) -> bool {
        msgs.iter().any(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, Part::ImagePng(_)))
        })
    }

    const SAMPLE: &str = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="a" value="A" vertex="1" parent="1"><mxGeometry x="0" y="0" width="100" height="50" as="geometry"/></mxCell><mxCell id="b" value="B" vertex="1" parent="1"><mxGeometry x="200" y="0" width="100" height="50" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;

    #[tokio::test]
    async fn view_image_roundtrip_drives_edits() {
        // Scripted model: 1) view -> receives a screenshot back as an image
        // part, 2) edit cell `b`, 3) done.
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        let replacement = r#"<mxCell id="b" value="B v2" vertex="1" parent="1"><mxGeometry x="220" y="0" width="120" height="50" as="geometry"/></mxCell>"#;
        let edit_env = json!({
            "tool": "edit",
            "args": { "range": "cell:b", "text": replacement }
        })
        .to_string();
        let mut fake = FakeChat::new(vec![
            r#"{"tool":"view","args":{}}"#,
            &edit_env,
            r#"{"reply":"改好了","done":true}"#,
        ]);
        let png = vec![0x89, b'P', b'N', b'G'];
        let mock = drawio_agent_renderer::MockDriver::new().with_bytes(png.clone());
        let renderer = drawio_agent_renderer::Renderer::new(std::sync::Arc::new(mock));
        let mut tools = Tools::with_renderer(renderer);
        let harness = Harness::default();

        let outcome = harness
            .run(&mut fake, &mut tools, &mut doc, "两个节点重叠了，看看并修复 b", "")
            .await
            .unwrap();
        assert_eq!(outcome.tool_calls, 2);
        assert_eq!(outcome.reply, "改好了");
        // the transcript handed to the *second* model call must contain the
        // screenshot as an image part (tool result of view)
        assert_eq!(fake.snapshots.len(), 3);
        let after_view = &fake.snapshots[1];
        assert!(contains_image_parts(after_view), "view 的结果必须带图像 part");
        // image part is the second part of the tool-result user message
        let user_img = after_view
            .iter()
            .find(|m| m.role == "user" && m.parts.len() == 2)
            .expect("view 工具结果消息应有 text+image 两个 part");
        match &user_img.parts[1] {
            Part::ImagePng(b) => assert_eq!(b, &png),
            _ => panic!("第二个 part 应为 PNG"),
        }
        // and the edit really landed
        assert!(doc.canonical().contains("B v2"));
        assert!(doc.id_to_cell("b").is_some());
    }

    #[tokio::test]
    async fn old_screenshots_are_folded_after_a_newer_view() {
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        // view -> edit b -> view -> done: the first screenshot must be folded
        // once the second arrives, so at most one image is ever in context.
        let mut fake = FakeChat::new(vec![
            r#"{"tool":"view","args":{}}"#,
            r#"{"tool":"edit","args":{"range":"cell:b","text":"<mxCell id=\"b\" value=\"B2\" parent=\"1\"/>"}}"#,
            r#"{"tool":"view","args":{}}"#,
            r#"{"reply":"done","done":true}"#,
        ]);
        let mock = drawio_agent_renderer::MockDriver::new()
            .with_bytes(vec![0x89, b'P', b'N', b'G', 0x0d]);
        let renderer = drawio_agent_renderer::Renderer::new(std::sync::Arc::new(mock));
        let mut tools = Tools::with_renderer(renderer);
        let outcome = Harness::default()
            .run(&mut fake, &mut tools, &mut doc, "检查布局", "")
            .await
            .unwrap();
        assert_eq!(outcome.tool_calls, 3);
        let final_snapshot = fake.snapshots.last().unwrap();
        let images = final_snapshot
            .iter()
            .flat_map(|m| &m.parts)
            .filter(|p| matches!(p, Part::ImagePng(_)))
            .count();
        assert_eq!(images, 1, "上下文中最多保留最近一张截图");
        // the folded one became an explicit note
        assert!(final_snapshot.iter().any(|m| {
            m.parts.iter().any(|p| matches!(p, Part::Text(t) if t.contains("已折叠")))
        }));
    }
}