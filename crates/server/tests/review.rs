//! TDD #5: review endpoint.
//!
//! POST /api/sessions/:id/review renders the session's current XML (or an
//! override), then sends the PNG + XML to the LLM for visual review.
//! Returns the parsed ReviewResponse (verdict + issues).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use drawio_agent_llm_client::{
    LlmProvider, LlmResponse, LlmStream, ProviderError, ReviewIssue, ReviewRequest,
    ReviewResponse, Usage,
};
use drawio_agent_renderer::{MockDriver, RenderDriver};
use drawio_agent_server::{AppState, CreateSessionRequest, ReviewRequest as RevReq};
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

struct TestLlm {
    calls: AtomicU32,
    last_image_len: Mutex<Option<usize>>,
    last_xml: Mutex<Option<String>>,
    last_checks: Mutex<Option<Vec<String>>>,
    fail_with: Mutex<Option<String>>,
}

impl TestLlm {
    fn new() -> Self {
        Self {
            calls: AtomicU32::new(0),
            last_image_len: Mutex::new(None),
            last_xml: Mutex::new(None),
            last_checks: Mutex::new(None),
            fail_with: Mutex::new(None),
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
        _req: drawio_agent_llm_client::GenerateRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        unimplemented!()
    }
    async fn generate_streaming(
        &self,
        _req: drawio_agent_llm_client::GenerateRequest,
    ) -> Result<LlmStream, ProviderError> {
        unimplemented!()
    }
    async fn review_visual(
        &self,
        req: ReviewRequest,
    ) -> Result<LlmResponse<ReviewResponse>, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_image_len.lock().unwrap() = Some(req.image_png.len());
        *self.last_xml.lock().unwrap() = Some(req.xml.clone());
        *self.last_checks.lock().unwrap() = Some(req.checks.clone());
        if let Some(msg) = self.fail_with.lock().unwrap().clone() {
            return Err(ProviderError::Provider(msg));
        }
        Ok(LlmResponse {
            content: ReviewResponse {
                verdict: "issues".to_string(),
                issues: vec![ReviewIssue {
                    kind: "overlap".to_string(),
                    severity: "high".to_string(),
                    cell_ids: vec!["5".to_string(), "7".to_string()],
                    description: "cells 5 and 7 overlap by 12px".to_string(),
                }],
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
        llm: llm,
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

async fn call_review(
    app: axum::Router,
    sid: &str,
    req: RevReq,
) -> (StatusCode, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/review"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 65536)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn review_returns_llm_review_response() {
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm.clone(), Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let (status, body) = call_review(
        app,
        &sid,
        RevReq {
            xml: None,
            checks: vec!["overlap".to_string()],
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: ReviewResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed.verdict, "issues");
    assert_eq!(parsed.issues.len(), 1);
    assert_eq!(parsed.issues[0].kind, "overlap");
    assert_eq!(parsed.issues[0].severity, "high");
    assert_eq!(parsed.issues[0].cell_ids, vec!["5", "7"]);
}

#[tokio::test]
async fn review_calls_renderer_with_current_xml() {
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm.clone(), Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    call_review(
        app,
        &sid,
        RevReq {
            xml: None,
            checks: vec![],
        },
    )
    .await;

    // LLM should have received the XML that the session was created with.
    let captured = llm.last_xml.lock().unwrap().clone().unwrap();
    assert!(captured.contains("value=\"Hello\""), "LLM got: {captured}");
}

#[tokio::test]
async fn review_accepts_xml_override_in_request() {
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm.clone(), Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let override_xml = r#"<mxfile><diagram id="o"><mxGraphModel><root><mxCell id="0"/><mxCell id="9" value="OVERRIDE"/></root></mxGraphModel></diagram></mxfile>"#;

    call_review(
        app,
        &sid,
        RevReq {
            xml: Some(override_xml.to_string()),
            checks: vec![],
        },
    )
    .await;

    let captured = llm.last_xml.lock().unwrap().clone().unwrap();
    assert!(
        captured.contains("OVERRIDE"),
        "override XML should reach LLM, got: {captured}"
    );
}

#[tokio::test]
async fn review_propagates_checks_list_to_llm() {
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm.clone(), Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    call_review(
        app,
        &sid,
        RevReq {
            xml: None,
            checks: vec![
                "overlap".to_string(),
                "text_overflow".to_string(),
                "edge_crossing".to_string(),
            ],
        },
    )
    .await;

    let captured = llm.last_checks.lock().unwrap().clone().unwrap();
    assert_eq!(captured.len(), 3);
    assert!(captured.contains(&"overlap".to_string()));
    assert!(captured.contains(&"text_overflow".to_string()));
    assert!(captured.contains(&"edge_crossing".to_string()));
}

#[tokio::test]
async fn review_passes_image_bytes_from_renderer_to_llm() {
    let llm = Arc::new(TestLlm::new());
    let renderer: Arc<dyn RenderDriver> =
        Arc::new(MockDriver::new().with_bytes(vec![0xAA; 2048]));
    let state = state_with(llm.clone(), renderer);
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    call_review(
        app,
        &sid,
        RevReq {
            xml: None,
            checks: vec![],
        },
    )
    .await;

    let len = llm.last_image_len.lock().unwrap().expect("image captured");
    assert_eq!(len, 2048, "LLM should get the rendered PNG bytes (2048)");
}

#[tokio::test]
async fn review_returns_404_for_unknown_session() {
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state);
    let (status, _) = call_review(
        app,
        "no-such",
        RevReq {
            xml: None,
            checks: vec![],
        },
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn review_returns_400_when_no_xml_available() {
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state);

    // Create empty session (no initial_xml)
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

    // Review without override fails
    let (status, _) = call_review(
        app,
        &sid,
        RevReq {
            xml: None,
            checks: vec![],
        },
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn review_returns_502_when_llm_errors() {
    let llm = Arc::new(TestLlm::new());
    llm.set_failure("vlm down");
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let (status, _) = call_review(
        app,
        &sid,
        RevReq {
            xml: None,
            checks: vec![],
        },
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn review_returns_503_when_renderer_fails() {
    let llm = Arc::new(TestLlm::new());
    let failing_renderer: Arc<dyn RenderDriver> =
        Arc::new(MockDriver::new().with_error("chrome keychain".to_string()));
    let state = state_with(llm, failing_renderer);
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let (status, _) = call_review(
        app,
        &sid,
        RevReq {
            xml: None,
            checks: vec![],
        },
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn review_with_xml_override_skips_current_xml_check() {
    // Even with no current XML, an explicit override should work.
    let llm = Arc::new(TestLlm::new());
    let state = state_with(llm, Arc::new(MockDriver::new()));
    let app = router(state);

    // Create empty session.
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

    let (status, body) = call_review(
        app,
        &sid,
        RevReq {
            xml: Some(FULL_XML.to_string()),
            checks: vec![],
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: ReviewResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed.verdict, "issues");
}

#[allow(dead_code)]
fn _silence_unused(_p: base64::engine::general_purpose::GeneralPurpose) {}
