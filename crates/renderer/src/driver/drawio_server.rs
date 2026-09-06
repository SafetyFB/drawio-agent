//! In-process static server for headless rendering: serves the drawio
//! webapp (from the agent cache) plus a tiny export-wrapper page that
//! embeds the app in an iframe and runs the native `export` postMessage
//! protocol. The headless browser navigates to the wrapper with the xml
//! in the URL fragment; no per-call server state.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::OnceLock;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::drawio_app::ensure_drawio_app;
use crate::RenderOptions;

static PORT: OnceLock<u16> = OnceLock::new();

const WRAPPER_HTML: &str = r#"<!DOCTYPE html><html><body style="margin:0">
<iframe id="f" style="position:fixed;inset:0;width:100%;height:100%;border:0"
  src="/drawio/index.html?embed=1&proto=json&spin=1&modified=unsavedChanges&keepmodified=1&noSaveBtn=1&saveAndExit=0"></iframe>
<script>
(function () {
  var params = {};
  try {
    var h = decodeURIComponent(location.hash.slice(1));
    var b = h.replace(/-/g, '+').replace(/_/g, '/');
    while (b.length % 4) b += '=';
    params = JSON.parse(decodeURIComponent(escape(atob(b))));
  } catch (e) {}
  var f = document.getElementById('f');
  var loaded = false;
  window.addEventListener('message', function (ev) {
    var d = ev.data;
    try { if (typeof d === 'string') d = JSON.parse(d); } catch (e) {}
    if (!d || typeof d !== 'object') return;
    if (d.event === 'init' && !loaded) {
      loaded = true;
      window.__ready = true;
      f.contentWindow.postMessage(JSON.stringify({ action: 'load', autosave: 1, xml: params.xml }), '*');
    }
    if (d.event === 'export') {
      var data = d.data || '';
      var m = data.match(/^data:image\/png;base64,([\s\S]*)$/);
      window.__exportPng = m ? m[1] : data;
      window.__exportDone = true;
    }
  });
  window.__doExport = function () {
    f.contentWindow.postMessage(JSON.stringify({
      action: 'export', format: 'png', xml: params.xml,
      scale: params.scale, border: params.border, background: params.background
    }), '*');
  };
})();
</script></body></html>"#;

/// Fragment-encode render parameters (base64url of the JSON payload).
fn frag(params: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(params).unwrap_or_default();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// URL of the export wrapper for the given xml/opts.
pub fn export_url(xml: &str, opts: &RenderOptions) -> String {
    let params = serde_json::json!({
        "xml": xml,
        "scale": opts.scale,
        "border": opts.border,
        "background": opts.background,
    });
    format!("http://127.0.0.1:{}/__harness_export.html#{}", port(), frag(&params))
}

fn port() -> u16 {
    *PORT.get_or_init(|| {
        std::thread::spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("static server runtime");
            rt.block_on(run_server());
        });
        // Port is published by the server thread once bound. The first
        // caller blocks briefly until available.
        let mut tries = 0;
        loop {
            let p = SERVER_PORT.load(std::sync::atomic::Ordering::Acquire);
            if p != 0 {
                return p;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            tries += 1;
            if tries > 250 {
                panic!("drawio static server did not bind");
            }
        }
    })
}

static SERVER_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

async fn run_server() {
    let Some(app_dir) = ensure_drawio_app().unwrap_or_else(|e| {
        eprintln!("drawio renderer: webapp 不可用: {e}");
        None
    }) else {
        // 无 app：仍启动一个空服务器，所有请求 503（driver 侧报清晰错误）
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        SERVER_PORT.store(listener.local_addr().unwrap().port(), std::sync::atomic::Ordering::Release);
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = sock.write_all(b"HTTP/1.1 503 drawio webapp unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            });
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    SERVER_PORT.store(addr.port(), std::sync::atomic::Ordering::Release);
    loop {
        let (sock, _) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => continue,
        };
        let dir = app_dir.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(sock, &dir).await {
                eprintln!("drawio static: {e}");
            }
        });
    }
}

async fn handle_conn(
    mut sock: tokio::net::TcpStream,
    dir: &Path,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            return Ok(());
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let mut lines = head.lines();
    let req = lines.next().unwrap_or("");
    let mut parts = req.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");
    if method != "GET" {
        return respond(&mut sock, 405, "application/octet-stream", &[]).await;
    }
    let path = path.split('?').next().unwrap_or("/");
    if path == "/__harness_export.html" {
        return respond(&mut sock, 200, "text/html", WRAPPER_HTML.as_bytes()).await;
    }
    let rel = path.trim_start_matches("/drawio/");
    if path == "/drawio" || path == "/drawio/" || rel.is_empty() {
        return respond(&mut sock, 200, "text/html", &std::fs::read(dir.join("index.html")).unwrap_or_default()).await;
    }
    if rel.contains("..") || rel.contains('\\') {
        return respond(&mut sock, 400, "text/plain", b"bad path").await;
    }
    let full = dir.join(rel);
    let mime = mime_of(&full);
    match std::fs::read(&full) {
        Ok(bytes) => respond(&mut sock, 200, mime, &bytes).await,
        Err(_) => respond(&mut sock, 404, "text/plain", b"not found").await,
    }
}

fn mime_of(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html",
        "js" => "text/javascript",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "json" | "map" => "application/json",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

async fn respond(
    sock: &mut tokio::net::TcpStream,
    status: u16,
    mime: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body).await?;
    Ok(())
}
