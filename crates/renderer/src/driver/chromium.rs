//! Real headless-Chromium render driver speaking CDP over WebSocket.
//!
//! Launches a headless Chromium/Chrome binary with `--remote-debugging-port=0`,
//! discovers the DevTools ws:// endpoint from stderr, then drives it with a
//! minimal hand-rolled CDP client (tokio-tungstenite). Rendering loads a
//! bundled `render.html` that uses the draw.io viewer static bundle
//! (`mxGraph` + `mxCodec`) to render the diagram into the DOM; the driver
//! captures the result with `Page.captureScreenshot`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;
use tokio_tungstenite::{tungstenite::Message, MaybeTlsStream};
use tracing::{debug, info, warn};
use base64::Engine as _;

use crate::{RenderDriver, RenderError, RenderOptions};

/// Budget for one `__doRender` evaluate (load + annotate + export inside
/// the hot page). Generous on purpose: large diagrams on slow machines.
const RENDER_EVALUATE_TIMEOUT: Duration = Duration::from_secs(45);

// ---------------------------------------------------------------------------
// Binary discovery
// ---------------------------------------------------------------------------

/// Resolve a usable CDP browser binary. Lookup order (see chromium_ensure):
/// 1. `DRAWIO_AGENT_CHROMIUM_PATH` env var (always wins)
/// 2. Cached bundled chrome-headless-shell
/// 3. System Chrome/Chromium/Edge/Brave (no download)
/// 4. Lazy download of the pinned bundle
pub fn find_chromium() -> Option<PathBuf> {
    crate::chromium_ensure::resolve_chromium()
        .map_err(|e| eprintln!("chromium 解析失败: {e}"))
        .ok()
        .flatten()
}

// ---------------------------------------------------------------------------
// CDP message types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct CdpRequest {
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
    /// Present for page-session commands (after Target.attachToTarget).
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CdpResponse {
    id: Option<u64>,
    method: Option<String>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<CdpError>,
}

#[derive(Debug, Deserialize)]
struct CdpError {
    message: String,
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// A real Chromium renderer driven over the DevTools protocol.
#[derive(Debug)]
pub struct HeadlessChromiumDriver {
    inner: Arc<ChromiumInner>,
}

/// Pending CDP request id -> oneshot response sender.
type PendingMap = HashMap<u64, tokio::sync::oneshot::Sender<Result<Value, RenderError>>>;

#[derive(Debug)]
struct ChromiumInner {
    /// Keep the child pid so `Drop` can kill the process.
    _child_pid: u32,
    /// Throwaway user-data-dir created for this launch; removed on Drop
    /// (otherwise every browser launch leaks a profile into /tmp).
    profile_dir: PathBuf,
    write: mpsc::Sender<Message>,
    next_id: Mutex<u64>,
    /// Pending request id -> oneshot response sender.
    pending: Arc<Mutex<PendingMap>>,
    /// Page-session id after `Target.attachToTarget`; `None` before then.
    session_id: Mutex<Option<String>>,
    /// Keeps the WebSocket reader task alive.
    _reader_task: tokio::task::JoinHandle<()>,
    /// 热页面：wrapper 已导航并等待 app 就绪后置真；进程级复用。
    page_ready: std::sync::atomic::AtomicBool,
    /// 串行化渲染（同一进程/页面一次只跑一个导出）。
    render_lock: tokio::sync::Mutex<()>,
}

impl Drop for ChromiumInner {
    fn drop(&mut self) {
        // Best-effort kill + profile cleanup; not in async context here.
        #[cfg(unix)]
        let _ = std::process::Command::new("kill")
            .arg(self._child_pid.to_string())
            .output();
        // Non-unix (Windows has no `kill` binary): the child dies with the
        // parent process in practice; proper Job Objects are out of scope.
        let _ = std::fs::remove_dir_all(&self.profile_dir);
    }
}

impl HeadlessChromiumDriver {
    /// Locate a Chromium binary and launch it.
    pub async fn launch() -> Result<Self, RenderError> {
        // 解析可能触发首次下载（~90MB），放 blocking 池避免卡住异步运行时
        let path = tokio::task::spawn_blocking(find_chromium)
            .await
            .unwrap_or(None)
            .ok_or_else(|| {
                RenderError::Browser(
                    "no chromium binary found (set DRAWIO_AGENT_CHROMIUM_PATH)".into(),
                )
            })?;
        Self::launch_with(path).await
    }

    /// Launch the given Chromium binary and connect to its CDP endpoint.
    pub async fn launch_with<P: AsRef<Path>>(path: P) -> Result<Self, RenderError> {
        let path = path.as_ref();
        let temp_profile = tempdir()?;

        // Headless launch flags. Keychain-free flags are essential on macOS
        // so the browser never pops a certificate prompt.
        let mut cmd = Command::new(path);
        cmd.arg("--headless")
            .arg(format!("--user-data-dir={}", temp_profile.display()))
            .arg("--use-mock-keychain")
            .arg("--password-store=basic")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-sync")
            .arg("--remote-debugging-port=0")
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let mut child = cmd
            .spawn()
            .map_err(|e| RenderError::Browser(format!("spawn chromium: {e}")))?;
        let pid = child
            .id()
            .ok_or_else(|| RenderError::Browser("chromium exited before we got pid".into()))?;

        // Parse stderr for the DevTools ws:// URL.
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| RenderError::Browser("no stderr from chromium".into()))?;
        let (ws_tx, ws_rx) = tokio::sync::oneshot::channel::<String>();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(url) = parse_ws_url(&line) {
                    let _ = ws_tx.send(url);
                    return;
                }
            }
        });

        let ws_url = timeout(Duration::from_secs(10), ws_rx)
            .await
            .map_err(|_| RenderError::Browser("timed out waiting for ws url".into()))?
            .map_err(|_| RenderError::Browser("stderr task died".into()))?;
        stderr_task.abort();

        // Connect to the CDP WebSocket.
        let host_port = parse_host_port(&ws_url)?;
        let tcp = timeout(Duration::from_secs(10), TcpStream::connect(host_port.as_str()))
            .await
            .map_err(|_| RenderError::Browser("connect tcp timeout".into()))?
            .map_err(|e| RenderError::Browser(format!("connect tcp: {e}")))?;
        let (ws_stream, _) = timeout(
            Duration::from_secs(10),
            tokio_tungstenite::client_async(ws_url.as_str(), MaybeTlsStream::Plain(tcp)),
        )
        .await
        .map_err(|_| RenderError::Browser("ws handshake timeout".into()))?
        .map_err(|e| RenderError::Browser(format!("ws handshake: {e}")))?;
        let (mut write_half, mut read_half) = ws_stream.split();

        // Reader task: dispatch CDP responses by id, log events.
        let (out_tx, mut out_rx) = mpsc::channel::<Message>(64);
        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
        let pending_for_reader = pending.clone();
        let reader_task = tokio::spawn(async move {
            while let Some(msg) = read_half.next().await {
                match msg {
                    Ok(Message::Text(t)) => {
                        if let Ok(resp) = serde_json::from_str::<CdpResponse>(&t) {
                            if let Some(id) = resp.id {
                                let mut map = pending_for_reader.lock().await;
                                if let Some(tx) = map.remove(&id) {
                                    let value = if let Some(err) = resp.error {
                                        Err(RenderError::Page(err.message))
                                    } else {
                                        Ok(resp.result.unwrap_or(Value::Null))
                                    };
                                    let _ = tx.send(value);
                                }
                            } else if let Some(method) = resp.method {
                                debug!(target: "cdp", "event: {method}");
                            }
                        }
                    }
                    Ok(Message::Binary(_)) | Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                    Ok(Message::Close(_)) => break,
                    Err(e) => {
                        warn!(target: "cdp", "ws read error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
        });

        // Pump out_tx -> write_half.
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = write_half.send(msg).await {
                    warn!(target: "cdp", "ws write error: {e}");
                    break;
                }
            }
        });

        let inner = ChromiumInner {
            _child_pid: pid,
            profile_dir: temp_profile,
            write: out_tx,
            next_id: Mutex::new(1),
            pending,
            session_id: Mutex::new(None),
            _reader_task: reader_task,
            page_ready: std::sync::atomic::AtomicBool::new(false),
            render_lock: tokio::sync::Mutex::new(()),
        };
        let driver = Self {
            inner: Arc::new(inner),
        };

        // The DevTools ws endpoint from stderr is the BROWSER target: it
        // only speaks Browser/Target domains. Attach to the page target and
        // route all subsequent commands through its session so Page/Runtime/
        // Emulation work.
        let page_target = {
            let mut found = None;
            for _ in 0..10 {
                let targets = driver.send("Target.getTargets", None).await?;
                let infos = targets
                    .get("targetInfos")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                found = infos
                    .iter()
                    .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
                    .cloned();
                if found.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            found.ok_or_else(|| RenderError::Browser("no page target found".into()))?
        };
        let target_id = page_target
            .get("targetId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RenderError::Browser("page target missing targetId".into()))?;
        let attach = driver
            .send(
                "Target.attachToTarget",
                Some(json!({ "targetId": target_id, "flatten": true })),
            )
            .await?;
        let session_id = attach
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RenderError::Browser("attachToTarget missing sessionId".into()))?;
        *driver.inner.session_id.lock().await = Some(session_id.to_string());

        // Reap the child without awaiting (we kill it on Drop).
        tokio::spawn(async move {
            let _ = child.wait().await;
        });

        Ok(driver)
    }

    async fn send(&self, method: &str, params: Option<Value>) -> Result<Value, RenderError> {
        self.send_with_timeout(method, params, Duration::from_secs(15)).await
    }

    /// Like [`Self::send`] but with a caller-chosen timeout. The default
    /// 15s suits CDP control commands; long-running evaluates (a full
    /// diagram export) need their own budget — an inner timeout shorter
    /// than the caller's outer one would silently fire first.
    async fn send_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout_budget: Duration,
    ) -> Result<Value, RenderError> {
        let id = {
            let mut g = self.inner.next_id.lock().await;
            let id = *g;
            *g += 1;
            id
        };
        let session_id = self.inner.session_id.lock().await.clone();
        let req = CdpRequest {
            id,
            method: method.to_string(),
            params,
            session_id,
        };
        let text = serde_json::to_string(&req)
            .map_err(|e| RenderError::Browser(format!("encode: {e}")))?;

        // Register the response slot BEFORE sending to avoid a race with a
        // fast reply arriving before we're listening.
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut map = self.inner.pending.lock().await;
            map.insert(id, tx);
        }
        self.inner
            .write
            .send(Message::Text(text))
            .await
            .map_err(|e| RenderError::Browser(format!("ws send: {e}")))?;

        let resp = timeout(timeout_budget, rx)
            .await
            .map_err(|_| RenderError::Page(format!("{method}: timeout")))?
            .map_err(|_| RenderError::Browser("response channel dropped".into()))??;
        Ok(resp)
    }
}

#[async_trait]
impl RenderDriver for HeadlessChromiumDriver {
    async fn render(&self, xml: &str, opts: &RenderOptions) -> Result<Vec<u8>, RenderError> {
        if xml.trim().is_empty() {
            return Err(RenderError::Xml("empty xml".into()));
        }

        // 渲染串行化（同进程共享一个热页面）。
        let _guard = self.inner.render_lock.lock().await;

        // 1. 首次：导航到 wrapper 页并等应用就绪；之后进程/页面常驻，
        //    每次渲染只换 xml + 导出（省掉进程启动与应用加载）。
        if !self.inner.page_ready.load(std::sync::atomic::Ordering::Acquire) {
            let url = crate::driver::drawio_server::export_page_url();
            self.send("Page.enable", None).await?;
            self.send("Page.navigate", Some(json!({ "url": url }))).await?;
            let mut ready = false;
            for _ in 0..600 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                match self
                    .send(
                        "Runtime.evaluate",
                        Some(json!({
                            "expression": "window.__ready === true",
                            "returnByValue": true,
                        })),
                    )
                    .await
                {
                    Ok(r) => {
                        if r.get("result")
                            .and_then(|v| v.get("value"))
                            .and_then(|v| v.as_bool())
                            == Some(true)
                        {
                            ready = true;
                            break;
                        }
                    }
                    Err(_) => continue,
                }
            }
            if !ready {
                return Err(RenderError::Page(
                    "drawio webapp did not become ready (offline? app not cached?)".into(),
                ));
            }
            self.inner
                .page_ready
                .store(true, std::sync::atomic::Ordering::Release);
        }

        // 2. 热路径渲染：__doRender 返回 Promise，CDP awaitPromise 等它
        //    在应用内完成 load + export。超时预算给足（大图导出慢于
        //    控制命令）；超时/失败都尽力带回页面诊断。
        let bg = if opts.background.is_empty() {
            "#ffffff"
        } else {
            &opts.background
        };
        let expr = format!(
            "window.__doRender({xml}, {scale}, {border}, {bg}, {annotate}, {focus})",
            xml = serde_json::Value::String(xml.to_string()),
            scale = opts.scale,
            border = opts.border,
            bg = serde_json::Value::String(bg.to_string()),
            annotate = opts.annotate,
            focus = serde_json::Value::Array(
                opts.focus.iter().map(|f| serde_json::Value::String(f.clone())).collect()
            ),
        );
        let result = match self
            .send_with_timeout(
                "Runtime.evaluate",
                Some(json!({
                    "expression": expr,
                    "awaitPromise": true,
                    "returnByValue": true,
                })),
                RENDER_EVALUATE_TIMEOUT,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let diag = self
                    .send(
                        "Runtime.evaluate",
                        Some(json!({
                            "expression": "JSON.stringify(window.__diag || [])",
                            "returnByValue": true,
                        })),
                    )
                    .await
                    .ok()
                    .and_then(|r| {
                        r.get("result")
                            .and_then(|v| v.get("value"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| "(no diag)".into());
                return Err(RenderError::Page(format!(
                    "drawio export failed: {e}. page diag: {diag}"
                )));
            }
        };
        if let Some(exception) = result.get("exceptionDetails") {
            if !exception.is_null() {
                return Err(RenderError::Xml(format!("__doRender threw: {exception}")));
            }
        }
        let value = result
            .get("result")
            .and_then(|v| v.get("value"))
            .cloned()
            .unwrap_or(json!({}));
        let png_b64 = value
            .get("png")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                let msg = value
                    .get("err")
                    .and_then(|v| v.as_str())
                    .unwrap_or("no png in export result");
                RenderError::Export(msg.to_string())
            })?;
        let png = base64::engine::general_purpose::STANDARD
            .decode(png_b64.trim())
            .map_err(|e| RenderError::Export(format!("base64 decode: {e}")))?;
        if png.len() < 8 || &png[..4] != b"\x89PNG" {
            return Err(RenderError::Export("export data is not a PNG".into()));
        }
        info!(target: "renderer", bytes = png.len(), "drawio export ok");
        Ok(png)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Chromium prints: `DevTools listening on ws://127.0.0.1:12345/devtools/...`
fn parse_ws_url(line: &str) -> Option<String> {
    let idx = line.find("ws://")?;
    let rest = &line[idx..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

fn parse_host_port(ws_url: &str) -> Result<String, RenderError> {
    let stripped = ws_url.strip_prefix("ws://").unwrap_or(ws_url);
    let end = stripped.find('/').unwrap_or(stripped.len());
    Ok(stripped[..end].to_string())
}

fn tempdir() -> Result<PathBuf, RenderError> {
    let dir = std::env::temp_dir().join(format!(
        "drawio-agent-chromium-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|e| RenderError::Browser(format!("tempdir: {e}")))?;
    Ok(dir)
}