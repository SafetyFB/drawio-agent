//! TDD tests for the R2 conversation memory pipeline:
//! every runSend turn (generate / patch / agent-loop) records user asks +
//! agent summaries into the session, and later runs replay the recent tail
//! as background context for the LLM.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drawio_agent_llm_client::{
    GenerateRequest, LlmProvider, LlmResponse, LlmStream, ProviderError, ReviewRequest,
    ReviewResponse, Usage,
};
use drawio_agent_renderer::{MockDriver, RenderDriver};
use drawio_agent_server::{
    AppState, CreateSessionRequest, EventBus, SessionStore,
};
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

/// Records every generation request (including its memory) so tests can
/// assert what earlier-turn context reached the model.
#[derive(Clone)]
struct MemoryRecordingLlm {
    generate_memories: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    fix_memories: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
}

impl MemoryRecordingLlm {
    fn new() -> Self {
        Self {
            generate_memories: Arc::new(std::sync::Mutex::new(Vec::new())),
            fix_memories: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
    fn generate_memories(&self) -> Vec<Vec<String>> {
        self.generate_memories.lock().unwrap().clone()
    }
    fn fix_memories(&self) -> Vec<Vec<String>> {
        self.fix_memories.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl LlmProvider for MemoryRecordingLlm {
    fn name(&self) -> &str {
        "memory-recording"
    }
    async fn generate_xml(
        &self,
        req: GenerateRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        self.generate_memories.lock().unwrap().push(req.memory.clone());
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
        unimplemented!()
    }
    async fn fix_diagram(
        &self,
        req: drawio_agent_llm_client::FixRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        self.fix_memories.lock().unwrap().push(req.memory.clone());
        let xml = req
            .current_xml
            .or(req.scope_xml)
            .unwrap_or_else(|| FULL_XML.to_string());
        let content =
            serde_json::json!({"done": true, "xml": xml, "issues": []}).to_string();
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

/// A generate turn must receive empty memory (nothing happened yet), then
/// a follow-up refine (agent-loop) must receive the first turn's ask AND
/// outcome summary as context.
#[tokio::test]
async fn later_turns_receive_earlier_turns_as_memory() {
    let llm = Arc::new(MemoryRecordingLlm::new());
    let state = state_with(llm.clone(), Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    // Turn 1: plain generate (Fast). No prior context.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/generate"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"prompt": "draw a payment flow"}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let gen_memories = llm.generate_memories();
    assert_eq!(gen_memories.len(), 1);
    assert!(
        gen_memories[0].is_empty(),
        "first turn must have no memory: {gen_memories:?}"
    );

    // Turn 2: refine (agent-loop) on the same session. The fix call must
    // now carry the earlier ask as context.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/agent-loop"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"prompt": "make it nicer"}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let fix_memories = llm.fix_memories();
    assert_eq!(fix_memories.len(), 1, "one fix round expected");
    let joined = fix_memories[0].join("\n");
    assert!(
        joined.contains("draw a payment flow"),
        "refine must remember the earlier user ask: {fix_memories:?}"
    );
    assert!(
        joined.contains("Generated a diagram from scratch"),
        "refine must know the earlier turn's outcome: {fix_memories:?}"
    );
}

/// The session payload must expose the conversation trail (user ask +
/// agent summary) for debugging / future UI.
#[tokio::test]
async fn session_payload_includes_conversation_entries() {
    let llm = Arc::new(MemoryRecordingLlm::new());
    let state = state_with(llm.clone(), Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/generate"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"prompt": "draw a payment flow"}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

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
    let conv = parsed["conversation"]
        .as_array()
        .expect("conversation must be an array");
    assert_eq!(conv.len(), 2, "user ask + agent summary: {conv:?}");
    assert_eq!(conv[0]["role"], "user");
    assert_eq!(conv[0]["kind"], "generate");
    assert_eq!(conv[1]["role"], "agent");
    assert_eq!(conv[1]["kind"], "generate");
    assert!(
        conv[1]["version_id"].is_string(),
        "agent summary should reference the version it produced"
    );
}
