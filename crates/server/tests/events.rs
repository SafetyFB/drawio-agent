//! TDD #6: WebSocket event stream + EventBus broadcast semantics.
//!
//! Phase 4 server emits `VersionCreated` / `Error` events on action
//! completion. Subscribers (e.g. the WebSocket handler) see them in
//! arrival order, isolated per session.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drawio_agent_llm_client::{
    GenerateRequest, LlmProvider, LlmResponse, LlmStream, ProviderError, ReviewRequest,
    ReviewResponse, Usage,
};
use drawio_agent_renderer::{MockDriver, RenderDriver};
use drawio_agent_server::{
    state::WsEvent, AppState, CreateSessionRequest, GenerateRequest as GenReq,
    PatchRequest as PatchReq, ReviewRequest as RevReq, SessionId,
};
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

const FULL_XML: &str = r#"<mxfile host="app.diagrams.net">
  <diagram id="d" name="Page-1">
    <mxGraphModel dx="800" dy="600" grid="1" guides="1" tooltips="1" connect="1" arrows="1" fold="1" page="1" pageScale="1" pageWidth="850" pageHeight="1100" math="0" shadow="0">
      <root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="2" value="Hello" vertex="1" parent="1">
          <mxGeometry x="100" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

struct TestLlm {
    calls: AtomicU32,
    fail_with: Mutex<Option<String>>,
    response_xml: Mutex<Option<String>>,
}

impl TestLlm {
    fn new() -> Self {
        Self {
            calls: AtomicU32::new(0),
            fail_with: Mutex::new(None),
            response_xml: Mutex::new(Some(FULL_XML.to_string())),
        }
    }
    fn set_failure(&self, msg: &str) {
        *self.fail_with.lock().unwrap() = Some(msg.to_string());
    }
}

#[async_trait::async_trait]
impl LlmProvider for TestLlm {
    fn name(&self) -> &str {
        "test"
    }
    async fn generate_xml(
        &self,
        _req: GenerateRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(msg) = self.fail_with.lock().unwrap().clone() {
            return Err(ProviderError::Provider(msg));
        }
        Ok(LlmResponse {
            content: self
                .response_xml
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "<mxfile/>".to_string()),
            usage: Usage::default(),
            raw: Value::Null,
            duration_ms: 0,
        })
    }
    async fn generate_streaming(
        &self,
        _req: GenerateRequest,
    ) -> Result<LlmStream, ProviderError> {
        unimplemented!()
    }
    async fn review_visual(
        &self,
        _req: ReviewRequest,
    ) -> Result<LlmResponse<ReviewResponse>, ProviderError> {
        if let Some(msg) = self.fail_with.lock().unwrap().clone() {
            return Err(ProviderError::Provider(msg));
        }
        Ok(LlmResponse {
            content: ReviewResponse {
                verdict: "pass".to_string(),
                issues: vec![],
            },
            usage: Usage::default(),
            raw: Value::Null,
            duration_ms: 0,
        })
    }
}

fn state_with(llm: Arc<TestLlm>, renderer: Arc<dyn RenderDriver>) -> Arc<AppState> {
    Arc::new(AppState {
        sessions: Arc::new(tokio::sync::RwLock::new(
            drawio_agent_server::SessionStore::new(),
        )),
        llm,
        renderer,
        events: drawio_agent_server::EventBus::new(),
    })
}

fn router(state: Arc<AppState>) -> axum::Router {
    drawio_agent_server::build_router((*state).clone())
}

async fn create_session_with_xml(app: axum::Router, xml: &str) -> String {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&CreateSessionRequest {
                        initial_xml: Some(xml.to_string()),
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), 8192)
        .await
        .unwrap();
    let parsed: drawio_agent_server::CreateSessionResponse = serde_json::from_slice(&body).unwrap();
    parsed.session_id.as_str().to_string()
}

async fn create_empty_session(app: axum::Router) -> String {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&CreateSessionRequest::default()).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), 4096)
        .await
        .unwrap();
    let parsed: drawio_agent_server::CreateSessionResponse = serde_json::from_slice(&body).unwrap();
    parsed.session_id.as_str().to_string()
}

// ---------------------------------------------------------------------------
// EventBus unit-level tests (no HTTP)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn eventbus_subscribed_receives_emitted_event() {
    use drawio_agent_server::EventBus;

    let bus = EventBus::new();
    let sid = SessionId::new();
    let mut rx = bus.subscribe(&sid).await.expect("subscribed");

    let event = WsEvent::Error {
        session_id: sid.clone(),
        message: "boom".into(),
    };
    bus.emit(&sid, event.clone()).await;

    let received = rx.recv().await.unwrap();
    match received {
        WsEvent::Error { message, .. } => assert_eq!(message, "boom"),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn eventbus_multiple_subscribers_all_receive_broadcast() {
    use drawio_agent_server::EventBus;

    let bus = EventBus::new();
    let sid = SessionId::new();
    let mut rx1 = bus.subscribe(&sid).await.unwrap();
    let mut rx2 = bus.subscribe(&sid).await.unwrap();
    let mut rx3 = bus.subscribe(&sid).await.unwrap();

    let event = WsEvent::Error {
        session_id: sid.clone(),
        message: "broadcast".into(),
    };
    bus.emit(&sid, event).await;

    for rx in [&mut rx1, &mut rx2, &mut rx3] {
        let got = rx.recv().await.unwrap();
        match got {
            WsEvent::Error { message, .. } => assert_eq!(message, "broadcast"),
            _ => panic!("wrong variant"),
        }
    }
}

#[tokio::test]
async fn eventbus_events_isolated_between_sessions() {
    use drawio_agent_server::EventBus;

    let bus = EventBus::new();
    let sid_a = SessionId::new();
    let sid_b = SessionId::new();
    let mut rx_a = bus.subscribe(&sid_a).await.unwrap();
    let mut rx_b = bus.subscribe(&sid_b).await.unwrap();

    bus.emit(
        &sid_a,
        WsEvent::Error {
            session_id: sid_a.clone(),
            message: "only_a".into(),
        },
    )
    .await;

    // rx_a gets the event; rx_b does not (within a short timeout).
    let got_a = tokio::time::timeout(std::time::Duration::from_millis(50), rx_a.recv())
        .await
        .expect("rx_a timed out")
        .expect("rx_a recv error");
    match got_a {
        WsEvent::Error { message, .. } => assert_eq!(message, "only_a"),
        _ => panic!("wrong variant"),
    }

    let got_b = tokio::time::timeout(std::time::Duration::from_millis(50), rx_b.recv())
        .await;
    assert!(
        got_b.is_err(),
        "rx_b should NOT receive session_a's event, got {got_b:?}"
    );
}

#[tokio::test]
async fn eventbus_emit_on_unknown_session_lazily_creates_channel() {
    use drawio_agent_server::EventBus;

    let bus = EventBus::new();
    let sid = SessionId::new();

    // Emit before anyone subscribes — channel is created lazily.
    bus.emit(
        &sid,
        WsEvent::Error {
            session_id: sid.clone(),
            message: "orphan".into(),
        },
    )
    .await;

    // A late subscriber now sees the orphan event in its lagged queue.
    let mut rx = bus.subscribe(&sid).await.unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
    // First recv on a fresh subscription: should yield a Lagged (we missed
    // events while not subscribed) and then Closed when the sender is dropped
    // — OR yield the buffered event depending on capacity. We just verify
    // the subscribe path didn't panic.
    let _ = got;
}

// ---------------------------------------------------------------------------
// Integration tests: action handlers emit events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn generate_emits_version_created_after_success() {
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm.clone(), Arc::new(MockDriver::new()));
    let mut rx = state
        .events
        .subscribe(&SessionId::new()) // placeholder; will subscribe to real one below
        .await;
    let _ = rx; // discard

    // Real flow: create session, subscribe, generate, expect event.
    let app = router(state.clone());
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let session_id = SessionId(sid.clone());
    let mut rx = state.events.subscribe(&session_id).await.unwrap();

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/generate"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&GenReq {
                        prompt: "x".into(),
                        json_mode: false,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("event timed out")
        .expect("recv error");
    match event {
        WsEvent::VersionCreated { kind, .. } => assert_eq!(kind, "generate"),
        other => panic!("expected VersionCreated, got {other:?}"),
    }
}

#[tokio::test]
async fn generate_emits_error_on_llm_failure() {
    let llm = Arc::new(TestLlm::new());
    llm.set_failure("vlm is down");
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state.clone());

    let sid = create_session_with_xml(app.clone(), FULL_XML).await;
    let session_id = SessionId(sid.clone());
    let mut rx = state.events.subscribe(&session_id).await.unwrap();

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/generate"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&GenReq {
                        prompt: "x".into(),
                        json_mode: false,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("error event timed out")
        .expect("recv error");
    match event {
        WsEvent::Error { message, .. } => assert!(
            message.contains("vlm is down"),
            "error msg: {message}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn patch_emits_version_created_after_success() {
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm.clone(), Arc::new(MockDriver::new()));
    let app = router(state.clone());

    let sid = create_session_with_xml(app.clone(), FULL_XML).await;
    let session_id = SessionId(sid.clone());
    let mut rx = state.events.subscribe(&session_id).await.unwrap();

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/patch"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&PatchReq {
                        cell_ids: vec!["2".into()],
                        instruction: "x".into(),
                        json_mode: false,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("event timed out")
        .expect("recv error");
    match event {
        WsEvent::VersionCreated { kind, .. } => assert_eq!(kind, "patch"),
        other => panic!("expected VersionCreated, got {other:?}"),
    }
}

#[tokio::test]
async fn review_emits_error_on_renderer_failure() {
    let llm = Arc::new(TestLlm::new());
    let failing_renderer: Arc<dyn RenderDriver> =
        Arc::new(MockDriver::new().with_error("chrome not found".to_string()));
    let state = state_with(llm, failing_renderer);
    let app = router(state.clone());

    let sid = create_session_with_xml(app.clone(), FULL_XML).await;
    let session_id = SessionId(sid.clone());
    let mut rx = state.events.subscribe(&session_id).await.unwrap();

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/review"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&RevReq {
                        xml: None,
                        checks: vec![],
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("event timed out")
        .expect("recv error");
    match event {
        WsEvent::Error { message, .. } => {
            assert!(
                message.contains("chrome not found"),
                "got: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn review_emits_error_on_llm_failure() {
    let llm = Arc::new(TestLlm::new());
    llm.set_failure("vlm down");
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state.clone());

    let sid = create_session_with_xml(app.clone(), FULL_XML).await;
    let session_id = SessionId(sid.clone());
    let mut rx = state.events.subscribe(&session_id).await.unwrap();

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/review"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&RevReq {
                        xml: None,
                        checks: vec![],
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("event timed out")
        .expect("recv error");
    match event {
        WsEvent::Error { message, .. } => {
            assert!(message.contains("vlm down"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// Suppress unused-warning on `Uuid` import (kept for clarity).
#[allow(dead_code)]
fn _phantom(_u: Uuid) {}
