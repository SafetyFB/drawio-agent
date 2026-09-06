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
            finish_reason: None,
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
            finish_reason: None,
        })
    }
}

fn state_with(llm: Arc<dyn LlmProvider>, renderer: Arc<dyn RenderDriver>) -> Arc<AppState> {
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
async fn agent_loop_on_empty_session_succeeds() {
    // Refine is now self-contained: the loop's first step generates a
    // baseline if the session has no current XML. The previous behavior
    // was to 400 with 'run /generate first'; that's gone.
    let state = state_with(Arc::new(StubLlm::new()), Arc::new(MockDriver::new()));
    let app = router(state);

    // Empty session (no initial_xml).
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
        .clone()
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
    assert_eq!(resp.status(), StatusCode::OK);
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

#[tokio::test]
async fn agent_loop_empty_session_auto_generates_baseline() {
    // Refine is now self-contained: when the session has no current XML,
    // the loop's first step generates a baseline (via the runner's existing
    // initial_xml=None handling) instead of 400ing. The user doesn't have
    // to do a separate /generate first.
    let state = state_with(Arc::new(StubLlm::new()), Arc::new(MockDriver::new()));
    let app = router(state);

    // Empty session (no XML yet) — the exact "create then Run Loop" flow.
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

    let resp = app
        .clone()
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
    // Must succeed — Refine is now self-contained.
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 65_536)
        .await
        .unwrap();
    let outcome: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // The runner returned an AgentOutcome with a non-empty final_xml
    // (StubLlm's MOCK_DIAGRAM, since the codegen step ran on the empty session).
    let final_xml = outcome["final_xml"]
        .as_str()
        .expect("final_xml must be a string");
    assert!(
        !final_xml.is_empty() && final_xml.contains("<mxfile"),
        "loop should have produced a diagram from scratch, got: {final_xml:?}"
    );

    // The session should now have a stored current_xml.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/sessions/{sid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 65_536)
        .await
        .unwrap();
    let session: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let stored = session["current_xml"]
        .as_str()
        .expect("current_xml must be present");
    assert!(
        !stored.is_empty(),
        "session should have a stored diagram after the loop, got: {stored:?}"
    );
}

// ---------------------------------------------------------------------------
// Stub LLM for the Plan-B subgraph test: records the patch scope, returns
// a modified subgraph document, and converges after one patch.
// ---------------------------------------------------------------------------

/// Full diagram with an unrelated cell (5) so a scope for `["2"]` must NOT
/// include it.
const SUBGRAPH_FULL_XML: &str = r#"<mxfile host="app.diagrams.net">
  <diagram id="d" name="Page-1">
    <mxGraphModel dx="800" dy="600" grid="1" guides="1" tooltips="1" connect="1" arrows="1" fold="1" page="1" pageScale="1" pageWidth="850" pageHeight="1100" math="0" shadow="0">
      <root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="2" value="Hello" style="rounded=0;" vertex="1" parent="1">
          <mxGeometry x="100" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="3" value="World" style="rounded=0;" vertex="1" parent="1">
          <mxGeometry x="300" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="4" style="edgeStyle=orthogonalEdgeStyle;" edge="1" parent="1" source="2" target="3">
          <mxGeometry relative="1" as="geometry"/>
        </mxCell>
        <mxCell id="5" value="Unrelated" style="rounded=0;" vertex="1" parent="1">
          <mxGeometry x="600" y="300" width="120" height="60" as="geometry"/>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

/// What the loop's internal patch LLM "returns": cell 2 modified, nothing else.
const SUBGRAPH_PATCHED_XML: &str = r#"<mxfile>
  <diagram id="d">
    <mxGraphModel>
      <root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="2" value="Modified" style="rounded=0;" vertex="1" parent="1">
          <mxGeometry x="100" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

#[derive(Clone)]
struct SubgraphAwareLlm {
    issues_served: Arc<std::sync::Mutex<u32>>,
    patch_scopes: Arc<std::sync::Mutex<Vec<String>>>,
    patch_current_xmls: Arc<std::sync::Mutex<Vec<Option<String>>>>,
}

impl SubgraphAwareLlm {
    fn new() -> Self {
        Self {
            issues_served: Arc::new(std::sync::Mutex::new(0)),
            patch_scopes: Arc::new(std::sync::Mutex::new(Vec::new())),
            patch_current_xmls: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for SubgraphAwareLlm {
    fn name(&self) -> &str {
        "subgraph-aware"
    }
    async fn generate_xml(
        &self,
        req: GenerateRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        // The only generate_xml call in the loop is the internal patch step
        // (initial XML is supplied), so every call records a patch request.
        self.patch_scopes
            .lock()
            .unwrap()
            .push(req.scope.clone().unwrap_or_default());
        self.patch_current_xmls
            .lock()
            .unwrap()
            .push(req.current_xml.clone());
        Ok(LlmResponse {
            content: SUBGRAPH_PATCHED_XML.to_string(),
            usage: Usage::default(),
            raw: Value::Null,
            duration_ms: 0,
            finish_reason: None,
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
        let mut served = self.issues_served.lock().unwrap();
        let first = *served == 0;
        *served += 1;
        let (verdict, issues) = if first {
            (
                "issues",
                vec![drawio_agent_llm_client::ReviewIssue {
                    kind: "overlap".into(),
                    severity: "high".into(),
                    cell_ids: vec!["2".into()],
                    description: "overlap".into(),
                }],
            )
        } else {
            ("pass", vec![])
        };
        Ok(LlmResponse {
            content: ReviewResponse {
                verdict: verdict.into(),
                issues,
            },
            usage: Usage::default(),
            raw: Value::Null,
            duration_ms: 0,
            finish_reason: None,
        })
    }
}

#[tokio::test]
async fn agent_loop_patch_sends_subgraph_scope_not_full_xml() {
    let llm = Arc::new(SubgraphAwareLlm::new());
    let state = state_with(llm.clone(), Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), SUBGRAPH_FULL_XML).await;

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
    assert!(outcome.converged(), "got {:?}", outcome.final_phase);

    // The internal patch must have sent ONLY the subgraph as scope.
    let scopes = llm.patch_scopes.lock().unwrap().clone();
    assert_eq!(scopes.len(), 1, "loop should have patched exactly once");
    let scope = &scopes[0];
    assert!(
        scope.len() < SUBGRAPH_FULL_XML.len(),
        "scope ({}) must be shorter than the full diagram ({})",
        scope.len(),
        SUBGRAPH_FULL_XML.len()
    );
    assert!(
        scope.contains("id=\"2\""),
        "scope must include the targeted cell: {scope}"
    );
    assert!(
        !scope.contains("Unrelated"),
        "scope must NOT include unrelated cell 5: {scope}"
    );

    // And the full diagram must NOT be sent as current_xml (Plan B sends
    // current_xml: None with only the subgraph as scope).
    let current_xmls = llm.patch_current_xmls.lock().unwrap().clone();
    assert!(
        current_xmls.iter().all(|c| c.is_none()),
        "patch must not send the full diagram as current_xml: {current_xmls:?}"
    );

    // The loop still converges with the patched cell applied back into the
    // full diagram.
    assert!(
        outcome.final_xml.contains("value=\"Modified\""),
        "patched cell must reflect the LLM's change"
    );
}

// ---------------------------------------------------------------------------
// Gated LLM for the live-streaming test: review_visual blocks until the
// test releases it, so the loop is provably mid-flight when we observe
// trajectory events on the EventBus.
// ---------------------------------------------------------------------------

struct GatedLlm {
    release: Arc<tokio::sync::Notify>,
}

impl GatedLlm {
    fn new() -> Self {
        Self {
            release: Arc::new(tokio::sync::Notify::new()),
        }
    }
    fn release(&self) {
        self.release.notify_one();
    }
}

#[async_trait::async_trait]
impl LlmProvider for GatedLlm {
    fn name(&self) -> &str {
        "gated"
    }
    async fn generate_xml(
        &self,
        _req: GenerateRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        Ok(LlmResponse {
            content: FULL_XML.to_string(),
            usage: Usage::default(),
            raw: Value::Null,
            duration_ms: 0,
            finish_reason: None,
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
        self.release.notified().await;
        Ok(LlmResponse {
            content: ReviewResponse {
                verdict: "pass".into(),
                issues: vec![],
            },
            usage: Usage::default(),
            raw: Value::Null,
            duration_ms: 0,
            finish_reason: None,
        })
    }
}

#[tokio::test]
async fn agent_loop_streams_trajectory_events_during_loop() {
    let llm = Arc::new(GatedLlm::new());
    let state = state_with(llm.clone(), Arc::new(MockDriver::new()));
    let app = router(state.clone());
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let session_id = drawio_agent_server::state::SessionId(sid.clone());
    let mut rx = state.events.subscribe(&session_id).await.unwrap();

    // Run the loop in a background task. Review blocks on the gate, so the
    // loop stays mid-flight until the test releases it.
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{sid}/agent-loop"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({"prompt": "improve"}))
                .unwrap(),
        ))
        .unwrap();
    let app2 = app.clone();
    let handle = tokio::spawn(async move { app2.oneshot(req).await.unwrap() });

    // Wait for a trajectory event on the bus BEFORE releasing the gate. If
    // events stream live, RenderStarted arrives while the loop is still
    // blocked in review.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut saw_live_event = false;
    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(WsEvent::Trajectory(e))) => {
                if e.kind.kind() == TrajectoryEventKind::RenderStarted {
                    saw_live_event = true;
                    break;
                }
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    assert!(
        saw_live_event,
        "expected a live RenderStarted on the WS before the loop finished"
    );

    // Release the review gate; the loop converges and the request completes.
    llm.release();
    let resp = handle.await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[allow(dead_code)]
fn _suppress_unused(_o: &Event, _l: &AgentLoop) {}

// ---------------------------------------------------------------------------
// Counting LLM for the Q3 regression: the loop must NOT re-run Generate
// when the caller supplies an initial_xml (the canvas's current diagram).
// ---------------------------------------------------------------------------

/// Counts every `generate_xml` call so the test can assert the loop never
/// re-generates when handed a starting XML. Reviews always pass (no patch →
/// no extra generate call from the inner patch step).
#[derive(Clone)]
struct CountingLlm {
    generate_calls: Arc<std::sync::Mutex<u32>>,
}

impl CountingLlm {
    fn new() -> Self {
        Self {
            generate_calls: Arc::new(std::sync::Mutex::new(0)),
        }
    }
    fn generate_count(&self) -> u32 {
        *self.generate_calls.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl LlmProvider for CountingLlm {
    fn name(&self) -> &str {
        "counting"
    }
    async fn generate_xml(
        &self,
        _req: GenerateRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        *self.generate_calls.lock().unwrap() += 1;
        Ok(LlmResponse {
            content: FULL_XML.to_string(),
            usage: Usage::default(),
            raw: Value::Null,
            duration_ms: 0,
            finish_reason: None,
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
        Ok(LlmResponse {
            content: ReviewResponse {
                verdict: "pass".into(),
                issues: vec![],
            },
            usage: Usage::default(),
            raw: Value::Null,
            duration_ms: 0,
            finish_reason: None,
        })
    }
}

#[tokio::test]
async fn agent_loop_with_initial_xml_does_not_call_generate() {
    let llm = Arc::new(CountingLlm::new());
    let state = state_with(llm.clone(), Arc::new(MockDriver::new()));
    let app = router(state);

    // Fresh session (no XML yet) — the UX flow: user clicks Generate.
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
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), 4096)
        .await
        .unwrap();
    let parsed: drawio_agent_server::CreateSessionResponse = serde_json::from_slice(&body).unwrap();
    let sid = parsed.session_id.as_str().to_string();

    // 1. The initial generate — this is the ONE allowed generate_xml call.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/generate"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"prompt": "draw something"}))
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
    let gen: drawio_agent_server::GenerateResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        llm.generate_count(),
        1,
        "generate endpoint must call the LLM exactly once"
    );

    // 2. Run Loop with the generated diagram as initial_xml (what the canvas
    // holds) — must NOT re-run the Generate phase.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/agent-loop"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "prompt": "improve",
                        "initial_xml": gen.xml,
                    }))
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
    assert!(outcome.converged(), "got {:?}", outcome.final_phase);

    assert_eq!(
        llm.generate_count(),
        1,
        "agent-loop with initial_xml must NOT call generate_xml again \
         (only the initial /generate did)"
    );
}
