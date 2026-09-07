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

/// 导出侧插件：徽章标注（碰撞避让）+ 裁剪矩形信息 + overlay XML 回传。
/// headless 页面专用（renderer 内部静态服务器提供；用户浏览器用的是
/// harness 侧的 sel 桥插件）。
pub const EXPORT_PLUGIN_JS: &str = r#"
Draw.loadPlugin(function (ui) {
  var g = ui.editor.graph;
  var overlays = [];
  window.addEventListener('message', function (ev) {
    var d = ev.data;
    try { if (typeof d === 'string') d = JSON.parse(d); } catch (e) {}
    if (!d || typeof d !== 'object') return;
    if (d.event) diag('msg:' + d.event);
    if (d.action === 'export_annotate') {
      var crop = null;
      var ids = d.ids || [];
      if (ids.length) {
        var minX = 1e12, minY = 1e12, maxX = -1e12, maxY = -1e12;
        for (var i = 0; i < ids.length; i++) {
          var cell = g.model.getCell(ids[i]);
          var st = cell ? g.view.getState(cell) : null;
          if (st) {
            minX = Math.min(minX, st.x); minY = Math.min(minY, st.y);
            maxX = Math.max(maxX, st.x + st.width); maxY = Math.max(maxY, st.y + st.height);
          }
        }
        if (minX < 1e12) {
          var M = 24;
          var gb = g.getGraphBounds();
          crop = { gx: gb.x, gy: gb.y, gw: gb.width, gh: gb.height,
                   fx: minX - M, fy: minY - M, fw: maxX - minX + 2 * M, fh: maxY - minY + 2 * M };
        }
      }
      if (d.annotate) {
        var placed = [];
        var allowed = d.allowed || null;
        var cap = 200;
        var walk = function (cell) {
          if (cell && cell.id !== '0' && cell.id !== '1' && g.model.isVertex(cell)
              && placed.length < cap
              && (!allowed || allowed.indexOf(cell.id) >= 0)) {
            var st = g.view.getState(cell);
            if (st && st.width > 0) {
              var label = cell.id.length > 10 ? cell.id.slice(0, 9) + '…' : cell.id;
              var bw = Math.max(24, label.length * 5.5 + 6), bh = 12;
              var x0 = st.x - 4, y0 = st.y - 12;
              var placedAt = null;
              for (var k = 0; k < 6; k++) {
                var bx = x0 + k * 14, by = y0;
                var hit = false;
                for (var p = 0; p < placed.length; p++) {
                  var r = placed[p];
                  if (bx < r.x + r.w && r.x < bx + bw && by < r.y + r.h && r.y < by + bh) { hit = true; break; }
                }
                if (!hit) { placedAt = [bx, by]; break; }
              }
              if (placedAt) {
                placed.push({ x: placedAt[0], y: placedAt[1], w: bw, h: bh });
                var s = g.view.scale, t = g.view.translate;
                // 关键：与被标注 cell 同父（getDefaultParent 是图层 cell，
                // 自带几何偏移——徽章会被整体平移错位）
                var pcell = cell.parent || g.getDefaultParent();
                var ov = g.insertVertex(
                  pcell, null, label,
                  (placedAt[0] - t.x) / s, (placedAt[1] - t.y) / s, bw / s, bh / s,
                  'text;html=1;align=left;verticalAlign=top;fontSize=9;fontColor=#D32F2F;fillColor=none;strokeColor=none;spacing=0;'
                );
                overlays.push(ov);
              }
            }
          }
          if (cell) for (var i = 0; i < g.model.getChildCount(cell); i++) walk(g.model.getChildAt(cell, i));
        };
        walk(g.model.getRoot());
        g.view.revalidate();
      }
      if (crop) {
        // 徽章可能高于 gb 顶边，扩出标注区保证裁剪映射正确
        crop.gy -= 16; crop.gh += 16;
      }
      parent.postMessage({ event: 'export_annotate_ready', crop: crop }, '*');
    }
    if (d.action === 'export_annotate_clear') {
      for (var i = 0; i < overlays.length; i++) g.model.remove(overlays[i]);
      overlays = [];
    }
  });
});
"#;

const WRAPPER_HTML: &str = r#"<!DOCTYPE html><html><body style="margin:0">
<iframe id="f" style="position:fixed;inset:0;width:100%;height:100%;border:0"
  src="/drawio/index.html?embed=1&proto=json&spin=1&modified=unsavedChanges&keepmodified=1&noSaveBtn=1&saveAndExit=0"></iframe>
<script>
(function () {
  // 注册导出插件（徽章/裁剪信息桥）。headless 专用浏览器 profile，
  // 与用户浏览器的 localStorage 完全隔离。
  try {
    var key = '.drawio-config';
    var cfg = {};
    try { cfg = JSON.parse(localStorage.getItem(key) || '{}'); } catch (e) {}
    var plugins = (cfg.plugins || []).filter(function (u) { return u !== '/drawio-export-plugin.js'; });
    plugins.push('/drawio-export-plugin.js');
    localStorage.setItem(key, JSON.stringify(Object.assign({}, cfg, { plugins: plugins })));
  } catch (e) {}
  window.__diag = [];
  var diag = function (m) { window.__diag.push(String(m).slice(0, 200)); };
  window.onerror = function (m, src, line) { diag('window.onerror: ' + m + ' @' + src + ':' + line); };
  var params = {};
  try {
    var h = decodeURIComponent(location.hash.slice(1));
    var b = h.replace(/-/g, '+').replace(/_/g, '/');
    while (b.length % 4) b += '=';
    params = JSON.parse(decodeURIComponent(escape(atob(b))));
  } catch (e) {}
  var f = document.getElementById('f');
  var booted = false;
  var pendingRun = null;
  var pendingResolve = null;
  window.addEventListener('message', function (ev) {
    var d = ev.data;
    try { if (typeof d === 'string') d = JSON.parse(d); } catch (e) {}
    if (!d || typeof d !== 'object') return;
    if (d.event) diag('msg:' + d.event);
    if (d.event === 'init') {
      booted = true;
      window.__ready = true;
      if (pendingRun) { var r = pendingRun; pendingRun = null; r(); }
      else if (params.xml) {
        f.contentWindow.postMessage(JSON.stringify({ action: 'load', autosave: 1, xml: params.xml }), '*');
      }
    }
    if (d.event === 'export_annotate_ready') {
      window.__crop = d.crop || null;
      window.__overlayXmls = d.overlayXmls || [];
      diag('annotate_ready: overlays=' + window.__overlayXmls.length + ' crop=' + JSON.stringify(window.__crop));
      if (d.badgeModels && d.badgeModels.length) {
        var bm = d.badgeModels[0];
        diag('badge model=' + bm.x + ',' + bm.y + ' view_s=' + bm.s + ' view_t=' + bm.tx + ',' + bm.ty);
      }
      if (window.__overlayXmls.length) diag('overlay[0]=' + window.__overlayXmls[0].slice(0, 150));
    }
    if (d.event === 'export') {
      var data = d.data || '';
      var m = data.match(/^data:image\/png;base64,([\s\S]*)$/);
      window.__exportPng = m ? m[1] : data;
      window.__exportDone = true;
      if (pendingResolve) { var rs = pendingResolve; pendingResolve = null; rs({ ok: true, png: window.__exportPng }); }
    }
  });
  // 热路径：应用常驻，重复渲染只换 xml + 导出，不再重启 iframe
  window.__doRender = function (xml, scale, border, background, annotate, focusIds) {
    window.__exportDone = false;
    window.__exportPng = undefined;
    window.__crop = null;
    var cropNeeded = !!(focusIds && focusIds.length);
    var run = function () {
      f.contentWindow.postMessage(JSON.stringify({ action: 'load', autosave: 1, xml: xml }), '*');
      setTimeout(function () {
        // 徽章/裁剪信息桥：插件返回 crop 矩形（与导出图同一坐标空间的比例）
        var allowed = [];
        try {
          var idRe = /\bid="([^"]+)"/g, m2;
          while ((m2 = idRe.exec(xml))) allowed.push(m2[1]);
        } catch (e) {}
        f.contentWindow.postMessage(JSON.stringify({
          action: 'export_annotate', annotate: !!annotate, ids: focusIds || [],
          allowed: allowed
        }), '*');
        setTimeout(function () {
          // 导出 live model（徽章 overlay 已插入模型，无需 xml 注入——
          // 探针实测：无 xml 参数的 export 即导出当前模型，几何精确）。
          // 无 xml 时协议回退到模型内容，此前 timeout 是 cropToPng
          // Promise bug 的同期假象。
          var effBorder = cropNeeded ? 0 : (annotate ? Math.max(border, 24) : border);
          f.contentWindow.postMessage(JSON.stringify({
            action: 'export', format: 'png',
            scale: scale, border: effBorder, background: background
          }), '*');
        }, 300);
      }, 600);
    };
    var cropToPng = function (b64) {
      if (!window.__crop) return Promise.resolve(b64);
      var img = new Image();
      img.src = 'data:image/png;base64,' + b64;
      return new Promise(function (res) {
        img.onload = function () {
          var W = img.width, H = img.height;
          var c = window.__crop;
          // crop 矩形以图元 bbox 的比例给出
          var x = Math.max(0, Math.round((c.fx - c.gx) / c.gw * W));
          var y = Math.max(0, Math.round((c.fy - c.gy) / c.gh * H));
          var w = Math.min(W - x, Math.round(c.fw / c.gw * W));
          var h = Math.min(H - y, Math.round(c.fh / c.gh * H));
          var cv = document.createElement('canvas');
          cv.width = w; cv.height = h;
          cv.getContext('2d').drawImage(img, x, y, w, h, 0, 0, w, h);
          res(cv.toDataURL('image/png').split(',')[1]);
        };
        img.onerror = function () { res(b64); };
      });
    };
    return new Promise(function (resolve) {
      pendingResolve = function (r) {
        cropToPng(r.png).then(function (png) {
          f.contentWindow.postMessage(JSON.stringify({ action: 'export_annotate_clear' }), '*');
          resolve({ ok: true, png: png });
        }, function (e) {
          resolve({ ok: false, err: 'crop failed: ' + (e && e.message ? e.message : e) });
        });
      };
      var runSafely = function () {
        try { run(); } catch (e) {
          diag('run failed: ' + (e && e.message ? e.message : e));
          resolve({ ok: false, err: 'run failed: ' + (e && e.message ? e.message : e) });
        }
      };
      if (booted) runSafely();
      else pendingRun = runSafely;
      // 兜底：8s 无 export 事件 → 带诊断报错（低于驱动 CDP 15s 超时）
      setTimeout(function () {
        if (pendingResolve) {
          pendingResolve = null;
          resolve({ ok: false, err: 'no export event within 8s; diag=' + window.__diag.join(' | ') });
        }
      }, 8000);
    });
  };
})();
</script></body></html>"#;

/// Base URL of the export wrapper (xml travels via CDP evaluate, not the
/// URL — no length limits, and the page stays hot across renders).
pub fn export_page_url() -> String {
    format!("http://127.0.0.1:{}/__harness_export.html", port())
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
    if path == "/drawio-export-plugin.js" {
        return respond(&mut sock, 200, "text/javascript", EXPORT_PLUGIN_JS.as_bytes()).await;
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
