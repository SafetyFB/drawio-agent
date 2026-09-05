//! TDD #1: session lifecycle endpoints (create / get / list / 404).
//!
//! Uses `tower::ServiceExt::oneshot` to drive the router directly — no
//! network socket needed.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drawio_agent_llm_client::{
    GenerateRequest, LlmProvider, LlmResponse, LlmStream, ProviderError, ReviewRequest,
};
use drawio_agent_renderer::MockDriver;
use drawio_agent_server::{
    AppState, CreateSessionRequest, CreateSessionResponse, GenerateRequest as GenReq,
    PatchRequest as PatchReq, ReviewRequest as RevReq, SessionInfoResponse, VersionsResponse,
};
use serde_json::json;
use tower::ServiceExt;
use uuid::Uuid;

/// Minimal LlmProvider stub for session-lifecycle tests (doesn't exercise
/// generate/review logic — those have their own TDDs).
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
    ) -> Result<LlmResponse<drawio_agent_llm_client::ReviewResponse>, ProviderError> {
        unimplemented!()
    }
}

fn test_state() -> Arc<AppState> {
    Arc::new(AppState::with_mock_renderer(Arc::new(StubLlm)))
}

fn router(state: Arc<AppState>) -> axum::Router {
    drawio_agent_server::build_router((*state).clone())
}

#[tokio::test]
async fn health_returns_ok() {
    let app = router(test_state());

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn create_session_returns_session_id() {
    let app = router(test_state());

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
    let parsed: CreateSessionResponse = serde_json::from_slice(&body).unwrap();
    assert!(!parsed.session_id.as_str().is_empty());
}

#[tokio::test]
async fn create_then_get_returns_same_session() {
    let app = router(test_state());

    // Create.
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
    let created: CreateSessionResponse = serde_json::from_slice(&body).unwrap();
    let id = created.session_id.as_str().to_string();

    // Get.
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 4096)
        .await
        .unwrap();
    let got: SessionInfoResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(got.id.as_str(), id);
    assert_eq!(got.meta.version_count, 0);
    assert!(got.current_xml.is_none());
}

#[tokio::test]
async fn get_unknown_session_returns_404() {
    let app = router(test_state());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/sessions/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn new_session_versions_list_is_empty() {
    let app = router(test_state());

    // Create.
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
    let created: CreateSessionResponse = serde_json::from_slice(&body).unwrap();
    let id = created.session_id.as_str().to_string();

    // List versions.
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{id}/versions"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 4096)
        .await
        .unwrap();
    let parsed: VersionsResponse = serde_json::from_slice(&body).unwrap();
    assert!(parsed.versions.is_empty());
}

#[tokio::test]
async fn versions_for_unknown_session_returns_404() {
    let app = router(test_state());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/sessions/no-such/versions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn generate_returns_501_for_unknown_session() {
    let app = router(test_state());
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
async fn patch_returns_501_for_unknown_session() {
    let app = router(test_state());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/no-such/patch")
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
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn review_returns_501_for_unknown_session() {
    let app = router(test_state());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/no-such/review")
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
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// Reference imports we don't actually use yet (silence warnings).
#[allow(dead_code)]
fn _phantom(_g: GenReq, _p: PatchReq, _r: RevReq, _id: Uuid) {}
