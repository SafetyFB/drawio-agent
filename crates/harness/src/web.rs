//! M6: slim web shell — mxGraph canvas + selection over the same harness.
//!
//! One process, one XmlDoc, one chat session. The canvas is a human's eyes
//! and hands: 框选 cells -> cell ids travel with the next chat message ->
//! the harness translates them into `@cell:` refs (span index -> numbered
//! xml context) exactly like the REPL's `/sel`. Everything else (tools,
//! engine, view/vision, validation, file saves) is the same code the CLI
//! uses — there is no second orchestration stack.

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

#[derive(Debug)]
pub struct WebState {
    pub file: String,
    pub doc: XmlDoc,
    pub chat: Option<OpenAiChat>,
    pub tools: Tools,
    /// Session totals across asks (usage + ¥ under current prices).
    pub usage: crate::engine::SessionStats,
    /// ¥ session budget from the config at the last ask.
    pub budget_yuan: Option<f64>,
}

impl WebState {
    fn llm_ready(&self) -> bool {
        self.chat.is_some()
    }
}

pub async fn serve(path: PathBuf, port: u16) -> Result<(), String> {
    let doc = XmlDoc::load(&path).map_err(|e| format!("加载失败: {e}"))?;
    if let Err(e) = doc.save() {
        eprintln!("警告: 保存规范化文件失败: {e}");
    }
    let chat = OpenAiChat::from_effective().ok();
    let state = Arc::new(Mutex::new(WebState {
        file: path.display().to_string(),
        doc,
        chat,
        tools: Tools::new(true),
        usage: crate::engine::SessionStats::default(),
        budget_yuan: None,
    }));

    let jobs = Arc::new(JobControl::default());
    let app_state = AppState { big: state, jobs };
    let app = Router::new()
        .route("/", get(page))
        .route("/app.css", get(css))
        .route("/app.js", get(js))
        .route("/vendor/viewer-static.min.js", get(viewer_bundle))
        .route("/api/state", get(api_state))
        .route("/api/file", get(api_file))
        .route("/api/chat", post(api_chat))
        .route("/api/check", post(api_check))
        .route("/api/undo", post(api_undo))
        .route("/api/reload", post(api_reload))
        .route("/api/config", get(api_config_get).put(api_config_put))
        .route("/api/config/test", post(api_config_test))
        .route("/api/chat/stream", post(api_chat_stream))
        .route("/api/chat/cancel", post(api_chat_cancel))
        .with_state(app_state);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("绑定 {addr} 失败: {e}"))?;
    println!("web 模式: http://{addr}  （Ctrl-C 退出）");
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

async fn api_state(State(st): State<Arc<Mutex<WebState>>>) -> Json<serde_json::Value> {
    let st = st.lock().await;
    Json(json!({
        "file": st.file,
        "lines": st.doc.canonical().lines().count(),
        "cells": st.doc.cells.len(),
        "llm_ready": st.llm_ready(),
        "render": st.tools.render,
        "config_source": config_source_label(),
        "session": json!({
            "in": st.usage.usage.input_tokens,
            "out": st.usage.usage.output_tokens,
            "cost_yuan": st.usage.cost_yuan,
            "budget_yuan": st.budget_yuan,
        }),
    }))
}

async fn api_file(State(st): State<Arc<Mutex<WebState>>>) -> Response {
    let st = st.lock().await;
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml")],
        st.doc.canonical().to_string(),
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
    if st.chat.is_none() {
        return Json(json!({
            "reply": null,
            "error": "LLM 未配置：设置 DRAWIO_LLM_BASE_URL / DRAWIO_LLM_MODEL（DRAWIO_LLM_API_KEY 可选）后重启"
        }));
    }
    // Canvas selection -> @cell refs (same as REPL /sel / stream endpoint).
    let ctx = selection_ctx(&req.cell_ids, &st.doc);
    let cfg = config::effective_settings().unwrap_or_default();
    let budget_yuan = cfg.budget_yuan;
    let opts = {
        let mut o = crate::engine::RunOpts::from_settings(&cfg);
        if let Some(b) = budget_yuan {
            o.budget_remaining = (b - st.usage.cost_yuan).max(0.0);
        }
        o
    };
    let harness = Harness::default();
    let WebState { doc, chat, tools, usage, .. } = &mut *st;
    let chat = chat.as_mut().expect("checked above");
    match harness.run(chat, tools, doc, &req.text, &ctx, &opts, usage, &None).await {
        Ok(outcome) => Json(json!({
            "reply": outcome.reply,
            "tool_calls": outcome.tool_calls,
            "cells": doc.cells.len(),
            "selected_refs": if ctx.is_empty() { 0 } else { ctx.lines().count() },
            "usage": json!({ "in": outcome.usage.input_tokens, "out": outcome.usage.output_tokens }),
            "cost_yuan": outcome.cost_yuan,
            "session": json!({
                "in": usage.usage.input_tokens,
                "out": usage.usage.output_tokens,
                "cost_yuan": usage.cost_yuan,
                "budget_yuan": budget_yuan,
            }),
            "error": null,
        })),
        Err(e) => Json(json!({
            "reply": null, "tool_calls": 0, "error": e,
            "session": json!({
                "in": usage.usage.input_tokens,
                "out": usage.usage.output_tokens,
                "cost_yuan": usage.cost_yuan,
                "budget_yuan": budget_yuan,
            }),
        })),
    }
}

async fn api_check(State(st): State<Arc<Mutex<WebState>>>) -> Json<serde_json::Value> {
    let st = st.lock().await;
    match check_doc(st.doc.canonical()) {
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
    match st.doc.undo() {
        Some(_) => {
            let _ = st.doc.save();
            Json(json!({ "ok": true }))
        }
        None => Json(json!({ "ok": false, "error": "没有可撤销的编辑" })),
    }
}

async fn api_reload(State(st): State<Arc<Mutex<WebState>>>) -> Json<serde_json::Value> {
    let mut st = st.lock().await;
    match XmlDoc::load(&st.file) {
        Ok(d) => {
            st.doc = d;
            Json(json!({ "ok": true, "cells": st.doc.cells.len() }))
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
    let configured = { st.lock().await.chat.is_some() };
    if !configured {
        return Json(json!({
            "type": "error",
            "error": "LLM 未配置：点右上角 ⚙ 填写并保存"
        }))
        .into_response();
    }

    let (tx, rx) = mpsc::channel::<Vec<u8>>(128);
    let job_id = JOB_ID.fetch_add(1, Ordering::SeqCst) + 1;
    let st2 = st.clone();
    let jobs2 = jobs.clone();
    let task = tokio::spawn(async move {
        let mut st = st2.lock().await;
        let WebState { doc, chat, tools, usage, budget_yuan, .. } = &mut *st;
        let Some(chat) = chat.as_mut() else { return };
        let cfg = config::effective_settings().unwrap_or_default();
        *budget_yuan = cfg.budget_yuan;
        let opts = {
            let mut o = crate::engine::RunOpts::from_settings(&cfg);
            if let Some(b) = *budget_yuan {
                o.budget_remaining = (b - usage.cost_yuan).max(0.0);
            }
            o
        };
        // Canvas selection -> @cell refs (same as REPL /sel / old endpoint).
        let ctx = selection_ctx(&req.cell_ids, doc);
        let harness = Harness::default();
        let tx2 = tx.clone();
        let progress: ProgressFn = Arc::new(move |ev: EngineEvent| {
            let line = event_line(&ev).to_string();
            let _ = tx2.try_send(format!("{line}\n").into_bytes());
        });
        let outcome = harness
            .run(chat, tools, doc, &req.text, &ctx, &opts, usage, &Some(progress))
            .await;
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
                                    "in": usage.usage.input_tokens,
                                    "out": usage.usage.output_tokens,
                                    "cost_yuan": usage.cost_yuan,
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
        // Clear this job's abort handle (best effort).
        let mut cur = jobs2.current.lock().await;
        if let Some((id, _)) = cur.as_ref() {
            if *id == job_id {
                *cur = None;
            }
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
    State(jobs): State<Arc<JobControl>>,
) -> Json<serde_json::Value> {
    let mut cur = jobs.current.lock().await;
    match cur.take() {
        Some((id, handle)) => {
            handle.abort();
            Json(json!({ "ok": true, "job": id, "note": "已发送停止信号" }))
        }
        None => Json(json!({ "ok": false, "error": "当前没有运行中的任务" })),
    }
}
