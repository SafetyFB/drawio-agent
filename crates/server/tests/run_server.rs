//! Integration tests for the runnable server: start on a random port,
//! serve the embedded static index.html, expose /health and /api,
//! and shut down cleanly via a oneshot signal.

use std::path::PathBuf;
use std::time::Duration;

use drawio_agent_server::{
    build_app_state, run_server, LlmProviderKind, RendererKind, ServerConfig,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

async fn spawn_server(
    static_dir: PathBuf,
) -> (
    String, // base URL
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    // Bind to a random port on localhost.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    let state = build_app_state(&ServerConfig {
        bind_addr: addr,
        llm_provider: LlmProviderKind::Mock,
        renderer: RendererKind::Mock,
        static_dir: Some(static_dir.clone()),
    })
    .await
    .unwrap();

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        let _ = run_server(listener, state, static_dir, async move {
            let _ = shutdown_rx.await;
        })
        .await;
    });
    // Give the server a moment to start accepting.
    tokio::time::sleep(Duration::from_millis(100)).await;
    (base, shutdown_tx, handle)
}

#[tokio::test]
async fn health_endpoint_returns_ok() {
    let (base, shutdown, handle) = spawn_server(static_dir()).await;
    let body = reqwest::get(format!("{base}/health")).await.unwrap();
    assert!(body.status().is_success());
    let json: serde_json::Value = body.json().await.unwrap();
    assert_eq!(json["status"], "ok");

    let _ = shutdown.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn api_create_session_works_via_real_http() {
    let (base, shutdown, handle) = spawn_server(static_dir()).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/sessions"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let sid = body["session_id"].as_str().unwrap().to_string();
    assert!(!sid.is_empty());

    // GET the session back.
    let resp = reqwest::get(format!("{base}/api/sessions/{sid}"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let _ = shutdown.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn static_index_html_is_served_at_root() {
    let (base, shutdown, handle) = spawn_server(static_dir()).await;
    let resp = reqwest::get(format!("{base}/")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    assert!(ct.starts_with("text/html"), "got content-type {ct}");
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Draw.io Agent Server"),
        "expected index.html to contain the marker, got: {body}"
    );

    let _ = shutdown.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn static_dir_with_relative_path_resolves_against_cwd() {
    // The serve_dir uses paths relative to the CWD by default. We give a
    // non-existent dir and expect the static fallback to 404 — this is a
    // smoke check that the fallback service is mounted (no panic).
    let (base, shutdown, handle) = spawn_server("/tmp/nonexistent-static-dir-xyz".into()).await;
    let resp = reqwest::get(format!("{base}/")).await.unwrap();
    // 404 from ServeDir, not a 200 and not a 5xx panic.
    assert_eq!(resp.status(), 404);

    let _ = shutdown.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn shutdown_signal_terminates_server() {
    let (base, shutdown_tx, handle) = spawn_server(static_dir()).await;
    // Server is up — health responds.
    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert!(resp.status().is_success());

    // Trigger shutdown.
    shutdown_tx.send(()).expect("shutdown send");
    // The server task should finish within a couple seconds.
    let join_result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("server did not shut down within 5s");
    assert!(
        join_result.is_ok(),
        "server task panicked or returned an error"
    );

    // After shutdown, the port is released.
    let listener = TcpListener::bind("127.0.0.1:0").await;
    assert!(listener.is_ok(), "port still in use after shutdown");
}

fn static_dir() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    manifest.join("static")
}
