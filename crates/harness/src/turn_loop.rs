//! Turn loop orchestration: the main engine loop that drives tool calls.

use crate::chat::{CallOpts, Chat, Message, Part, Usage};
use crate::config::usage_cost;
use crate::engine::{EngineEvent, Harness, ProgressFn, RunOpts, SessionStats, TurnOutcome};
use crate::guards::{check_budget, check_context_limit, price_opts};
use crate::memory::{fold_old_images, fold_old_reads};
use crate::tools::{ToolOutput, Tools};
use crate::xmlfile::XmlDoc;

/// Extension trait to keep the run method in turn_loop while Harness is defined in engine.rs
#[allow(async_fn_in_trait)]
pub trait HarnessRunExt {
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        chat: &mut dyn Chat,
        tools: &mut Tools,
        doc: &mut XmlDoc,
        user_text: &str,
        context: &str,
        opts: &RunOpts,
        stats: &mut SessionStats,
        progress: &Option<ProgressFn>,
    ) -> Result<TurnOutcome, String>;
}

impl HarnessRunExt for Harness {
    #[allow(clippy::too_many_arguments)]
    async fn run(
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

        let mut history: Vec<Message> = vec![Message::system(crate::prompt::build_system_prompt(doc, opts.legacy_viewer))];
        // R5: re-inject the rolling memory of earlier asks (kept trimmed).
        for m in &stats.transcript {
            history.push(m.clone());
        }
        let ask_start = history.len(); // where this ask's own messages begin
        let full = if context.trim().is_empty() {
            user_text.to_string()
        } else {
            format!("{user_text}\n{context}")
        };
        history.push(Message::user(full));

        // Commit this ask's tail into memory on every exit path.
        macro_rules! remember {
            () => {
                let tail: Vec<Message> = history.split_off(ask_start);
                let cap = opts
                    .context_limit
                    .map(|l| (l as f64 * 0.7) as u64)
                    .unwrap_or(crate::memory::DEFAULT_MEMORY_TOKENS as u64);
                stats.transcript.extend(tail);
                crate::memory::trim_memory(stats, cap);
            };
        }

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
            if let Some(err) = check_context_limit(&history, opts) {
                stats.add(&usage, spent);
                remember!();
                return Err(err);
            }

            // Budget guard: stop before the next call once the run's spend
            // reaches the remaining budget.
            if let Some(err) = check_budget(spent, opts) {
                stats.add(&usage, spent);
                remember!();
                return Err(err);
            }

            let call_opts = CallOpts {
                no_think: opts.no_think,
                temperature: opts.temperature,
            };
            let mut reply = None;
            let mut last_llm_err = None;
            for attempt in 0..=self.max_llm_retries {
                match chat.complete(&history, &call_opts).await {
                    Ok(r) => {
                        reply = Some(r);
                        break;
                    }
                    Err(e) if attempt < self.max_llm_retries => {
                        // Providers 500 occasionally (large multimodal
                        // contexts); retry before giving up on the whole ask.
                        last_llm_err = Some(format!(
                            "第 {turn} 轮 LLM 调用失败（重试 {}/{}）: {e}",
                            attempt + 1,
                            self.max_llm_retries
                        ));
                        if let Some(m) = &last_llm_err {
                            eprintln!("{m}");
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                    }
                    Err(e) => {
                        last_llm_err = Some(format!("第 {turn} 轮 LLM 调用失败: {e}"));
                        break;
                    }
                }
            }
            let reply = match reply {
                Some(r) => r,
                None => {
                    stats.add(&usage, spent);
                    remember!();
                    let done = if tool_calls > 0 {
                        format!(
                            "；注意：此前 {tool_calls} 次工具改动已应用并保存到文件，可继续对话或 /history 回看"
                        )
                    } else {
                        String::new()
                    };
                    return Err(format!(
                        "{}（已重试 {} 次）{done}",
                        last_llm_err.unwrap_or_default(),
                        self.max_llm_retries
                    ));
                }
            };
            let crate::chat::Reply { text: raw, usage: ru } = reply;
            usage.add(&ru);

            let cost = usage_cost(ru.input_tokens, ru.output_tokens, &price_opts(opts));
            spent += cost;
            emit!(EngineEvent::Usage {
                usage: ru,
                cost_yuan: cost,
            });
            let env = match crate::envelope::parse_envelope(&raw) {
                Ok(v) => v,
                Err(e) => {
                    // Loose fallback: before any tool ran, plain-language
                    // answers (e.g. "6") are accepted as the reply. Once the
                    // model has started calling tools we require protocol
                    // compliance (one correction retry, then give up).
                    if tool_calls == 0 {
                        let mut reply = crate::envelope::strip_fences(&raw).trim().to_string();
                        if crate::envelope::looks_like_action(user_text) {
                            reply = format!(
                                "{reply}\n\n（系统提示：本轮模型未调用任何工具，文件没有被修改。如果这与你的要求不符，请重新表述并强调要用工具修改。）"
                            );
                        }
                        emit!(EngineEvent::Final { reply: reply.clone() });
                        stats.add(&usage, spent);
                        remember!();
                        return Ok(TurnOutcome {
                            reply,
                            tool_calls: 0,
                            envelopes: vec![raw.clone()],
                            usage,
                            cost_yuan: spent,
                        });
                    }
                    if last_err.is_none() {
                        last_err = Some(e.clone());
                        history.push(Message::assistant(raw.clone()));
                        history.push(Message::user(format!(
                            "你的上一条输出不是合法信封: {e}\n请只输出一个 JSON 信封，例如 {{\"tool\": \"read\", \"args\": {{\"query\": \"x\"}}}} 或 {{\"reply\": \"…\", \"done\": true}}；不要 markdown 代码块、不要附带其他文字。"
                        )));
                        continue;
                    }
                    stats.add(&usage, spent);
                    remember!();
                    return Err(e);
                }
            };
            last_err = None;
            emit!(EngineEvent::ModelOutput { raw: raw.clone() });
            envelopes.push(raw.clone());

            if let Some(reply) = env.get("reply").and_then(Value::as_str) {
                let reply = reply.to_string();
                history.push(Message::assistant(reply.clone()));
                emit!(EngineEvent::Final { reply: reply.clone() });
                stats.add(&usage, spent);
                remember!();
                return Ok(TurnOutcome {
                    reply,
                    tool_calls,
                    envelopes,
                    usage,
                    cost_yuan: spent,
                });
            }

            let name = match env.get("tool").and_then(Value::as_str) {
                Some(n) => n,
                None if tool_calls == 0 => {
                    // JSON envelope without tool/reply before any tool ran:
                    // treat the model's raw text as the reply. If the user's
                    // ask sounded like an action request, warn that nothing
                    // was actually changed.
                    let mut reply = crate::envelope::strip_fences(&raw).trim().to_string();
                    if crate::envelope::looks_like_action(user_text) {
                        reply = format!(
                            "{reply}\n\n（系统提示：本轮模型未调用任何工具，文件没有被修改。如果这与你的要求不符，请重新表述并强调要用工具修改。）"
                        );
                    }
                    emit!(EngineEvent::Final { reply: reply.clone() });
                    stats.add(&usage, spent);
                    remember!();
                    return Ok(TurnOutcome {
                        reply,
                        tool_calls: 0,
                        envelopes: vec![raw.clone()],
                        usage,
                        cost_yuan: spent,
                    });
                }
                None => {
                    // Started calling tools already — require the protocol:
                    // one correction round, then give up.
                    if last_err.is_none() {
                        last_err = Some("信封缺 tool/reply 字段".to_string());
                        history.push(Message::assistant(raw.clone()));
                        history.push(Message::user(
                            "信封缺少 tool 或 reply 字段。请只输出一个 JSON 信封：\
                             {{\"tool\": \"<工具名>\", \"args\": {{…}}}} 或 {{\"reply\": \"…\", \"done\": true}}。"
                                .to_string(),
                        ));
                        continue;
                    }
                    stats.add(&usage, spent);
                    remember!();
                    return Err("信封缺 tool/reply 字段".to_string());
                }
            };
            // Tool whitelist + fast fail: a model that hallucinates tool
            // names (e.g. `{"tool":"reply"}`) would otherwise loop until
            // max_turns.
            if !crate::tools::TOOL_NAMES.contains(&name) {
                bad_tools += 1;
                if bad_tools >= 2 {
                    stats.add(&usage, spent);
                    remember!();
                    return Err(format!(
                        "模型连续输出未知工具 `{name}`，已中止。请检查系统提示中的工具协议是否被遵守。"
                    ));
                }
                history.push(Message::assistant(raw.clone()));
                history.push(Message::user(format!(
                    "`{name}` 不是可用工具。可用工具: {}。\
                     每轮只输出一个 JSON 信封；完成后用 {{\"reply\": \"...\", \"done\": true}} 结束。",
                    crate::tools::TOOL_NAMES.join(" ")
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
            // Same for the biggest text payloads: old `read` results are
            // folded to a stub once newer ones arrive — line numbers drift
            // after every edit, so stale reads must be re-read anyway.
            fold_old_reads(&mut history);
        }
        stats.add(&usage, spent);
        remember!();
        Err(format!(
            "达到最大轮数 {} 仍未完成（可在设置面板调大「最大轮数」，或把任务拆小分步完成）",
            self.max_turns
        ))
    }
}

use serde_json::{Value, json};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Harness, RunOpts, SessionStats, TurnOutcome};
    use crate::xmlfile::XmlDoc;
    use std::collections::VecDeque;

    const SAMPLE: &str = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="a" value="A" vertex="1" parent="1"><mxGeometry x="0" y="0" width="100" height="50" as="geometry"/></mxCell><mxCell id="b" value="B" vertex="1" parent="1"><mxGeometry x="200" y="0" width="100" height="50" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;

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

    #[tokio::test]
    async fn correction_message_example_uses_real_tool() {
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        let mut tools = Tools::new(false);
        // 触发纠错：先一次合法工具，再一条坏输出 → 纠错消息进入上下文。
        let mut fake = FakeChat::new(vec![
            r#"{"tool":"check","args":{}}"#,
            "这不是信封",
            r#"{"reply":"好","done":true}"#,
        ]);
        let _ = Harness::default()
            .run(&mut fake, &mut tools, &mut doc, "检查图", "", &RunOpts::default(), &mut SessionStats::default(), &None)
            .await;
        let sent: Vec<String> = fake
            .snapshots
            .iter()
            .flat_map(|s| s.iter().map(|m| match &m.parts[0] {
                Part::Text(t) => t.clone(),
                _ => String::new(),
            }))
            .collect();
        let correction = sent
            .iter()
            .find(|t| t.contains("不是合法信封"))
            .expect("纠错消息应进入上下文");
        assert!(
            correction.contains(r#"{"tool": "read""#),
            "纠错示例必须用真实工具: {correction}"
        );
        assert!(!correction.contains("locate"), "纠错示例不得用 locate");
    }

    #[tokio::test]
    async fn plain_answer_to_action_ask_warns_nothing_changed() {
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        let mut tools = Tools::new(false);
        let mut fake = FakeChat::new(vec![r#"好的，已经帮你改好了颜色。"#]);
        let out = Harness::default()
            .run(&mut fake, &mut tools, &mut doc, "把节点 b 的颜色改成蓝色", "", &RunOpts::default(), &mut SessionStats::default(), &None)
            .await
            .unwrap();
        assert_eq!(out.tool_calls, 0);
        assert!(out.reply.contains("文件没有被修改"), "{}", out.reply);
        // 纯问答不加警告
        let mut fake2 = FakeChat::new(vec![r#"图里有 5 个节点。"#]);
        let out2 = Harness::default()
            .run(&mut fake2, &mut tools, &mut doc, "图里有几个节点？", "", &RunOpts::default(), &mut SessionStats::default(), &None)
            .await
            .unwrap();
        assert!(!out2.reply.contains("文件没有被修改"), "{}", out2.reply);
    }

    #[tokio::test]
    async fn envelope_missing_fields_gets_one_correction_round() {
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        let mut tools = Tools::new(false);
        // round 1: valid tool; round 2: envelope without tool/reply;
        // round 3 (after correction): proper reply. Should succeed.
        let mut fake = FakeChat::new(vec![
            r#"{"tool":"check","args":{}}"#,
            r#"{"note":"我还需要看一下"}"#,
            r#"{"reply":"完成","done":true}"#,
        ]);
        let out = Harness::default()
            .run(&mut fake, &mut tools, &mut doc, "检查一下图", "", &RunOpts::default(), &mut SessionStats::default(), &None)
            .await
            .unwrap();
        assert_eq!(out.reply, "完成");
        assert_eq!(out.tool_calls, 1);
    }

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

    /// Fails the first N calls with a provider-style 500, then succeeds.
    struct FlakyChat {
        failures_left: u32,
        calls: u32,
    }

    #[async_trait::async_trait]
    impl Chat for FlakyChat {
        async fn complete(
            &mut self,
            _m: &[Message],
            _o: &CallOpts,
        ) -> Result<crate::chat::Reply, crate::chat::ChatError> {
            self.calls += 1;
            if self.failures_left > 0 {
                self.failures_left -= 1;
                Err(crate::chat::ChatError::Api("HTTP 500 Internal Server Error".into()))
            } else {
                Ok(crate::chat::Reply {
                    text: r#"{"reply":"ok","done":true}"#.into(),
                    usage: Usage::default(),
                })
            }
        }
    }

    #[tokio::test]
    async fn transient_llm_500_is_retried_and_ask_survives() {
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        let mut tools = Tools::new(false);
        let mut chat = FlakyChat { failures_left: 1, calls: 0 };
        let outcome = Harness::default()
            .run(&mut chat, &mut tools, &mut doc, "hi", "", &RunOpts::default(), &mut SessionStats::default(), &None)
            .await
            .unwrap();
        assert_eq!(outcome.reply, "ok");
        assert_eq!(chat.calls, 2, "一次失败 + 一次成功");
    }

    #[tokio::test]
    async fn llm_retries_exhausted_reports_and_keeps_partial_work() {
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        let mut tools = Tools::new(false);
        let mut chat = FlakyChat { failures_left: 99, calls: 0 };
        let h = Harness { max_llm_retries: 1, ..Default::default() };
        let err = h
            .run(&mut chat, &mut tools, &mut doc, "hi", "", &RunOpts::default(), &mut SessionStats::default(), &None)
            .await
            .unwrap_err();
        assert!(err.contains("已重试 1 次"), "{err}");
        assert_eq!(chat.calls, 2, "初始调用 + 1 次重试后放弃");
    }

    #[tokio::test]
    async fn memory_reinjects_prior_asks_across_runs() {
        let mut doc = XmlDoc::from_text(SAMPLE).unwrap();
        let mut tools = Tools::new(false);
        let mut stats = SessionStats::default();
        // ask 1: user says X; fake replies done without tools.
        let mut fake = FakeChat::new(vec![r#"{"reply":"收到","done":true}"#]);
        Harness::default()
            .run(&mut fake, &mut tools, &mut doc, "记住：目标是蓝色主题", "", &RunOpts::default(), &mut stats, &None)
            .await
            .unwrap();
        assert_eq!(stats.transcript.len(), 2, "user ask + assistant reply");
        // ask 2: the transcript must be visible to the model again.
        let mut fake2 = FakeChat::new(vec![r#"{"reply":"好","done":true}"#]);
        Harness::default()
            .run(&mut fake2, &mut tools, &mut doc, "继续", "", &RunOpts::default(), &mut stats, &None)
            .await
            .unwrap();
        let first_snapshot = &fake2.snapshots[0];
        let texts: Vec<&str> = first_snapshot
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| match &m.parts[0] {
                Part::Text(t) => t.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("蓝色主题")),
            "前一轮的 user 消息应被重新注入: {texts:?}"
        );
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