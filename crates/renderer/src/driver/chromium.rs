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

use crate::{RenderDriver, RenderError, RenderOptions};

// ---------------------------------------------------------------------------
// Binary discovery
// ---------------------------------------------------------------------------

/// Resolve the Chromium binary path. Looks at (in order):
/// 1. `DRAWIO_AGENT_CHROMIUM_PATH` env var
/// 2. Common macOS/Linux app locations
/// 3. `$PATH` via `which()`
pub fn find_chromium() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DRAWIO_AGENT_CHROMIUM_PATH") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let candidates = [
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    which("chromium").or_else(|| which("google-chrome"))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
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
    write: mpsc::Sender<Message>,
    next_id: Mutex<u64>,
    /// Pending request id -> oneshot response sender.
    pending: Arc<Mutex<PendingMap>>,
    /// Page-session id after `Target.attachToTarget`; `None` before then.
    session_id: Mutex<Option<String>>,
    /// Directory containing `render.html` (and the viewer bundle).
    assets_dir: PathBuf,
    /// Background color used when `RenderOptions.background` is empty.
    default_background: String,
    /// Keeps the WebSocket reader task alive.
    _reader_task: tokio::task::JoinHandle<()>,
}

impl Drop for ChromiumInner {
    fn drop(&mut self) {
        // Best-effort kill; not in async context here.
        let _ = std::process::Command::new("kill")
            .arg(self._child_pid.to_string())
            .output();
    }
}

impl HeadlessChromiumDriver {
    /// Locate a Chromium binary and launch it.
    pub async fn launch() -> Result<Self, RenderError> {
        let path = find_chromium().ok_or_else(|| {
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

        // Compile-time manifest dir: embeds the RENDERER crate's path, so the
// driver finds render.html regardless of which binary is running (a
// runtime env!() would be overridden by the host crate).
let assets_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");

        let inner = ChromiumInner {
            _child_pid: pid,
            write: out_tx,
            next_id: Mutex::new(1),
            pending,
            session_id: Mutex::new(None),
            assets_dir,
            default_background: "#ffffff".into(),
            _reader_task: reader_task,
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

        let resp = timeout(Duration::from_secs(15), rx)
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

        let render_html = self.inner.assets_dir.join("render.html");
        let url = url::Url::from_file_path(&render_html)
            .map_err(|_| {
                RenderError::Browser(format!("bad assets path: {}", render_html.display()))
            })?;

        // 1. Navigate to render.html.
        self.send("Page.enable", None).await?;
        self.send("Page.navigate", Some(json!({ "url": url.as_str() }))).await?;

        // 2. Wait for the page to load and define window.renderXml.
        let mut loaded = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            match self
                .send(
                    "Runtime.evaluate",
                    Some(json!({
                        "expression": "typeof window.renderXml !== 'undefined'",
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
                        loaded = true;
                        break;
                    }
                }
                // Navigation in progress: context may be gone; keep polling.
                Err(_) => continue,
            }
        }
        if !loaded {
            return Err(RenderError::Page(
                "render.html did not define window.renderXml".into(),
            ));
        }

        // 3. Call window.renderXml synchronously.
        let bg = if opts.background.is_empty() {
            &self.inner.default_background
        } else {
            &opts.background
        };
        let expr = format!(
            r#"window.renderXml({xml_json}, {scale}, {bg_json}, {border})"#,
            xml_json = serde_json::Value::String(xml.to_string()),
            scale = opts.scale,
            bg_json = serde_json::Value::String(bg.clone()),
            border = opts.border,
        );
        let result = self
            .send(
                "Runtime.evaluate",
                Some(json!({
                    "expression": expr,
                    "awaitPromise": false,
                    "returnByValue": true,
                })),
            )
            .await?;
        if let Some(exception) = result.get("exceptionDetails") {
            if !exception.is_null() {
                return Err(RenderError::Xml(format!("renderXml threw: {exception}")));
            }
        }
        let value = result
            .get("result")
            .and_then(|v| v.get("value"))
            .cloned()
            .unwrap_or(json!({}));
        if value.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let msg = value
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("renderXml reported failure");
            return Err(RenderError::Xml(msg.to_string()));
        }
        let w = value.get("width").and_then(|v| v.as_u64()).unwrap_or(800);
        let h = value.get("height").and_then(|v| v.as_u64()).unwrap_or(600);

        // 4. Size the viewport to the diagram + border so the screenshot
        //    captures exactly the rendered content.
        self.send(
            "Emulation.setDeviceMetricsOverride",
            Some(json!({
                "width": w + 2 * opts.border as u64,
                "height": h + 2 * opts.border as u64,
                "deviceScaleFactor": opts.scale,
                "mobile": false,
            })),
        )
        .await?;

        // Give the compositor a beat to paint after the resize.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 5. Capture PNG.
        let capture = self
            .send("Page.captureScreenshot", Some(json!({ "format": "png" })))
            .await?;
        let b64 = capture
            .get("data")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RenderError::Export("captureScreenshot missing 'data'".into()))?;
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
            .map_err(|e| RenderError::Export(format!("base64 decode: {e}")))?;

        // 6. PNG signature check.
        const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        if bytes.len() < 8 || bytes[..8] != PNG_SIG {
            return Err(RenderError::Export(format!(
                "screenshot is not PNG (got {} bytes)",
                bytes.len()
            )));
        }
        info!(target: "renderer", bytes = bytes.len(), "chromium render ok");
        Ok(bytes)
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