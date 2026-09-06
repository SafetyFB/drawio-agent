//! The harness engine: one user ask -> model decides tool calls -> results
//! are appended -> until the model says done. Plain ReAct over a JSON
//! envelope — no bespoke pipeline, no phases. Works with any
//! OpenAI-compatible endpoint.
//!
//! Accounting (R6): every LLM call's usage is parsed and accumulated into
//! [`SessionStats`]; cost follows the configured ¥/1M prices; an optional
//! budget stops the run before spending more. A configured context length
//! guards prompt size before each call.

use serde_json::{Value, json};

use std::sync::Arc;

use crate::chat::{CallOpts, Chat, Message, Part, Usage};
use crate::config::usage_cost;
use crate::tools::{ToolOutput, Tools};
use crate::xmlfile::XmlDoc;

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
#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    pub usage: Usage,
    pub cost_yuan: f64,
}

impl SessionStats {
    pub fn add(&mut self, u: &Usage, cost: f64) {
        self.usage.add(u);
        self.cost_yuan += cost;
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
}

impl Default for RunOpts {
    fn default() -> Self {
        Self {
            no_think: false,
            context_limit: None,
            price_input_per_m: 0.0,
            price_output_per_m: 0.0,
            budget_remaining: f64::INFINITY,
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
        }
    }
}

#[derive(Debug)]
pub struct Harness {
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

/// Rough token estimate for prompt-size guarding (text chars ≈ 0.5 token
/// for CJK-heavy prompts, images billed as a flat 900 tokens).
fn estimate_tokens(msgs: &[Message]) -> u64 {
    let mut t = 0u64;
    for m in msgs {
        for p in &m.parts {
            match p {
                Part::Text(s) => t += (s.chars().count() as u64 + 1) / 2,
                Part::ImagePng(_) => t += 900,
            }
        }
    }
    t
}

impl Harness {
    /// Run one user ask to completion. `stats` accumulates this ask's
    /// usage/cost so the caller keeps session totals across asks.
    pub async fn run(
        &self,
        chat: &mut dyn Chat,
        tools: &mut Tools,
        doc: &mut XmlDoc,
        user_text: &str,
        context: &str,
        opts: &RunOpts,
        stats: &mut SessionStats,
        progress: &Option<ProgressFn>,
    ) -> Result<TurnOutcome, String> {
        macro_rules! emit {
            ($e:expr) => {
                if let Some(f) = progress {
                    f(EngineEvent::from($e));
                }
            };
        }
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
        let mut bad_tools = 0usize;
        let mut spent: f64 = 0.0;
        let mut usage = Usage::default();

        for turn in 0..self.max_turns {
            emit!(EngineEvent::Turn { index: turn });
            // Context-length guard: fail loudly instead of silently blowing
            // the configured window.
            if let Some(limit) = opts.context_limit {
                let est = estimate_tokens(&history);
                if est > limit {
                    return Err(format!(
                        "上下文估算 {est} tokens 超过配置的 context_length={limit}。\
                         请调高 ⚙ 设置里的上下文长度，或开始新会话。"
                    ));
                }
            }
            // Budget guard: stop before the next call once the run's spend
            // reaches the remaining budget.
            if opts.budget_remaining.is_finite() && spent >= opts.budget_remaining {
                stats.add(&usage, spent);
                return Err(format!(
                    "会话预算已用尽（本次已花 ¥{spent:.4} ≥ 预算 ¥{:.2}）。\
                     可在 ⚙ 设置里调高预算后继续。",
                    opts.budget_remaining
                ));
            }

            let raw = match chat
                .complete(&history, &CallOpts { no_think: opts.no_think })
                .await
            {
                Ok(reply) => {
                    usage.add(&reply.usage);
                    let cost = usage_cost(
                        reply.usage.input_tokens,
                        reply.usage.output_tokens,
                        &price_opts(opts),
                    );
                    spent += cost;
                    emit!(EngineEvent::Usage {
                        usage: reply.usage,
                        cost_yuan: cost,
                    });
                    reply.text
                }
                Err(e) => {
                    stats.add(&usage, spent);
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
                    stats.add(&usage, spent);
                    return Err(e);
                }
            };
            last_err = None;
            emit!(EngineEvent::ModelOutput { raw: raw.clone() });
            envelopes.push(raw.clone());

            if let Some(reply) = env.get("reply").and_then(Value::as_str) {
                let reply = reply.to_string();
                emit!(EngineEvent::Final { reply: reply.clone() });
                stats.add(&usage, spent);
                return Ok(TurnOutcome {
                    reply,
                    tool_calls,
                    envelopes,
                    usage,
                    cost_yuan: spent,
                });
            }

            let name = env
                .get("tool")
                .and_then(Value::as_str)
                .ok_or_else(|| "信封缺 tool/reply 字段".to_string())?;
            // Tool whitelist + fast fail: a model that hallucinates tool
            // names (e.g. `{"tool":"reply"}`) would otherwise loop until
            // max_turns.
            if !["read", "locate", "edit", "draw", "check", "view"].contains(&name) {
                bad_tools += 1;
                if bad_tools >= 2 {
                    stats.add(&usage, spent);
                    return Err(format!(
                        "模型连续输出未知工具 `{name}`，已中止。请检查系统提示中的工具协议是否被遵守。"
                    ));
                }
                history.push(Message::assistant(raw.clone()));
                history.push(Message::user(format!(
                    "`{name}` 不是可用工具。可用工具: read locate edit draw check view。\
                     每轮只输出一个 JSON 信封；完成后用 {{\"reply\": \"...\", \"done\": true}} 结束。"
                )));
                continue;
            }
            bad_tools = 0;
            let args = env.get("args").cloned().unwrap_or(json!({}));
            emit!(EngineEvent::Tool {
                name: name.to_string(),
                args: serde_json::to_string(&args).unwrap_or_default(),
            });
            history.push(Message::assistant(raw));
            let result = match tools.run(doc, name, &args).await {
                Ok(out) => out,
                Err(text) => ToolOutput::text(format!("工具执行失败: {text}")),
            };
            tool_calls += 1;
            emit!(EngineEvent::ToolResult {
                name: name.to_string(),
                text: result.text.clone(),
                has_image: result.image_png.is_some(),
            });
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
        stats.add(&usage, spent);
        Err(format!("达到最大轮数 {} 仍未完成", self.max_turns))
    }
}

fn price_opts(o: &RunOpts) -> crate::config::LlmSettings {
    crate::config::LlmSettings {
        price_input_per_m: o.price_input_per_m,
        price_output_per_m: o.price_output_per_m,
        ..Default::default()
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
    /// snapshot it was sent, with configurable usage per call.
    struct FakeChat {
        script: VecDeque<(String, Usage)>,
        snapshots: Vec<Vec<Message>>,
    }

    impl FakeChat {
        fn new(script: Vec<&str>) -> Self {
            Self {
                script: script
                    .into_iter()
                    .map(|s| (s.to_string(), Usage::default()))
                    .collect(),
                snapshots: Vec::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl Chat for FakeChat {
        async fn complete(
            &mut self,
            messages: &[Message],
            _opts: &CallOpts,
        ) -> Result<crate::chat::Reply, crate::chat::ChatError> {
            self.snapshots.push(messages.to_vec());
            let (text, usage) = self
                .script
                .pop_front()
                .ok_or(crate::chat::ChatError::Empty)?;
            Ok(crate::chat::Reply { text, usage })
        }
    }

    fn contains_image_parts(msgs: &[Message]) -> bool {
        msgs.iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, Part::ImagePng(_))))
    }

    const SAMPLE: &str = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="a" value="A" vertex="1" parent="1"><mxGeometry x="0" y="0" width="100" height="50" as="geometry"/></mxCell><mxCell id="b" value="B" vertex="1" parent="1"><mxGeometry x="200" y="0" width="100" height="50" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;

    #[tokio::test]
    async fn view_image_roundtrip_drives_edits() {
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
        let mut stats = SessionStats::default();

        let outcome = harness
            .run(&mut fake, &mut tools, &mut doc, "两个节点重叠了，看看并修复 b", "", &RunOpts::default(), &mut stats, &None)
            .await
            .unwrap();
        assert_eq!(outcome.tool_calls, 2);
        assert_eq!(outcome.reply, "改好了");
        assert_eq!(fake.snapshots.len(), 3);
        let after_view = &fake.snapshots[1];
        assert!(contains_image_parts(after_view), "view 的结果必须带图像 part");
        let user_img = after_view
            .iter()
            .find(|m| m.role == "user" && m.parts.len() == 2)
            .expect("view 工具结果消息应有 text+image 两个 part");
        match &user_img.parts[1] {
            Part::ImagePng(b) => assert_eq!(b, &png),
            _ => panic!("第二个 part 应为 PNG"),
        }
        assert!(doc.canonical().contains("B v2"));
    }

    #[tokio::test]
    async fn usage_and_cost_accumulate_and_budget_stops() {
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        let mut fake = FakeChat {
            script: VecDeque::new(),
            snapshots: Vec::new(),
        };
        fake.script.push_back((
            r#"{"reply":"好了","done":true}"#.into(),
            Usage { input_tokens: 1000, output_tokens: 2000 },
        ));
        let mut tools = Tools::new(false);
        let mut stats = SessionStats::default();
        // 1 元/百万 in, 2 元/百万 out → 1000/1e6*1 + 2000/1e6*2 = 0.001+0.004
        let opts = RunOpts {
            price_input_per_m: 1.0,
            price_output_per_m: 2.0,
            ..Default::default()
        };
        let outcome = Harness::default()
            .run(&mut fake, &mut tools, &mut doc, "hi", "", &opts, &mut stats, &None)
            .await
            .unwrap();
        assert_eq!(outcome.usage.input_tokens, 1000);
        assert_eq!(outcome.usage.output_tokens, 2000);
        assert!((outcome.cost_yuan - 0.005).abs() < 1e-9, "{}", outcome.cost_yuan);
        // engine already accumulated into stats (跨 ask 累计语义)
        assert!((stats.cost_yuan - 0.005).abs() < 1e-9);

        // Budget smaller than one call's spend: the first call runs, then the
        // engine refuses to continue (guard sits before the next call).
        let mut fake2 = FakeChat {
            script: vec![
                (r#"{"tool":"check","args":{}}"#.into(), Usage { input_tokens: 1000, output_tokens: 2000 }),
                (r#"{"reply":"x","done":true}"#.into(), Usage { input_tokens: 10, output_tokens: 5 }),
            ]
            .into_iter()
            .collect(),
            snapshots: Vec::new(),
        };
        let opts_broke = RunOpts {
            price_input_per_m: 1.0,
            price_output_per_m: 2.0,
            budget_remaining: 0.001,
            ..Default::default()
        };
        let err = Harness::default()
            .run(&mut fake2, &mut tools, &mut doc, "hi", "", &opts_broke, &mut SessionStats::default(), &None)
            .await
            .unwrap_err();
        assert!(err.contains("预算"), "{err}");
        assert_eq!(fake2.snapshots.len(), 1, "预算用尽后不应再发起调用");
    }

    #[tokio::test]
    async fn context_limit_guard_rejects_oversized_prompts() {
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        let mut fake = FakeChat::new(vec![]);
        let mut tools = Tools::new(false);
        let opts = RunOpts {
            context_limit: Some(10), // tiny: any real prompt exceeds it
            ..Default::default()
        };
        let err = Harness::default()
            .run(&mut fake, &mut tools, &mut doc, "hi", "", &opts, &mut SessionStats::default(), &None)
            .await
            .unwrap_err();
        assert!(err.contains("context_length"), "{err}");
        assert!(fake.snapshots.is_empty(), "超限时不应发起调用");
    }

    #[tokio::test]
    async fn old_screenshots_are_folded_after_a_newer_view() {
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
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
            .run(&mut fake, &mut tools, &mut doc, "检查布局", "", &RunOpts::default(), &mut SessionStats::default(), &None)
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
        assert!(final_snapshot.iter().any(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, Part::Text(t) if t.contains("已折叠")))
        }));
    }
}
