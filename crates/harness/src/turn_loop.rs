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
                             {{\"tool\": \"<工具名>\", \"args\": {{…}}}} 或 {{\"reply\": \"…\", \"done\": true}}."
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