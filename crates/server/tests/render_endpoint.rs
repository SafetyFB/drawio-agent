//! TDD #4: render endpoint.
//!
//! POST /api/sessions/:id/render takes the session's current XML and asks the
//! configured RenderDriver for PNG bytes. Status codes:
//! - 200 with PNG bytes when both session and renderer are healthy
//! - 404 for unknown session
//! - 400 for a session that exists but has no current XML
//! - 503 when the renderer itself fails

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use drawio_agent_llm_client::{
    GenerateRequest, LlmProvider, LlmResponse, LlmStream, ProviderError, ReviewRequest,
    ReviewResponse,
};
use drawio_agent_renderer::{MockDriver, RenderDriver};
use drawio_agent_server::{AppState, CreateSessionRequest, RenderResponse};
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

struct StubLlm;

#[async_trait::async_trait]
impl LlmProvider for StubLlm {
    fn name(&self) -> &str {
        "stub"
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
}

fn state_with_renderer(renderer: Arc<dyn RenderDriver>) -> Arc<AppState> {
    Arc::new(AppState {
        sessions: Arc::new(tokio::sync::RwLock::new(
            drawio_agent_server::SessionStore::new(),
        )),
        llm: Arc::new(drawio_agent_server::RuntimeLlm::new(Arc::new(StubLlm))),
        renderer,
        events: drawio_agent_server::EventBus::new(),
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

#[tokio::test]
async fn render_returns_404_for_unknown_session() {
    let state = state_with_renderer(Arc::new(MockDriver::new()));
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/no-such/render")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn render_returns_400_when_session_has_no_current_xml() {
    let state = state_with_renderer(Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_empty_session(app.clone()).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/render"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn render_returns_200_with_valid_png_when_session_has_xml() {
    let state = state_with_renderer(Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/render"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 65536)
        .await
        .unwrap();
    let parsed: RenderResponse = serde_json::from_slice(&body).expect("valid RenderResponse");
    let png = base64::engine::general_purpose::STANDARD
        .decode(&parsed.png_base64)
        .expect("base64 decodes");
    assert!(
        png.len() > 50,
        "PNG should have non-trivial size, got {} bytes",
        png.len()
    );
    // PNG signature
    assert_eq!(
        &png[..8],
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
        "output must start with PNG signature"
    );
}

#[tokio::test]
async fn render_response_bytes_count_matches_png_size() {
    let state = state_with_renderer(Arc::new(MockDriver::new()));
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/render"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 65536)
        .await
        .unwrap();
    let parsed: RenderResponse = serde_json::from_slice(&body).unwrap();
    let png = base64::engine::general_purpose::STANDARD
        .decode(&parsed.png_base64)
        .unwrap();
    assert_eq!(parsed.bytes, png.len());
}

#[tokio::test]
async fn render_returns_503_when_renderer_errors() {
    let failing = Arc::new(
        MockDriver::new().with_error("headless chrome not available".to_string()),
    );
    let state = state_with_renderer(failing);
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/render"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn render_respects_configured_png_size() {
    // Configure MockDriver to return a known payload; assert the response
    // surfaces the same size.
    let driver: Arc<dyn RenderDriver> = Arc::new(
        MockDriver::new().with_bytes(vec![0u8; 1024]),
    );
    let state = state_with_renderer(driver);
    let app = router(state);
    let sid = create_session_with_xml(app.clone(), FULL_XML).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/render"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 65536)
        .await
        .unwrap();
    let parsed: RenderResponse = serde_json::from_slice(&body).unwrap();
    let png = base64::engine::general_purpose::STANDARD
        .decode(&parsed.png_base64)
        .unwrap();
    assert_eq!(png.len(), 1024);
}
