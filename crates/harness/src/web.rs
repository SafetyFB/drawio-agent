//! Web is the primary user entry point. The unit of everything is the
//! **session = one .drawio file**: creating a session creates a file in the
//! sessions dir; each session owns its doc, its rolling memory/usage stats
//! and its per-file history jsonl. The canvas is the human's eyes and hands:
//! 框选 cells -> cell ids travel with the next chat message -> resolved to
//! `@cell:` refs exactly like the REPL's `/sel`. Tools/engine/validation are
//! the same code the CLI uses.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{FromRef, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use crate::chat::{Chat, OpenAiChat};
use crate::config::{self, LlmSettings};
use crate::engine::{EngineEvent, Harness, ProgressFn};
use crate::history::{self, HistoryRec};
use crate::refs;
use crate::tools::Tools;
use crate::xmlfile::{check_doc, XmlDoc};

/// Separate, small lock for the running job's abort handle so /cancel stays
/// responsive even while the big state lock is held by a running engine.
#[derive(Debug, Default)]
pub struct JobControl {
    pub current: Mutex<Option<(u64, tokio::task::AbortHandle)>>,
}

/// Router state bundle (axum resolves per-handler State via FromRef).
#[derive(Clone)]
struct AppState {
    big: Arc<Mutex<WebState>>,
    jobs: Arc<JobControl>,
}

impl FromRef<AppState> for Arc<Mutex<WebState>> {
    fn from_ref(s: &AppState) -> Self {
        s.big.clone()
    }
}
impl FromRef<AppState> for Arc<JobControl> {
    fn from_ref(s: &AppState) -> Self {
        s.jobs.clone()
    }
}

/// Per-session (per-file) state: the doc plus its rolling memory + usage.
#[derive(Debug)]
pub struct SessionState {
    pub doc: XmlDoc,
    pub stats: crate::engine::SessionStats,
}

#[derive(Debug)]
pub struct WebState {
    /// Directory holding one .drawio per session.
    pub dir: PathBuf,
    /// File name of the session currently open in the UI.
    pub current: Option<String>,
    /// Loaded sessions keyed by file name (doc + memory/usage stats).
    pub sessions: std::collections::HashMap<String, SessionState>,
    pub chat: Option<OpenAiChat>,
    pub tools: Tools,
    /// ¥ session budget from the config at the last ask (per-session cap).
    pub budget_yuan: Option<f64>,
    /// One engine job at a time (single user; extra asks fail fast).
    pub running_job: bool,
}

impl WebState {
    fn llm_ready(&self) -> bool {
        self.chat.is_some()
    }

}

pub async fn serve(dir: PathBuf, port: u16) -> Result<(), String> {
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建会话目录失败: {e}"))?;
    let chat = OpenAiChat::from_effective().ok();
    // 启动即打开最近修改的会话（若有），与前端 boot 的选择保持一致
    let mut current = None;
    let mut sessions = std::collections::HashMap::new();
    if let Some(first) = list_session_files(&dir).first() {
        let name = first
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Ok(doc) = XmlDoc::load(first) {
            current = Some(name.clone());
            sessions.insert(
                name,
                SessionState {
                    doc,
                    stats: crate::engine::SessionStats::default(),
                },
            );
        }
    }
    let state = Arc::new(Mutex::new(WebState {
        dir: dir.clone(),
        current,
        sessions,
        chat,
        tools: Tools::new(true),
        budget_yuan: None,
        running_job: false,
    }));

    let jobs = Arc::new(JobControl::default());
    let app_state = AppState { big: state, jobs };
    let app = Router::new()
        .route("/", get(page))
        .route("/app.css", get(css))
        .route("/app.js", get(js))
        .route("/vendor/viewer-static.min.js", get(viewer_bundle))
        .route("/api/state", get(api_state))
        .route("/api/sessions", get(api_sessions_list).post(api_sessions_create))
        .route("/api/sessions/switch", post(api_sessions_switch))
        .route("/api/sessions/:name", axum::routing::delete(api_sessions_delete))
        .route("/api/file", get(api_file))
        .route("/api/chat", post(api_chat))
        .route("/api/check", post(api_check))
        .route("/api/undo", post(api_undo))
        .route("/api/reload", post(api_reload))
        .route("/api/config", get(api_config_get).put(api_config_put))
        .route("/api/config/test", post(api_config_test))
        .route("/api/chat/stream", post(api_chat_stream))
        .route("/api/chat/cancel", post(api_chat_cancel))
        .route("/api/history", get(api_history_list))
        .route("/api/history/:idx", get(api_history_detail))
        .route("/api/history/:idx/restore", post(api_history_restore))
        .route("/api/context/load", post(api_context_load))
        .with_state(app_state);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("绑定 {addr} 失败: {e}"))?;
    let dir = dir.display();
    println!("web 模式: http://{addr}  （会话目录: {dir}，Ctrl-C 退出）");
    axum::serve(listener, app).await.map_err(|e| format!("server: {e}"))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn page() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}
async fn css() -> impl IntoResponse {
    static_text(include_str!("../web/style.css"), "text/css")
}
async fn js() -> impl IntoResponse {
    static_text(include_str!("../web/app.js"), "text/javascript")
}

/// The 3.4MB draw.io viewer bundle lives in the renderer crate's assets
/// (single copy, runtime-read — same approach renderer itself uses).
async fn viewer_bundle() -> Response {
    let mut path: Option<PathBuf> = std::env::var("DRAWIO_VIEWER_PATH").ok().map(PathBuf::from);
    if path.is_none() {
        let guess = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../renderer/assets/viewer-static.min.js");
        if guess.exists() {
            path = Some(guess);
        }
    }
    match path.and_then(|p| std::fs::read(p).ok()) {
        Some(bytes) => static_bytes(bytes, "text/javascript"),
        None => (
            StatusCode::NOT_FOUND,
            "viewer-static.min.js 未找到（设 DRAWIO_VIEWER_PATH）",
        )
            .into_response(),
    }
}

fn static_text(s: &'static str, ct: &'static str) -> Response {
    static_bytes(s.as_bytes().to_vec(), ct)
}

fn static_bytes(bytes: Vec<u8>, ct: &'static str) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, ct)],
        bytes,
    )
        .into_response()
}


/// Grab the current session's doc+stats or a JSON 400 (no session yet).
fn current_err() -> Json<serde_json::Value> {
    Json(json!({ "ok": false, "error": "还没有打开的会话：先创建一个（新建会话 = 新建 .drawio 文件）" }))
}

async fn api_state(State(st): State<Arc<Mutex<WebState>>>) -> Json<serde_json::Value> {
    let st = st.lock().await;
    let Some(cur) = st.current.as_ref() else {
        return Json(json!({ "file": null, "lines": 0, "cells": 0, "llm_ready": st.llm_ready(),
            "render": st.tools.render, "config_source": config_source_label(), "current": null,
            "session": null }));
    };
    let Some(ss) = st.sessions.get(cur) else { return current_err() };
    Json(json!({
        "file": ss.doc.path.display().to_string(),
        "current": cur,
        "lines": ss.doc.canonical().lines().count(),
        "cells": ss.doc.cells.len(),
        "llm_ready": st.llm_ready(),
        "render": st.tools.render,
        "config_source": config_source_label(),
        "session": json!({
            "in": ss.stats.usage.input_tokens,
            "out": ss.stats.usage.output_tokens,
            "cost_yuan": ss.stats.cost_yuan,
            "budget_yuan": st.budget_yuan,
        }),
    }))
}

async fn api_file(State(st): State<Arc<Mutex<WebState>>>) -> Response {
    let st = st.lock().await;
    let Some(cur) = st.current.as_ref() else {
        return (StatusCode::NOT_FOUND, "no session").into_response();
    };
    let Some(ss) = st.sessions.get(cur) else {
        return (StatusCode::NOT_FOUND, "no session").into_response();
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml")],
        ss.doc.canonical().to_string(),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct ChatReq {
    text: String,
    #[serde(default)]
    cell_ids: Vec<String>,
}

/// Canvas selection -> @cell refs -> numbered xml context (REPL /sel 同款).
fn selection_ctx(cell_ids: &[String], doc: &XmlDoc) -> String {
    if cell_ids.is_empty() {
        return String::new();
    }
    let tokens = cell_ids
        .iter()
        .map(|id| format!("@cell:{id}"))
        .collect::<Vec<_>>()
        .join(" ");
    let (resolved, errors, snippet) = refs::resolve_refs(&tokens, doc);
    for e in &errors {
        eprintln!("selection ref warning: {e}");
    }
    if resolved.is_empty() {
        String::new()
    } else {
        snippet
    }
}

async fn api_chat(
    State(st): State<Arc<Mutex<WebState>>>,
    Json(req): Json<ChatReq>,
) -> Json<serde_json::Value> {
    let mut st = st.lock().await;
    if st.running_job {
        return Json(json!({ "reply": null, "error": "另一个任务正在运行中——先点「停止」或等它完成" }));
    }
    if st.chat.is_none() {
        return Json(json!({
            "reply": null,
            "error": "LLM 未配置：点右上角 ⚙ 填写并保存"
        }));
    }
    let Some(cur) = st.current.clone() else { return current_err() };
    let ctx = selection_ctx(&req.cell_ids, &st.sessions.get(&cur).map(|s| &s.doc).unwrap());
    let cfg = config::effective_settings().unwrap_or_default();
    let budget_yuan = cfg.budget_yuan;
    let opts = {
        let mut o = crate::engine::RunOpts::from_settings(&cfg);
        if let Some(b) = budget_yuan {
            o.budget_remaining = (b - st.sessions[&cur].stats.cost_yuan).max(0.0);
        }
        o
    };
    let harness = Harness::default();
    let WebState { sessions, chat, tools, .. } = &mut *st;
    let ss = sessions.get_mut(&cur).expect("session exists");
    let SessionState { doc, stats } = ss;
    let chat = chat.as_mut().expect("checked above");
    match harness.run(chat, tools, doc, &req.text, &ctx, &opts, stats, &None).await {
        Ok(outcome) => Json(json!({
            "reply": outcome.reply,
            "tool_calls": outcome.tool_calls,
            "cells": doc.cells.len(),
            "selected_refs": if ctx.is_empty() { 0 } else { ctx.lines().count() },
            "usage": json!({ "in": outcome.usage.input_tokens, "out": outcome.usage.output_tokens }),
            "cost_yuan": outcome.cost_yuan,
            "session": json!({
                "in": stats.usage.input_tokens,
                "out": stats.usage.output_tokens,
                "cost_yuan": stats.cost_yuan,
                "budget_yuan": budget_yuan,
            }),
            "error": null,
        })),
        Err(e) => Json(json!({
            "reply": null, "tool_calls": 0, "error": e,
            "session": json!({
                "in": stats.usage.input_tokens,
                "out": stats.usage.output_tokens,
                "cost_yuan": stats.cost_yuan,
                "budget_yuan": budget_yuan,
            }),
        })),
    }
}

async fn api_check(State(st): State<Arc<Mutex<WebState>>>) -> Json<serde_json::Value> {
    let st = st.lock().await;
    let Some(cur) = st.current.as_ref() else { return current_err() };
    let Some(ss) = st.sessions.get(cur) else { return current_err() };
    match check_doc(ss.doc.canonical()) {
        Ok(r) => Json(json!({
            "ok": r.ok(),
            "cells": r.cells,
            "edges": r.edges,
            "issues": r.issues,
        })),
        Err(e) => Json(json!({ "ok": false, "issues": [e.to_string()] })),
    }
}

async fn api_undo(State(st): State<Arc<Mutex<WebState>>>) -> Json<serde_json::Value> {
    let mut st = st.lock().await;
    let Some(cur) = st.current.clone() else { return current_err() };
    let Some(ss) = st.sessions.get_mut(&cur) else { return current_err() };
    match ss.doc.undo() {
        Some(_) => {
            let _ = ss.doc.save();
            Json(json!({ "ok": true }))
        }
        None => Json(json!({ "ok": false, "error": "没有可撤销的编辑" })),
    }
}

async fn api_reload(State(st): State<Arc<Mutex<WebState>>>) -> Json<serde_json::Value> {
    let mut st = st.lock().await;
    let Some(cur) = st.current.clone() else { return current_err() };
    let Some(ss) = st.sessions.get_mut(&cur) else { return current_err() };
    let path = ss.doc.path.clone();
    match XmlDoc::load(&path) {
        Ok(d) => {
            ss.doc = d;
            Json(json!({ "ok": true, "cells": ss.doc.cells.len() }))
        }
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}


// ---------------------------------------------------------------------------
// LLM settings (config file + hot swap) — same panel semantics as the old UI
// ---------------------------------------------------------------------------

fn config_source_label() -> &'static str {
    match config::effective_source() {
        config::ConfigSource::File => "file",
        config::ConfigSource::Env => "env",
        config::ConfigSource::None => "none",
    }
}

fn llm_view() -> serde_json::Value {
    match config::effective_settings() {
        Some(s) => json!({
            "kind": "openai-compat",
            "base_url": s.base_url,
            "model": s.model,
            "api_key_masked": s.api_key_masked(),
            "context_length": s.context_length,
            "thinking": s.thinking,
            "price_input_per_m": s.price_input_per_m,
            "price_output_per_m": s.price_output_per_m,
            "budget_yuan": s.budget_yuan,
        }),
        None => json!({ "kind": "unconfigured", "base_url": "", "model": "", "api_key_masked": "",
            "context_length": null, "thinking": "default",
            "price_input_per_m": 0.0, "price_output_per_m": 0.0, "budget_yuan": null }),
    }
}

fn config_file_display() -> Option<String> {
    config::config_file_path().map(|p| p.display().to_string())
}

async fn api_config_get() -> Json<serde_json::Value> {
    Json(json!({
        "llm": llm_view(),
        "config_file": config_file_display(),
        "source": config_source_label(),
        "configured": config::effective_settings().is_some(),
    }))
}

/// PUT body. `api_key`: empty string / missing = keep the current key.
#[derive(Debug, Deserialize)]
struct ConfigPutReq {
    base_url: String,
    model: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    thinking: Option<config::ThinkingMode>,
    #[serde(default)]
    price_input_per_m: Option<f64>,
    #[serde(default)]
    price_output_per_m: Option<f64>,
    #[serde(default)]
    budget_yuan: Option<f64>,
}

async fn api_config_put(
    State(st): State<Arc<Mutex<WebState>>>,
    Json(req): Json<ConfigPutReq>,
) -> Json<serde_json::Value> {
    let base_url = req.base_url.trim().to_string();
    let model = req.model.trim().to_string();
    if base_url.is_empty() || model.is_empty() {
        return Json(json!({ "ok": false, "error": "Base URL 与 Model 不能为空" }));
    }
    // Empty key on save = keep whatever is effective now.
    let api_key = if req.api_key.trim().is_empty() {
        config::effective_settings()
            .map(|s| s.api_key)
            .unwrap_or_default()
    } else {
        req.api_key.trim().to_string()
    };
    // Full-form semantics: every field is sent by the UI (null/absent =
    // cleared to default), except the API key which is merged on blank.
    let settings = LlmSettings {
        base_url,
        model,
        api_key,
        context_length: req.context_length,
        thinking: req.thinking.unwrap_or_default(),
        price_input_per_m: req.price_input_per_m.unwrap_or(0.0),
        price_output_per_m: req.price_output_per_m.unwrap_or(0.0),
        budget_yuan: req.budget_yuan,
    };
    let Some(path) = config::config_file_path() else {
        return Json(json!({ "ok": false, "error": "找不到 config 文件路径（HOME 未设置？）" }));
    };
    if let Err(e) = config::save_config_file(&path, &settings) {
        return Json(json!({ "ok": false, "error": format!("保存失败: {e}") }));
    }
    // Hot swap: rebuild the live chat client from what we just saved (file
    // wins over env once a config exists, mirroring the old server).
    let mut st = st.lock().await;
    st.chat = OpenAiChat::from_settings(&settings).ok();
    st.budget_yuan = settings.budget_yuan;
    Json(json!({
        "ok": true,
        "config_file": config_file_display(),
        "llm": llm_view(),
        "configured": true,
    }))
}

async fn api_config_test(Json(req): Json<ConfigPutReq>) -> Json<serde_json::Value> {
    let base_url = req.base_url.trim().to_string();
    let model = req.model.trim().to_string();
    if base_url.is_empty() || model.is_empty() {
        return Json(json!({ "ok": false, "error": "先填 Base URL 与 Model" }));
    }
    let api_key = if req.api_key.trim().is_empty() {
        config::effective_settings()
            .map(|s| s.api_key)
            .unwrap_or_default()
    } else {
        req.api_key.trim().to_string()
    };
    let settings = LlmSettings { base_url, model, api_key, ..Default::default() };
    let mut chat = match OpenAiChat::from_settings(&settings) {
        Ok(c) => c,
        Err(e) => return Json(json!({ "ok": false, "error": e.to_string() })),
    };
    let start = std::time::Instant::now();
    match chat
        .complete(
            &[crate::chat::Message::user("Reply with exactly: pong")],
            &crate::chat::CallOpts::default(),
        )
        .await
    {
        Ok(reply) => Json(json!({
            "ok": true,
            "ms": start.elapsed().as_millis(),
            "model": settings.model,
            "reply": reply.text.trim(),
            "usage": json!({ "in": reply.usage.input_tokens, "out": reply.usage.output_tokens }),
        })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}


// ---------------------------------------------------------------------------
// R4: streaming progress (NDJSON over fetch) + cancel
// ---------------------------------------------------------------------------

static JOB_ID: AtomicU64 = AtomicU64::new(0);

/// One engine event -> one NDJSON line for the frontend.
fn event_line(ev: &EngineEvent) -> serde_json::Value {
    match ev {
        EngineEvent::Turn { index } => json!({ "type": "turn", "index": index }),
        EngineEvent::ModelOutput { raw } => json!({
            "type": "model",
            "preview": truncate_utf8(raw, 300),
        }),
        EngineEvent::Tool { name, args } => json!({
            "type": "tool",
            "name": name,
            "args": truncate_utf8(args, 200),
        }),
        EngineEvent::ToolResult { name, text, has_image } => json!({
            "type": "tool_result",
            "name": name,
            "preview": text.lines().next().unwrap_or("").chars().take(200).collect::<String>(),
            "has_image": has_image,
        }),
        EngineEvent::Usage { usage, cost_yuan } => json!({
            "type": "usage",
            "in": usage.input_tokens,
            "out": usage.output_tokens,
            "cost_yuan": cost_yuan,
        }),
        EngineEvent::Final { reply } => json!({ "type": "reply", "reply": reply }),
    }
}

fn truncate_utf8(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

async fn api_chat_stream(
    State(st): State<Arc<Mutex<WebState>>>,
    State(jobs): State<Arc<JobControl>>,
    Json(req): Json<ChatReq>,
) -> Response {
    // One job at a time: fail fast instead of queueing behind the lock
    // (the UI guards too; this covers second tabs / curl).
    {
        let mut st = st.lock().await;
        if st.running_job {
            return Json(json!({
                "type": "error",
                "error": "另一个任务正在运行中——先点「停止」或等它完成"
            }))
            .into_response();
        }
        if st.chat.is_none() {
            return Json(json!({
                "type": "error",
                "error": "LLM 未配置：点右上角 ⚙ 填写并保存"
            }))
            .into_response();
        }
        if st.current.is_none() {
            return Json(json!({
                "type": "error",
                "error": "还没有打开的会话：先点「＋ 新建会话」"
            }))
            .into_response();
        }
        st.running_job = true;
    }

    let (tx, rx) = mpsc::channel::<Vec<u8>>(128);
    let job_id = JOB_ID.fetch_add(1, Ordering::SeqCst) + 1;
    let st2 = st.clone();
    let jobs2 = jobs.clone();
    let task = tokio::spawn(async move {
        let mut st = st2.lock().await;
        let cur = st.current.clone().expect("pre-checked");
        let WebState { sessions, chat, tools, budget_yuan, .. } = &mut *st;
        let chat = chat.as_mut().expect("pre-checked");
        let ss = sessions.get_mut(&cur).expect("pre-checked");
        let SessionState { doc, stats } = ss;
        let cfg = config::effective_settings().unwrap_or_default();
        *budget_yuan = cfg.budget_yuan;
        let opts = {
            let mut o = crate::engine::RunOpts::from_settings(&cfg);
            if let Some(b) = *budget_yuan {
                o.budget_remaining = (b - stats.cost_yuan).max(0.0);
            }
            o
        };
        // Canvas selection -> @cell refs (same as REPL /sel / old endpoint).
        let ctx = selection_ctx(&req.cell_ids, doc);
        let harness = Harness::default();
        let tx2 = tx.clone();
        let events: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Arc::default();
        let events2 = events.clone();
        let progress: ProgressFn = Arc::new(move |ev: EngineEvent| {
            let line = event_line(&ev);
            if let Ok(mut v) = events2.lock() {
                v.push(line.clone());
            }
            let _ = tx2.try_send(format!("{line}\n").into_bytes());
        });

        // 客户端断开监视器：接收端掉线（页面刷新/关闭）即中止引擎并释放
        // 大锁。Sender::is_closed 与发送无关，模型长时间思考时同样即时。
        let tx3 = tx.clone();
        let monitor = async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if tx3.is_closed() {
                    return;
                }
            }
        };
        let run_fut = async {
            harness
                .run(chat, tools, doc, &req.text, &ctx, &opts, stats, &Some(progress))
                .await
        };
        let outcome = tokio::select! {
            _ = monitor => Err("客户端已断开（页面刷新/关闭），任务已停止。".to_string()),
            r = run_fut => r,
        };
        let (reply, error, tool_calls) = match &outcome {
            Ok(o) => (o.reply.clone(), None, o.tool_calls),
            Err(e) => (String::new(), Some(e.clone()), 0),
        };
        // R5: durable per-file history with the full trajectory.
        let rec = HistoryRec {
            ts: history::now_secs(),
            user: req.text.clone(),
            reply: reply.clone(),
            tool_calls: tool_calls as u32,
            usage_in: stats.usage.input_tokens,
            usage_out: stats.usage.output_tokens,
            cost_yuan: stats.cost_yuan,
            events: events.lock().map(|v| v.clone()).unwrap_or_default(),
            xml: doc.canonical().to_string(),
            error,
        };
        let _ = history::append(&history::history_path(&doc.path), &rec);
        if !tx.is_closed() {
            match outcome {
                Ok(o) => {
                    let _ = tx
                        .send(
                            format!(
                                "{}\n",
                                json!({
                                    "type": "done",
                                    "tool_calls": o.tool_calls,
                                    "session": json!({
                                        "in": stats.usage.input_tokens,
                                        "out": stats.usage.output_tokens,
                                        "cost_yuan": stats.cost_yuan,
                                        "budget_yuan": budget_yuan,
                                    }),
                                })
                            )
                            .into_bytes(),
                        )
                        .await;
                }
                Err(e) => {
                    let _ = tx
                        .send(format!("{}\n", json!({ "type": "error", "error": e })).into_bytes())
                        .await;
                }
            }
        }
        // Clear this job's abort handle, then release the big lock and
        // mark the slot free (drop(st) first: we still hold it here).
        drop(st);
        let mut cur = jobs2.current.lock().await;
        if let Some((id, _)) = cur.as_ref() {
            if *id == job_id {
                *cur = None;
            }
        }
        if let Ok(mut st) = st2.try_lock() {
            st.running_job = false;
        }
    });

    jobs.current.lock().await.replace((job_id, task.abort_handle()));
    let stream = ReceiverStream::new(rx).map(|bytes| Ok::<_, std::io::Error>(axum::body::Bytes::from(bytes)));
    (
        axum::http::StatusCode::OK,
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn api_chat_cancel(
    State(st): State<Arc<Mutex<WebState>>>,
    State(jobs): State<Arc<JobControl>>,
) -> Json<serde_json::Value> {
    let mut cur = jobs.current.lock().await;
    match cur.take() {
        Some((id, handle)) => {
            handle.abort();
            // The aborted task drops the big lock at its next await; once we
            // can take it, mark the job slot free.
            drop(cur);
            st.lock().await.running_job = false;
            Json(json!({ "ok": true, "job": id, "note": "已停止" }))
        }
        None => Json(json!({ "ok": false, "error": "当前没有运行中的任务" })),
    }
}


// ---------------------------------------------------------------------------
// R5: per-file history + session context save/load
// ---------------------------------------------------------------------------

fn hp(doc: &XmlDoc) -> std::path::PathBuf {
    history::history_path(&doc.path)
}

async fn api_history_list(State(st): State<Arc<Mutex<WebState>>>) -> Json<serde_json::Value> {
    let st = st.lock().await;
    let Some(cur) = st.current.as_ref() else { return current_err() };
    let Some(ss) = st.sessions.get(cur) else { return current_err() };
    let recs = history::list(&hp(&ss.doc), 50);
    let list: Vec<serde_json::Value> = recs
        .iter()
        .enumerate()
        .map(|(i, r)| {
            json!({
                "idx": i,
                "ts": r.ts,
                "user": r.user.chars().take(120).collect::<String>(),
                "tool_calls": r.tool_calls,
                "usage_in": r.usage_in,
                "usage_out": r.usage_out,
                "cost_yuan": r.cost_yuan,
                "error": r.error,
                "xml_chars": r.xml.chars().count(),
            })
        })
        .collect();
    Json(json!({ "records": list, "file": hp(&ss.doc).display().to_string() }))
}

async fn api_history_detail(
    State(st): State<Arc<Mutex<WebState>>>,
    axum::extract::Path(idx): axum::extract::Path<usize>,
) -> Json<serde_json::Value> {
    let st = st.lock().await;
    let Some(cur) = st.current.as_ref() else { return current_err() };
    let Some(ss) = st.sessions.get(cur) else { return current_err() };
    let recs = history::list(&hp(&ss.doc), 50);
    match recs.get(idx) {
        Some(r) => Json(json!({ "ok": true, "record": r })),
        None => Json(json!({ "ok": false, "error": format!("没有第 {idx} 条历史记录") })),
    }
}

async fn api_history_restore(
    State(st): State<Arc<Mutex<WebState>>>,
    axum::extract::Path(idx): axum::extract::Path<usize>,
) -> Json<serde_json::Value> {
    let mut st = st.lock().await;
    let Some(cur) = st.current.clone() else { return current_err() };
    let Some(ss) = st.sessions.get_mut(&cur) else { return current_err() };
    let recs = history::list(&hp(&ss.doc), 50);
    let Some(rec) = recs.get(idx) else {
        return Json(json!({ "ok": false, "error": format!("没有第 {idx} 条历史记录") }));
    };
    let save_path = ss.doc.path.clone();
    match XmlDoc::from_text_at(&rec.xml, &save_path) {
        Ok(d) => {
            ss.doc = d;
            let _ = ss.doc.save();
            Json(json!({ "ok": true, "cells": ss.doc.cells.len(), "note": "已恢复到该历史版本的 xml（会话统计不清零）" }))
        }
        Err(e) => Json(json!({ "ok": false, "error": format!("历史 xml 无法加载: {e}") })),
    }
}

/// Import a SessionBundle (whole-context save/load): replaces the doc with
/// the bundle's xml and replays its transcript into the rolling memory.
async fn api_context_load(
    State(st): State<Arc<Mutex<WebState>>>,
    body: axum::body::Bytes,
) -> Json<serde_json::Value> {
    let bundle: Result<history::SessionBundle, _> = serde_json::from_slice(&body);
    let bundle = match bundle {
        Ok(b) => b,
        Err(e) => {
            return Json(json!({ "ok": false, "error": format!("不是有效的会话 JSON: {e}") }));
        }
    };
    let mut st = st.lock().await;
    let Some(cur) = st.current.clone() else { return current_err() };
    let Some(ss) = st.sessions.get_mut(&cur) else { return current_err() };
    let save_path = ss.doc.path.clone();
    let doc = match XmlDoc::from_text_at(&bundle.xml, &save_path) {
        Ok(d) => d,
        Err(e) => return Json(json!({ "ok": false, "error": format!("会话里的 xml 无法加载: {e}") })),
    };
    ss.doc = doc;
    let _ = ss.doc.save();
    ss.stats.transcript = history::SessionBundle::strip_images(&bundle.messages);
    ss.stats.usage = crate::chat::Usage {
        input_tokens: bundle.usage_in,
        output_tokens: bundle.usage_out,
    };
    ss.stats.cost_yuan = bundle.cost_yuan;
    Json(json!({
        "ok": true,
        "cells": ss.doc.cells.len(),
        "memory_messages": ss.stats.transcript.len(),
        "note": "已导入到当前会话：文档与多轮记忆均已恢复",
    }))
}


// ---------------------------------------------------------------------------
// Sessions: 会话 = 一个 .drawio 文件（创建会话 = 创建文件）
// ---------------------------------------------------------------------------

fn sanitize_name(name: &str) -> String {
    let mut n: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.' | '(' | ')') {
                c
            } else {
                '-'
            }
        })
        .collect();
    while n.contains("--") {
        n = n.replace("--", "-");
    }
    let mut n = n.trim().trim_matches('.').to_string();
    if n.is_empty() {
        n = format!("untitled-{}", history::now_secs());
    }
    if !n.ends_with(".drawio") {
        n = format!("{n}.drawio");
    }
    n
}

fn list_session_files(dir: &PathBuf) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "drawio").unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    out.reverse();
    out
}

async fn api_sessions_list(State(st): State<Arc<Mutex<WebState>>>) -> Json<serde_json::Value> {
    let st = st.lock().await;
    let files = list_session_files(&st.dir);
    let sessions: Vec<serde_json::Value> = files
        .iter()
        .map(|p| {
            let name = p
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (cells, loaded) = match st.sessions.get(&name) {
                Some(ss) => (Some(ss.doc.cells.len()), true),
                None => (None, false),
            };
            json!({
                "name": name,
                "size": std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
                "loaded": loaded,
                "cells": cells,
                "current": st.current.as_deref() == Some(name.as_str()),
            })
        })
        .collect();
    Json(json!({ "sessions": sessions, "dir": st.dir.display().to_string() }))
}

/// Create a session = create a .drawio file and open it.
async fn api_sessions_create(
    State(st): State<Arc<Mutex<WebState>>>,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    let mut st = st.lock().await;
    let payload = body.map(|Json(b)| b).unwrap_or(serde_json::json!({}));
    let want = payload
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut name = sanitize_name(&want);
    // uniquify on collision
    let mut i = 1;
    while st.sessions.contains_key(&name) || list_session_files(&st.dir).iter().any(|p| {
        p.file_name().map(|f| f.to_string_lossy() == name.as_str()).unwrap_or(false)
    }) {
        let stem = name.trim_end_matches(".drawio");
        name = if i == 1 { format!("{stem}-2.drawio") } else { format!("{stem}-{i}.drawio") };
        i += 1;
    }
    let path = st.dir.join(&name);
    let _ = &path;
    let template = payload
        .get("xml")
        .and_then(|v| v.as_str())
        .map(|x| x.to_string());
    let doc = match template {
        Some(xml) => XmlDoc::from_text_at(&xml, &path).map_err(|e| e.to_string()),
        None => XmlDoc::from_text_at(crate::EMPTY_TEMPLATE, &path).map_err(|e| e.to_string()),
    };
    let doc = match doc {
        Ok(d) => d,
        Err(e) => return Json(json!({ "ok": false, "error": format!("创建会话失败: {e}") })),
    };
    if let Err(e) = doc.save() {
        return Json(json!({ "ok": false, "error": format!("写文件失败: {e}") }));
    }
    st.sessions.insert(
        name.clone(),
        SessionState {
            doc,
            stats: crate::engine::SessionStats::default(),
        },
    );
    st.current = Some(name.clone());
    Json(json!({ "ok": true, "name": name, "note": "新会话已创建（会话 = 文件）" }))
}

async fn api_sessions_switch(
    State(st): State<Arc<Mutex<WebState>>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let mut st = st.lock().await;
    if !st.sessions.contains_key(&name) {
        let path = st.dir.join(&name);
        if !path.exists() {
            return Json(json!({ "ok": false, "error": format!("会话不存在: {name}") }));
        }
        let doc = match XmlDoc::load(&path) {
            Ok(d) => d,
            Err(e) => return Json(json!({ "ok": false, "error": format!("加载失败: {e}") })),
        };
        st.sessions.insert(
            name.clone(),
            SessionState {
                doc,
                stats: crate::engine::SessionStats::default(),
            },
        );
    }
    st.current = Some(name.clone());
    let ss = st.sessions.get(&name).expect("just inserted");
    Json(json!({
        "ok": true,
        "current": name,
        "cells": ss.doc.cells.len(),
        "lines": ss.doc.canonical().lines().count(),
        "note": "已切换到会话（文件）"
    }))
}

async fn api_sessions_delete(
    State(st): State<Arc<Mutex<WebState>>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    let mut st = st.lock().await;
    let path = st.dir.join(&name);
    if !path.exists() {
        return Json(json!({ "ok": false, "error": format!("会话不存在: {name}") }));
    }
    let _ = std::fs::remove_file(&path);
    let hp = history::history_path(&path);
    let _ = std::fs::remove_file(&hp);
    st.sessions.remove(&name);
    if st.current.as_deref() == Some(name.as_str()) {
        st.current = None;
    }
    Json(json!({ "ok": true, "note": format!("已删除会话 {name}（文件与历史）") }))
}
