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

/// Memory cap when no context_length is configured (tokens, estimate).
const DEFAULT_MEMORY_TOKENS: usize = 24_000;

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
                // unknown-tool correction: "`{name}` 不是可用工具…"
                || t.starts_with('`')
        }
        _ => true, // image-only user message = view result = synthetic
    }
}

/// Trim `stats.transcript` to fit the memory cap: drop oldest messages
/// until the estimate fits — but only in whole-ask groups, so the surviving
/// transcript always starts at a real user ask (no orphaned tool results).
fn trim_memory(stats: &mut SessionStats, cap_tokens: u64) {
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

fn system_prompt(doc: &XmlDoc, legacy_viewer: bool) -> String {
    let legacy_note = if legacy_viewer {
        r#"

## 当前画布兼容性（旧版回退模式）
本次会话未加载最新 drawio 编辑器，画布/渲染为 2018 版 mxGraph：
形状必须写 `shape=<名字>`；禁止裸形状名（如 `ellipse;…`）或
`shape=mxgraph.basic.ellipse`——这两种都会渲染成矩形（系统会自动改写，
但自己写对更稳）。"#
    } else {
        ""
    };
    format!(
        r#"你是 drawio 图表的编辑 Agent，唯一工件是本地文件 {path}
（规范 XML：每元素一行、行号稳定、属性已规范转义；共 {cells} 个带 id 元素）。
需要看图时调用 view（最新渲染 {png}），截图会作为图像消息直接发给你。

## 输出协议（每轮必须遵守）
每轮**只输出一个 JSON 信封**，除此之外不要输出任何文字、解释、markdown
代码块。一条消息里出现多个 JSON 时系统只取第一个，其余全部丢弃。

- 调用工具：{{"tool": "<工具名>", "args": {{…}}}}
- 结束：{{"reply": "<给用户的话>", "done": true}}

JSON 必须合法：字符串里的换行写成 \n、双引号写成 \"。
工具名只能从下方清单里选，不要发明新工具。

## 核心规则
1. 文件是唯一真相：动手前用 read（range 或 query）确认当前内容与准确
   行区间，不要凭记忆猜行号。行号会随编辑漂移，优先用 cell:id。
   读之前先想清楚要什么：能定位到 cell 就用 cell:id 一次读够，不要
   反复零碎 read（每次 read 都进上下文，浪费 token）。
2. 用户消息可能附有「选中区段」（带行号的 xml 片段）——改动必须局限在
   对应 cell 的行区间内，不要动范围外的内容。
3. edit 会被全量校验：XML 非法、id 重复、引用断掉会被拒绝（文件保持
   原样）；系统回报 added/changed/removed 清单，出现「范围外改动」警告
   说明你动了不该动的内容，要立刻修正。
4. **动作最大化**：每轮只做一个动作，但动作要尽可能大——需要改动多个
   cell 时用 edit 的批量 ranges 一次提交；画新图尽量一次 draw 整图
   （含全部节点与连线，一次性规划好坐标）；小改动不要拆成多轮逐个做。
   整个任务的轮数取决于你的动作粒度。
5. 涉及布局/位置/连线/样式的修改：先 view 看图再动手；关键修改后可以
   再 view 核对一次，确认没有引入重叠、溢出或断线。每次 view 前先问
   自己"要看什么"——同一文件状态最多 view 3 次，看懂即止，不要反复
   刷图（截图占上下文）。
6. 只有纯信息类问答（用户明确要求"直接回答/不要用工具"）可以直接
   回复；任何涉及画图、修改、检查的请求都必须通过工具完成。
7. edit/draw 之后建议 check 一次（结构校验 + 布局 lint 摘要）。若报
   结构错误（引用断裂等）必须修；布局警告（重叠/交叉）修最明显的
   1-2 处即可——残余的轻微交叉/边缘重叠在说明里提一句就好，
   **不要陷入逐条清零的循环**（反复小修浪费大量轮次，收益极低）。

## Draw.io 样式知识（与 drawio 编辑器互通）
- 常用形状：椭圆 shape=ellipse（width=height 即正圆；aspect=fixed 保持比例）；
  菱形 shape=rhombus；三角形 shape=triangle；六边形 shape=hexagon；
  圆柱 shape=cylinder；云 shape=cloud；泳道 shape=swimlane；
  数据库 shape=datastore；文档 shape=document；平行四边形 shape=parallelogram；
  梯形 shape=trapezoid。裸形状名（如 `ellipse`）与 `shape=mxgraph.basic.ellipse`
  同样有效，写法任选其一即可。
常用样式键（style 属性内分号分隔）：
  fillColor=#RRGGBB | strokeColor=#RRGGBB | strokeWidth=n | dashed=1
  fontSize=n | fontColor=#RRGGBB | align=left|center|right |
  verticalAlign=top|middle|bottom | whiteSpace=wrap | html=1 |
  labelPosition=center | spacing=n | opacity=n | rounded=1 | arcSize=n
连线（edge="1" 的 mxCell）：
  source/target=cell id；startArrow/endArrow=none|classic|block|oval|diamond|open
  edgeStyle=orthogonalEdgeStyle（正交走线）；curved=1；dashed=1
  exitX/exitY/entryX/entryY 为 0..1 的锚点比例；折线用
  <Array as="points"><mxPoint x=.. y=../>…</Array> 放 mxGeometry 内
几何：<mxGeometry x= y= width= height= as="geometry"/>；相对定位用
relative="1"。坐标是绝对画布坐标，摆位时注意间距避免重叠（可先 view）。

## 工具
{specs}

## 错误处理
- 工具失败时读返回的错误信息，修正参数重试；不要原样重复失败调用。
- 预算或上下文超限会被系统强制中止，不要尝试绕过。
{legacy_note}"#,
        path = doc.path.display(),
        legacy_note = legacy_note,
        cells = doc.cells.len(),
        png = doc.path.with_extension("png").display(),
        specs = Tools::tool_specs(),
    )
}

/// Action-ish verbs in the user's ask — when the model answers in plain text
/// without touching any tool, a reply to one of these probably means the
/// request was NOT carried out.
fn looks_like_action(s: &str) -> bool {
    [
        "改", "画", "加", "添加", "删", "连", "移", "调整", "新建", "创建", "修复",
        "设计", "生成", "布局", "优化", "重构", "换", "设置", "移动", "连线", "改色",
        "改名", "增", "去掉", "放大", "缩小", "对齐", "重新排列", "拖",
    ]
    .iter()
    .any(|k| s.contains(k))
}

/// Strip ```json fences if the model wrapped its answer in a code block.
fn strip_fences(raw: &str) -> String {
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

/// Rough token estimate for prompt-size guarding (text chars ≈ 0.5 token
/// for CJK-heavy prompts; images billed by pixel area, since vision
/// providers commonly charge ~750px²/token — well above the old flat 900
/// for full-page screenshots, so the floor keeps the guard conservative).
fn estimate_tokens(msgs: &[Message]) -> u64 {
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

impl Harness {
    /// Run one user ask to completion. `stats` accumulates this ask's
    /// usage/cost so the caller keeps session totals across asks.
    #[allow(clippy::too_many_arguments)]
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
        let mut history: Vec<Message> = vec![Message::system(system_prompt(doc, opts.legacy_viewer))];
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
                    .unwrap_or(DEFAULT_MEMORY_TOKENS as u64);
                stats.transcript.extend(tail);
                trim_memory(stats, cap);
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
                remember!();
                return Err(format!(
                    "会话预算已用尽（本次已花 ¥{spent:.4} ≥ 预算 ¥{:.2}）。\
                     可在 ⚙ 设置里调高预算后继续。",
                    opts.budget_remaining
                ));
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
            let env = match parse_envelope(&raw) {
                Ok(v) => v,
                Err(e) => {
                    // Loose fallback: before any tool ran, plain-language
                    // answers (e.g. "6") are accepted as the reply. Once the
                    // model has started calling tools we require protocol
                    // compliance (one correction retry, then give up).
                    if tool_calls == 0 {
                        let mut reply = strip_fences(&raw).trim().to_string();
                        if looks_like_action(user_text) {
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
                    let mut reply = strip_fences(&raw).trim().to_string();
                    if looks_like_action(user_text) {
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
            if !["read", "edit", "draw", "check", "view", "layout"].contains(&name) {
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
                    "`{name}` 不是可用工具。可用工具: read edit draw check view layout。\
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

/// Fold older `read` tool results (the largest text payloads in long
/// sessions — up to 150 lines each) into a one-line stub, keeping the most
/// recent [`KEEP_READS`] results verbatim. Safe by design: line numbers
/// drift on every edit, so the prompt already tells the model not to trust
/// stale reads.
fn fold_old_reads(history: &mut [Message]) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmlfile::XmlDoc;
    use std::collections::VecDeque;

    #[test]
    fn system_prompt_renders_single_brace_json_examples() {
        let doc = XmlDoc::from_text(SAMPLE).unwrap();
        let p = system_prompt(&doc, false);
        assert!(p.contains("Draw.io 样式知识"));
        assert!(!p.contains("2018"));
        let legacy = system_prompt(&doc, true);
        assert!(legacy.contains("2018"));
        assert!(legacy.contains("旧版回退模式"));
        assert!(p.contains(r#"{"tool": "<工具名>""#), "信封示例必须是单层花括号");
        assert!(p.contains("read   {"), "工具清单必须有 read");
        assert!(!p.contains("{{"), "提示词里不应残留双层花括号: {}", &p[p.len().saturating_sub(400)..]);
    }

    #[test]
    fn system_prompt_is_internally_consistent() {
        let doc = XmlDoc::from_text(SAMPLE).unwrap();
        let p = system_prompt(&doc, false);
        // 规则 4「动作最大化」只出现一次（历史上曾因编辑事故重复）。
        assert_eq!(
            p.matches("动作最大化").count(),
            1,
            "规则 4 不应重复: {p}"
        );
        // 无「行尾直接粘上 markdown 标题」的坏格式。
        assert!(!p.contains("。##"), "标题必须另起一行: {p}");
        // 提示词不得向模型宣传不存在的工具（locate 已并入 read 的
        // query 模式；lint 已并入 check）。
        assert!(!p.contains("read/locate"), "locate 残留");
        assert!(!p.contains("6. lint"), "lint 工具条目残留");
        assert!(!p.contains(r#"{"tool": "locate""#), "示例不得用 locate");
        // 纠错消息（运行期生成）示例也必须是合法工具。
        // 工具清单与白名单一致：specs 里的编号工具逐个在白名单内。
        let whitelist = ["read", "edit", "draw", "check", "view", "layout"];
        let specs = Tools::tool_specs();
        for w in whitelist {
            assert!(specs.contains(w), "tool_specs 缺工具 {w}");
        }
        assert!(!specs.contains("lint   {}"), "tool_specs 不应再含 lint");
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
    async fn memory_trim_keeps_newest_when_over_cap() {
        // Direct trim check: a transcript larger than the cap sheds its
        // oldest messages but keeps the newest ask intact.
        let mut stats = SessionStats::default();
        for i in 0..10 {
            let long = format!(
                "第 {i} 轮：{}",
                "这是一段很长的记忆内容，用来撑大 token 估算值。" .repeat(300)
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
