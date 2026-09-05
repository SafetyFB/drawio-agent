//! TDD #3: server integration — action handlers record to trajectory,
//! and GET /api/sessions/:id/trajectory returns them.

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
    AppState, CreateSessionRequest, GenerateRequest as GenReq, PatchRequest as PatchReq,
    ReviewRequest as RevReq,
};
use drawio_agent_trajectory::{Event, TrajectoryEvent, TrajectoryEventKind};
use tower::ServiceExt;

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
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
            },
            raw: serde_json::Value::Null,
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
            raw: serde_json::Value::Null,
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
        trajectory: drawio_agent_trajectory::TrajectoryStore::new(),
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

async fn fetch_trajectory(app: axum::Router, sid: &str) -> (StatusCode, Vec<Event>) {
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{sid}/trajectory"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 65536)
        .await
        .unwrap();
    let events: Vec<Event> = serde_json::from_slice(&body).unwrap_or_default();
    (status, events)
}

// ---------------------------------------------------------------------------
// New endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trajectory_endpoint_returns_404_for_unknown_session() {
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state);
    let (status, _) = fetch_trajectory(app, "no-such").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn trajectory_endpoint_returns_empty_array_for_fresh_session() {
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let (status, events) = fetch_trajectory(app, &sid).await;
    assert_eq!(status, StatusCode::OK);
    assert!(events.is_empty(), "no actions yet → no events");
}

// ---------------------------------------------------------------------------
// Generate handler recording
// ---------------------------------------------------------------------------

#[tokio::test]
async fn generate_records_llm_started_and_completed() {
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/generate"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&GenReq {
                        prompt: "draw a box".into(),
                        json_mode: true,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let (status, events) = fetch_trajectory(app, &sid).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(events.len(), 2, "expected Started + Completed, got {events:?}");

    match &events[0].kind {
        TrajectoryEvent::LlmCallStarted { prompt_chars, json_mode } => {
            assert_eq!(*prompt_chars, "draw a box".chars().count());
            assert!(*json_mode);
        }
        other => panic!("expected LlmCallStarted, got {other:?}"),
    }
    match &events[1].kind {
        TrajectoryEvent::LlmCallCompleted {
            input_tokens,
            output_tokens,
            ..
        } => {
            assert_eq!(*input_tokens, 100);
            assert_eq!(*output_tokens, 50);
        }
        other => panic!("expected LlmCallCompleted, got {other:?}"),
    }
}

#[tokio::test]
async fn generate_records_error_event_on_llm_failure() {
    let llm = Arc::new(TestLlm::new());
    llm.set_failure("upstream gone");
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

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

    let (_, events) = fetch_trajectory(app, &sid).await;
    assert_eq!(events.len(), 2);
    match &events[1].kind {
        TrajectoryEvent::Error { stage, message } => {
            assert_eq!(stage, "generate");
            assert!(message.contains("upstream gone"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Patch handler recording
// ---------------------------------------------------------------------------

#[tokio::test]
async fn patch_records_llm_started_and_completed() {
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/patch"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&PatchReq {
                        cell_ids: vec!["2".into()],
                        instruction: "recolor".into(),
                        json_mode: false,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let (_, events) = fetch_trajectory(app, &sid).await;
    assert_eq!(events.len(), 2, "patch → Started + Completed");
    assert!(matches!(events[0].kind, TrajectoryEvent::LlmCallStarted { .. }));
    assert!(matches!(events[1].kind, TrajectoryEvent::LlmCallCompleted { .. }));
}

// ---------------------------------------------------------------------------
// Render handler recording
// ---------------------------------------------------------------------------

#[tokio::test]
async fn render_records_render_started_and_completed() {
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/render"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let (_, events) = fetch_trajectory(app, &sid).await;
    assert_eq!(events.len(), 2, "render → Started + Completed");
    assert!(matches!(events[0].kind, TrajectoryEvent::RenderStarted { .. }));
    match &events[1].kind {
        TrajectoryEvent::RenderCompleted { bytes, .. } => {
            assert!(*bytes > 0);
        }
        other => panic!("expected RenderCompleted, got {other:?}"),
    }
}

#[tokio::test]
async fn render_records_error_on_renderer_failure() {
    let llm = Arc::new(TestLlm::new());
    let failing_renderer: Arc<dyn RenderDriver> =
        Arc::new(MockDriver::new().with_error("chrome missing".to_string()));
    let state = state_with(llm, failing_renderer);
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/render"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let (_, events) = fetch_trajectory(app, &sid).await;
    assert_eq!(events.len(), 2, "Started + Error");
    match &events[1].kind {
        TrajectoryEvent::Error { stage, message } => {
            assert_eq!(stage, "render");
            assert!(message.contains("chrome missing"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Review handler recording
// ---------------------------------------------------------------------------

#[tokio::test]
async fn review_records_llm_started_and_completed() {
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/review"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&RevReq {
                        xml: None,
                        checks: vec!["overlap".into(), "text_overflow".into()],
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let (_, events) = fetch_trajectory(app, &sid).await;
    let kinds: Vec<TrajectoryEventKind> = events.iter().map(|e| e.kind.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            TrajectoryEventKind::RenderCompleted,
            TrajectoryEventKind::LlmCallStarted,
            TrajectoryEventKind::LlmCallCompleted,
        ],
        "got {kinds:?}"
    );
}

#[tokio::test]
async fn review_records_error_on_llm_failure() {
    let llm = Arc::new(TestLlm::new());
    llm.set_failure("vlm offline");
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

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

    let (_, events) = fetch_trajectory(app, &sid).await;
    let kinds: Vec<TrajectoryEventKind> = events.iter().map(|e| e.kind.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            TrajectoryEventKind::RenderCompleted,
            TrajectoryEventKind::LlmCallStarted,
            TrajectoryEventKind::Error,
        ],
        "got {kinds:?}"
    );
}

#[tokio::test]
async fn trajectory_endpoint_at_is_i64_milliseconds_for_js() {
    // Bug A: `at` used to serialize as a {secs, nanos} object, which JS
    // `new Date({...})` rejects with Invalid Date. It must be a plain i64
    // milliseconds-since-epoch number on the wire.
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/generate"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&GenReq {
                        prompt: "draw".into(),
                        json_mode: false,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{sid}/trajectory"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 65536)
        .await
        .unwrap();
    let events: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert!(!events.is_empty(), "expected at least one event");
    assert!(
        events[0]["at"].is_i64(),
        "at should be an i64 (ms since epoch), got: {}",
        events[0]["at"]
    );
}
