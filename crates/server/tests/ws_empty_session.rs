//! TDD: WebSocket must stay open for a session that has no events yet.
//!
//! Bug B repro: a freshly-created session (POST /api/sessions, no events
//! ever emitted) used to close the WS connection immediately.

use std::path::PathBuf;
use std::time::Duration;

use drawio_agent_server::{build_app_state, run_server, LlmProviderKind, RendererKind, ServerConfig};
use futures_util::StreamExt;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

async fn spawn_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    let state = build_app_state(&ServerConfig {
        bind_addr: addr,
        llm_provider: LlmProviderKind::Mock,
        renderer: RendererKind::Mock,
        static_dir: Some(manifest.join("static")),
    })
    .await
    .unwrap();

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = run_server(listener, state, PathBuf::from("."), async move {
            let _ = shutdown_rx.await;
        })
        .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::mem::forget(shutdown_tx); // keep the shutdown channel alive for the whole test
    base
}

#[tokio::test]
async fn ws_stays_open_on_empty_session() {
    let base = spawn_server().await;

    // Create an empty session (no initial XML, no events).
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/sessions"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "session create status");
    let body: serde_json::Value = resp.json().await.unwrap();
    let sid = body["session_id"].as_str().unwrap();

    let ws_url = format!("ws://127.0.0.1:8080/api/sessions/{sid}/events");
    // Derive the actual ws url from the http base.
    let ws_url = ws_url.replace("127.0.0.1:8080", base.trim_start_matches("http://"));
    eprintln!("connecting to {ws_url}");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();

    // No events should ever arrive on an empty session. If the server keeps
    // the connection open, next() blocks → we time out. If it closes, we get
    // Close/Err within the window.
    let result = tokio::time::timeout(Duration::from_millis(500), ws.next()).await;
    match result {
        Err(_elapsed) => {
            eprintln!("WS stayed open for 500ms (expected)");
        }
        Ok(Some(Ok(Message::Close(_)))) => {
            panic!("BUG B REPRODUCED: server closed the WS immediately on empty session")
        }
        Ok(Some(Ok(other))) => {
            panic!("unexpected message on empty session: {other:?}")
        }
        Ok(Some(Err(e))) => {
            panic!("BUG B REPRODUCED: ws read error: {e}")
        }
        Ok(None) => {
            panic!("BUG B REPRODUCED: ws stream ended on empty session")
        }
    }
}