//! TDD tests for the Agent Loop HTTP endpoint.
//!
//! `POST /api/sessions/:id/agent-loop` runs the Agent Loop against an
//! existing session's current XML and streams progress to the WS.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drawio_agent_agent::{AgentLoop, AgentOutcome, LoopPhase};
use drawio_agent_llm_client::{
    GenerateRequest, LlmProvider, LlmResponse, LlmStream, ProviderError, ReviewRequest,
    ReviewResponse, Usage,
};
use drawio_agent_renderer::{MockDriver, RenderDriver};
use drawio_agent_server::{
    state::WsEvent, AppState, CreateSessionRequest, EventBus, SessionStore,
};
use drawio_agent_trajectory::{Event, TrajectoryEventKind};
use serde_json::Value;
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

// ---------------------------------------------------------------------------
// Stub LLM (returns pass on first review after a patch)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StubLlm {
    patched: Arc<std::sync::Mutex<bool>>,
}

impl StubLlm {
    fn new() -> Self {
        Self { patched: Arc::new(std::sync::Mutex::new(false)) }
    }
}

#[async_trait::async_trait]
impl LlmProvider for StubLlm {
    fn name(&self) -> &str { "stub" }
    async fn generate_xml(&self, _req: GenerateRequest) -> Result<LlmResponse<String>, ProviderError> {
        // First call returns the original XML; after patch() runs once,
        // it returns an "improved" XML. Either way the loop just uses it.
        Ok(LlmResponse {
            content: FULL_XML.to_string(),
            usage: Usage { input_tokens: 50, output_tokens: 20 },
            raw: Value::Null,
            duration_ms: 0,
        })
    }
    async fn generate_streaming(&self, _req: GenerateRequest) -> Result<LlmStream, ProviderError> { unimplemented!() }
    async fn review_visual(&self, _req: ReviewRequest) -> Result<LlmResponse<ReviewResponse>, ProviderError> {
        // First review (before patch) returns "issues"; subsequent ones return "pass".
        let verdict = if *self.patched.lock().unwrap() {
            "pass".to_string()
        } else {
            // Flip to patched after the first review completes.
            *self.patched.lock().unwrap() = true;
            "issues".to_string()
        };
        Ok(LlmResponse {
            content: ReviewResponse {
                verdict: verdict.clone(),
                issues: if verdict == "issues" {
                    vec![drawio_agent_llm_client::ReviewIssue {
                        kind: "overlap".into(),
                        severity: "high".into(),
                        cell_ids: vec!["2".into()],
                        description: "overlap".into(),
                    }]
                } else {
                    vec![]
                },
            },
            usage: Usage::default(),
            raw: Value::Null,
            duration_ms: 0,
        })
    }
}

fn state_with(llm: Arc<StubLlm>, renderer: Arc<dyn RenderDriver>) -> Arc<AppState> {
    Arc::new(AppState {
        sessions: Arc::new(tokio::sync::RwLock::new(SessionStore::new())),
        llm,
        renderer,
        events: EventBus::new(),
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
    let body = axum::body::to_bytes(resp.into_body(), 4096)
        .await
        .unwrap();
    let parsed: drawio_agent_server::CreateSessionResponse = serde_json::from_slice(&body).unwrap();
    parsed.session_id.as_str().to_string()
}

#[tokio::test]
async fn agent_loop_returns_404_for_unknown_session() {
    let state = state_with(Arc::new(StubLlm::new()), Arc::new(MockDriver::new()));
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/no-such/agent-loop")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"prompt": "draw"}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn agent_loop_returns_400_when_session_has_no_xml() {
    let state = state_with(Arc::new(StubLlm::new()), Arc::new(MockDriver::new()));
    let app = router(state);

    // Empty session (no initial_xml)
    let resp = app
        .clone()
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
    let body = axum::body::to_bytes(resp.into_body(), 4096)
        .await
        .unwrap();
    let parsed: drawio_agent_server::CreateSessionResponse = serde_json::from_slice(&body).unwrap();
    let sid = parsed.session_id.as_str().to_string();

    // Now try the agent-loop without an XML in the request body.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/agent-loop"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"prompt": "draw"}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn agent_loop_converges_after_patch_returns_done() {
    let state = state_with(Arc::new(StubLlm::new()), Arc::new(MockDriver::new()));
    let app = router(state.clone());
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/agent-loop"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"prompt": "improve"}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 65536)
        .await
        .unwrap();
    let outcome: AgentOutcome = serde_json::from_slice(&body).unwrap();
    assert!(outcome.converged(), "expected Done, got {:?}", outcome.final_phase);
    assert_eq!(outcome.final_phase, LoopPhase::Done);
    // Stub: first review "issues", patch runs, second review "pass" → 2 iterations.
    assert_eq!(outcome.iterations, 2);
}

#[tokio::test]
async fn agent_loop_final_xml_is_stored_as_new_session_version() {
    let state = state_with(Arc::new(StubLlm::new()), Arc::new(MockDriver::new()));
    let app = router(state.clone());
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/agent-loop"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"prompt": "improve"}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // GET versions — should now have the initial + the agent-loop final.
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{sid}/versions"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 8192)
        .await
        .unwrap();
    let parsed: drawio_agent_server::VersionsResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed.versions.len(), 2, "initial + agent-loop final");
    assert_eq!(parsed.versions.last().unwrap().kind, "agent-loop");
}

#[tokio::test]
async fn agent_loop_emits_trajectory_events_to_ws() {
    let llm = Arc::new(StubLlm::new());
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state.clone());
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    // Subscribe to WS events before running the loop.
    let session_id = drawio_agent_server::state::SessionId(sid.clone());
    let mut rx = state.events.subscribe(&session_id).await.unwrap();

    let _resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/agent-loop"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"prompt": "improve"}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    // Drain events for a few ms and collect Trajectory events.
    let mut traj_kinds: Vec<TrajectoryEventKind> = Vec::new();
    let mut saw_version_created = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) => match event {
                WsEvent::Trajectory(e) => traj_kinds.push(e.kind.kind()),
                WsEvent::VersionCreated { .. } => saw_version_created = true,
                WsEvent::Error { .. } => {}
            },
            _ => break,
        }
    }

    assert!(!traj_kinds.is_empty(), "expected at least one trajectory event");
    assert!(saw_version_created, "expected a VersionCreated WS event");
    // Stub loop emits: RenderStarted/Completed (x2) + LlmCallStarted/Completed
    // for both review (x2) and patch (x1) → at least 5 events.
    assert!(traj_kinds.len() >= 5, "got: {traj_kinds:?}");
    assert!(traj_kinds.contains(&TrajectoryEventKind::RenderStarted));
    assert!(traj_kinds.contains(&TrajectoryEventKind::LlmCallStarted));
}

#[tokio::test]
async fn agent_loop_response_includes_full_trajectory() {
    let state = state_with(Arc::new(StubLlm::new()), Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/agent-loop"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"prompt": "improve"}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 65536)
        .await
        .unwrap();
    let outcome: AgentOutcome = serde_json::from_slice(&body).unwrap();
    assert!(outcome.trajectory.len() >= 5);
    assert_eq!(outcome.final_phase, LoopPhase::Done);
}

#[tokio::test]
async fn agent_loop_rejects_request_without_prompt() {
    let state = state_with(Arc::new(StubLlm::new()), Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/agent-loop"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    // Either 400 Bad Request (missing required field) or 422 — both are
    // acceptable "client error" responses. We just need non-2xx.
    assert!(resp.status().is_client_error(), "got {}", resp.status());
}

#[allow(dead_code)]
fn _suppress_unused(_o: &Event, _l: &AgentLoop) {}
