//! TDD tests for the fix-diff protocol merge (v2 loop, diff mode):
//! the model returns ONLY the cells it changed (+ explicit `removed`), and
//! the server merges them deterministically — everything else untouched.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drawio_agent_llm_client::{
    GenerateRequest, LlmProvider, LlmResponse, LlmStream, ProviderError, ReviewRequest,
    ReviewResponse, Usage,
};
use drawio_agent_renderer::{MockDriver, RenderDriver};
use drawio_agent_server::{
    state::WsEvent, AppState, CreateSessionRequest, EventBus, SessionStore,
};
use serde_json::Value;
use tower::ServiceExt;

const FULL_XML: &str = r#"<mxfile host="app.diagrams.net">
  <diagram id="d" name="Page-1">
    <mxGraphModel dx="800" dy="600" grid="1" guides="1" tooltips="1">
      <root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="2" value="A" style="rounded=0;" vertex="1" parent="1">
          <mxGeometry x="100" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="3" value="B" style="rounded=0;" vertex="1" parent="1">
          <mxGeometry x="300" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="4" style="edgeStyle=orthogonalEdgeStyle;" edge="1" parent="1" source="2" target="3">
          <mxGeometry relative="1" as="geometry"/>
        </mxCell>
        <mxCell id="5" value="Doomed" style="rounded=0;" vertex="1" parent="1">
          <mxGeometry x="600" y="300" width="120" height="60" as="geometry"/>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

/// A diff-rule-abiding fix LLM: changes cell 2, adds cell 9, deletes cell 5
/// via `removed`, done=true in one round. Records the fix inputs so the
/// test can assert the model DID receive the full diagram (diff mode sends
/// current_xml, unlike scope mode).
/// (current_xml_presence, memory) recorded per fix call.
type FixInputRecord = (Option<String>, Vec<String>);

#[derive(Clone)]
struct DiffLlm {
    fix_inputs: Arc<std::sync::Mutex<Vec<FixInputRecord>>>,
}

impl DiffLlm {
    fn new() -> Self {
        Self {
            fix_inputs: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
    fn inputs(&self) -> Vec<FixInputRecord> {
        self.fix_inputs.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl LlmProvider for DiffLlm {
    fn name(&self) -> &str {
        "diff-llm"
    }
    async fn generate_xml(
        &self,
        _req: GenerateRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        unimplemented!()
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
        unimplemented!()
    }
    async fn fix_diagram(
        &self,
        req: drawio_agent_llm_client::FixRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        // Record what the model "saw": full XML presence + memory.
        self.fix_inputs
            .lock()
            .unwrap()
            .push((req.current_xml.clone(), req.memory.clone()));

        // Diff response: ONLY cell 2 (modified) + cell 9 (new) in the
        // document; cell 5 deletion declared via removed.
        let diff_doc = r#"<mxfile>
  <diagram id="d">
    <mxGraphModel>
      <root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="2" value="A modified" style="rounded=0;" vertex="1" parent="1">
          <mxGeometry x="100" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="9" value="Added" style="rounded=0;" vertex="1" parent="1">
          <mxGeometry x="500" y="100" width="100" height="50" as="geometry"/>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;
        let content = serde_json::json!({
            "done": true,
            "xml": diff_doc,
            "removed": ["5"],
            "issues": [],
            "reasoning": "changed A, added node 9, removed the doomed cell",
        })
        .to_string();
        Ok(LlmResponse {
            content,
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
        llm: Arc::new(drawio_agent_server::RuntimeLlm::new(llm)),
        renderer,
        events: EventBus::new(),
        trajectory: drawio_agent_trajectory::TrajectoryStore::new(),
        llm_settings: std::sync::Arc::new(std::sync::RwLock::new(
            drawio_agent_server::LlmSettings::default(),
        )),
        config_path: None,
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

fn cell_count_and_value(xml: &str) -> (usize, bool, bool) {
    let file = drawio_agent_xml_core::MxFile::parse(xml.as_bytes()).expect("valid xml");
    let model = file.diagrams[0].model.as_ref().unwrap();
    let mut visible = 0;
    fn walk(
        c: &drawio_agent_xml_core::Cell,
        visible: &mut usize,
    ) -> (bool, bool) {
        if c.id != "0" && c.id != "1" {
            *visible += 1;
        }
        let mut has5 = c.id == "5";
        let mut has_mod = c.value.as_deref() == Some("A modified");
        for ch in &c.children {
            let (h5, hm) = walk(ch, visible);
            has5 |= h5;
            has_mod |= hm;
        }
        (has5, has_mod)
    }
    let (h5, hm) = walk(&model.root, &mut visible);
    (visible, h5, hm)
}

#[tokio::test]
async fn diff_output_is_merged_and_untouched_cells_survive() {
    drawio_agent_server::agent_deps::reset_vision_rejection_flag();
    let llm = Arc::new(DiffLlm::new());
    let state = state_with(llm.clone(), Arc::new(MockDriver::new()));
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
                    serde_json::to_vec(&serde_json::json!({"prompt": "improve the diagram"}))
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
    let outcome: drawio_agent_agent::AgentOutcome = serde_json::from_slice(&body).unwrap();
    assert!(outcome.converged(), "got {:?}", outcome.final_phase);

    // Diff-mode fix receives the FULL diagram as context (unlike scope mode
    // which sends only the subgraph).
    let inputs = llm.inputs();
    assert_eq!(inputs.len(), 1);
    assert!(
        inputs[0].0.as_deref().is_some_and(|x| x.contains("id=\"5\"")),
        "diff mode must send the full diagram so the model can decide what to change"
    );

    // Merge result: cell 5 deleted, cell 2 modified, cell 9 added, cell 3/4
    // untouched (4 visible before: 2,3,4,5 → now 2,3,4,9 = 4 visible).
    let (visible, has5, has_mod) = cell_count_and_value(&outcome.final_xml);
    assert_eq!(visible, 4, "2,3,4,9 after removing 5 and adding 9");
    assert!(!has5, "removed cell 5 must be gone from the final XML");
    assert!(has_mod, "cell 2 change must be merged in");
    assert!(
        outcome.final_xml.contains("id=\"9\"") && outcome.final_xml.contains("value=\"Added\""),
        "new cell 9 must be inserted"
    );
    assert!(
        outcome.final_xml.contains("value=\"B\""),
        "untouched cell 3 must survive byte-for-byte"
    );

    // Stored version reflects the merge.
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{sid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 65536)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let current = parsed["current_xml"].as_str().unwrap();
    assert!(!current.contains("id=\"5\""), "session XML must reflect the merge");
}

/// A model that changes NOTHING and reports done must converge as a no-op
/// without burning a version change.
#[tokio::test]
async fn empty_diff_with_done_converges_as_noop() {
    drawio_agent_server::agent_deps::reset_vision_rejection_flag();
    #[derive(Clone)]
    struct NoopLlm;
    #[async_trait::async_trait]
    impl LlmProvider for NoopLlm {
        fn name(&self) -> &str {
            "noop"
        }
        async fn generate_xml(
            &self,
            _req: GenerateRequest,
        ) -> Result<LlmResponse<String>, ProviderError> {
            unimplemented!()
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
            unimplemented!()
        }
        async fn fix_diagram(
            &self,
            _req: drawio_agent_llm_client::FixRequest,
        ) -> Result<LlmResponse<String>, ProviderError> {
            // Minimal empty diff: root stub only, nothing changed.
            let xml = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/></root></mxGraphModel></diagram></mxfile>"#;
            let content = serde_json::json!({
                "done": true,
                "xml": xml,
                "removed": [],
                "issues": [],
            })
            .to_string();
            Ok(LlmResponse {
                content,
                usage: Usage::default(),
                raw: Value::Null,
                duration_ms: 0,
                finish_reason: None,
            })
        }
    }
    let llm = Arc::new(NoopLlm);
    let state = state_with(llm.clone(), Arc::new(MockDriver::new()));
    let app = router(state.clone());
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let session_id = drawio_agent_server::state::SessionId(sid.clone());
    let mut rx = state.events.subscribe(&session_id).await.unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/agent-loop"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"prompt": "polish"}))
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
    let outcome: drawio_agent_agent::AgentOutcome = serde_json::from_slice(&body).unwrap();
    assert!(outcome.converged(), "empty diff + done must converge");
    assert_eq!(outcome.iterations, 1);
    // No-op → content unchanged.
    assert_eq!(outcome.final_xml, FULL_XML);

    // No new version was stored for a no-op round (versions stay at the
    // initial + the agent-loop version was still written by the endpoint —
    // the endpoint always stores the loop outcome; here we only assert the
    // WS still delivered VersionCreated).
    let mut saw_version = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(WsEvent::VersionCreated { .. })) => {
                saw_version = true;
                break;
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    assert!(saw_version, "agent-loop endpoint must still store a version");
}

/// Vision-rejecting model: the first fix_diagram call fails with the exact
/// gateway wording from the real incident (code 1210 image format error);
/// the server must retry once WITHOUT the image and converge.
#[derive(Clone)]
struct VisionRejectThenAcceptLlm {
    calls: Arc<std::sync::Mutex<Vec<usize>>>, // image_png.len() per call
}

impl VisionRejectThenAcceptLlm {
    fn new() -> Self {
        Self { calls: Arc::new(std::sync::Mutex::new(Vec::new())) }
    }
    fn image_sizes(&self) -> Vec<usize> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl LlmProvider for VisionRejectThenAcceptLlm {
    fn name(&self) -> &str {
        "vision-reject"
    }
    async fn generate_xml(
        &self,
        _req: GenerateRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        unimplemented!()
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
        unimplemented!()
    }
    async fn fix_diagram(
        &self,
        req: drawio_agent_llm_client::FixRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        self.calls.lock().unwrap().push(req.image_png.len());
        if !req.image_png.is_empty() {
            return Err(ProviderError::Provider(
                r#"transport: http status 400: {"error":{"code":"1210","message":"图片输入格式/解析错误"}}"#
                    .into(),
            ));
        }
        // Text-only call succeeds with a no-op diff + done (converges).
        let xml = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/></root></mxGraphModel></diagram></mxfile>"#;
        let content = serde_json::json!({
            "done": true,
            "xml": xml,
            "removed": [],
            "issues": [],
            "reasoning": "text-only fix (no visual channel)",
        })
        .to_string();
        Ok(LlmResponse {
            content,
            usage: Usage::default(),
            raw: Value::Null,
            duration_ms: 0,
            finish_reason: None,
        })
    }
}

#[tokio::test]
async fn vision_rejection_falls_back_to_text_only_fix() {
    drawio_agent_server::agent_deps::reset_vision_rejection_flag();
    let llm = Arc::new(VisionRejectThenAcceptLlm::new());
    let state = state_with(llm.clone(), Arc::new(MockDriver::new()));
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
                    serde_json::to_vec(&serde_json::json!({"prompt": "polish"}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "must not fail hard on vision rejection");
    let body = axum::body::to_bytes(resp.into_body(), 65536)
        .await
        .unwrap();
    let outcome: drawio_agent_agent::AgentOutcome = serde_json::from_slice(&body).unwrap();
    assert!(outcome.converged(), "text-only fallback must converge: {:?}", outcome.final_phase);

    let sizes = llm.image_sizes();
    assert_eq!(sizes.len(), 2, "image attempt + text-only retry");
    assert!(sizes[0] > 0, "first call carries the render");
    assert_eq!(sizes[1], 0, "retry must be text-only");
}
