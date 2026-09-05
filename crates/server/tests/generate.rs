//! TDD #2: generate endpoint.
//!
//! POST /api/sessions/:id/generate calls the LLM, stores the resulting XML as
//! a new version, and returns `{xml, version_id}`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drawio_agent_llm_client::{
    GenerateRequest, LlmProvider, LlmResponse, LlmStream, ProviderError, ReviewRequest,
    ReviewResponse, Usage,
};
use drawio_agent_server::{
    AppState, CreateSessionRequest, GenerateRequest as GenReq, GenerateResponse,
    SessionInfoResponse, VersionsResponse,
};
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

/// Configurable LLM stub. Returns whatever `set_response` was called with;
/// tracks the most recent request for assertions; can be flipped to error.
#[derive(Default)]
struct TestLlm {
    calls: AtomicU32,
    last_prompt: std::sync::Mutex<Option<String>>,
    last_json_mode: std::sync::Mutex<Option<bool>>,
    response_xml: std::sync::Mutex<Option<String>>,
    fail_with: std::sync::Mutex<Option<String>>,
}

impl TestLlm {
    fn new(response_xml: &str) -> Self {
        Self {
            response_xml: std::sync::Mutex::new(Some(response_xml.to_string())),
            ..Default::default()
        }
    }

    fn set_failure(&self, msg: &str) {
        *self.fail_with.lock().unwrap() = Some(msg.to_string());
    }

    fn call_count(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }

    fn last_request(&self) -> (Option<String>, Option<bool>) {
        (
            self.last_prompt.lock().unwrap().clone(),
            *self.last_json_mode.lock().unwrap(),
        )
    }
}

#[async_trait::async_trait]
impl LlmProvider for TestLlm {
    fn name(&self) -> &str {
        "test"
    }

    async fn generate_xml(
        &self,
        req: GenerateRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_prompt.lock().unwrap() = Some(req.user_prompt.clone());
        *self.last_json_mode.lock().unwrap() = Some(req.json_mode);

        if let Some(msg) = self.fail_with.lock().unwrap().clone() {
            return Err(ProviderError::Provider(msg));
        }
        let xml = self
            .response_xml
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "<mxfile/>".to_string());
        Ok(LlmResponse {
            content: xml,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
            },
            raw: Value::Null,
            duration_ms: 42,
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
}

const SAMPLE_XML: &str = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="2" value="A" vertex="1" parent="1"><mxGeometry x="100" y="100" width="120" height="60" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;

fn test_state(llm: Arc<TestLlm>) -> Arc<AppState> {
    let llm_dyn: Arc<dyn LlmProvider> = llm;
    Arc::new(AppState::with_mock_renderer(llm_dyn))
}

fn router(state: Arc<AppState>) -> axum::Router {
    drawio_agent_server::build_router((*state).clone())
}

async fn create_session(app: axum::Router) -> String {
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

#[tokio::test]
async fn generate_calls_llm_and_returns_versioned_xml() {
    let llm = Arc::new(TestLlm::new(SAMPLE_XML));
    let state = test_state(llm.clone());
    let app = router(state);
    let session_id = create_session(app.clone()).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{session_id}/generate"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&GenReq {
                        prompt: "draw a box".into(),
                        json_mode: false,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 4096)
        .await
        .unwrap();
    let gen: GenerateResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(gen.xml, SAMPLE_XML);
    assert_eq!(llm.call_count(), 1);
    let (prompt, json_mode) = llm.last_request();
    assert_eq!(prompt.as_deref(), Some("draw a box"));
    assert_eq!(json_mode, Some(false));
}

#[tokio::test]
async fn generate_xml_becomes_current_session_xml() {
    let llm = Arc::new(TestLlm::new(SAMPLE_XML));
    let state = test_state(llm.clone());
    let app = router(state);
    let session_id = create_session(app.clone()).await;

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{session_id}/generate"))
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

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{session_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 4096)
        .await
        .unwrap();
    let info: SessionInfoResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(info.current_xml.as_deref(), Some(SAMPLE_XML));
    assert_eq!(info.meta.version_count, 1);
}

#[tokio::test]
async fn generate_appends_to_version_history() {
    let llm = Arc::new(TestLlm::new(SAMPLE_XML));
    let state = test_state(llm.clone());
    let app = router(state);
    let session_id = create_session(app.clone()).await;

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{session_id}/generate"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&GenReq {
                        prompt: "first".into(),
                        json_mode: false,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{session_id}/generate"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&GenReq {
                        prompt: "second".into(),
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
                .uri(format!("/api/sessions/{session_id}/versions"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 4096)
        .await
        .unwrap();
    let parsed: VersionsResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed.versions.len(), 2);
    // Both versions tagged "generate"
    assert!(parsed.versions.iter().all(|v| v.kind == "generate"));
}

#[tokio::test]
async fn generate_passes_json_mode_to_llm() {
    let llm = Arc::new(TestLlm::new(SAMPLE_XML));
    let state = test_state(llm.clone());
    let app = router(state);
    let session_id = create_session(app.clone()).await;

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{session_id}/generate"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&GenReq {
                        prompt: "structured".into(),
                        json_mode: true,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let (_prompt, json_mode) = llm.last_request();
    assert_eq!(json_mode, Some(true));
}

#[tokio::test]
async fn generate_returns_404_for_unknown_session() {
    let state = test_state(Arc::new(TestLlm::new(SAMPLE_XML)));
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/no-such/generate")
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
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn generate_returns_502_when_llm_errors() {
    let llm = Arc::new(TestLlm::new(SAMPLE_XML));
    llm.set_failure("upstream exploded");
    let state = test_state(llm);
    let app = router(state);
    let session_id = create_session(app.clone()).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{session_id}/generate"))
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

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn generate_records_summary_in_version_history() {
    let llm = Arc::new(TestLlm::new(SAMPLE_XML));
    let state = test_state(llm.clone());
    let app = router(state);
    let session_id = create_session(app.clone()).await;

    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{session_id}/generate"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&GenReq {
                        prompt: "a long prompt that should be truncated because it exceeds eighty chars".into(),
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
                .uri(format!("/api/sessions/{session_id}/versions"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 4096)
        .await
        .unwrap();
    let parsed: VersionsResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed.versions.len(), 1);
    let summary = parsed.versions[0].summary.as_deref().unwrap_or("");
    // Summary is truncated to ≤80 chars
    assert!(summary.chars().count() <= 80, "summary too long: {summary}");
    assert!(summary.starts_with("a long prompt"));
}

#[allow(dead_code)]
fn _phantom(_id: Uuid) {}
