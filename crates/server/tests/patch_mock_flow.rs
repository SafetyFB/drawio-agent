//! Regression: mock-LLM patch flow.
//!
//! StubLlm used to return `<mxfile/>` (no `<diagram>`) for every call, so the
//! patch handler recorded LlmCallCompleted and then failed parsing the LLM
//! response with "no diagram in LLM response" — the client never received new
//! XML and the canvas never updated. The mock now echoes the request's
//! `current_xml` (and returns a small fixed diagram for a fresh generate), so
//! a mock-mode patch succeeds end to end.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drawio_agent_llm_client::LlmProvider;
use drawio_agent_renderer::MockDriver;
use drawio_agent_server::{build_app_state_with_renderer, build_router, LlmSettings, ServerConfig};
use tower::ServiceExt;

const CELL_XML: &str = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="2" value="A" vertex="1" parent="1"><mxGeometry x="100" y="100" width="120" height="60" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;

async fn mock_state() -> Arc<drawio_agent_server::AppState> {
    let config = ServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        static_dir: None,
        config_path: None,
        llm: Some(LlmSettings {
            kind: drawio_agent_server::LlmKind::Mock,
            ..Default::default()
        }),
    };
    let _: Arc<dyn LlmProvider> = Arc::new(drawio_agent_server::StubLlm);
    build_app_state_with_renderer(&config, Some(Arc::new(MockDriver::new())))
        .await
        .map(Arc::new)
        .unwrap()
}

fn app(state: Arc<drawio_agent_server::AppState>) -> axum::Router {
    build_router((*state).clone())
}

#[tokio::test]
async fn mock_patch_returns_valid_xml_not_an_error() {
    let app = app(mock_state().await);

    // Create a session with real cells so patch has something to select.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&drawio_agent_server::CreateSessionRequest {
                        initial_xml: Some(CELL_XML.to_string()),
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
    let sid = parsed.session_id.as_str().to_string();

    // Patch cell "2".
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/patch"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&drawio_agent_server::PatchRequest {
                        cell_ids: vec!["2".into()],
                        no_think: false,
            instruction: "make it bigger".into(),
                        json_mode: false,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "mock patch must succeed (previously: 502 'no diagram in LLM response')"
    );
    let body = axum::body::to_bytes(resp.into_body(), 65536)
        .await
        .unwrap();
    let parsed: drawio_agent_server::PatchResponse = serde_json::from_slice(&body).unwrap();
    assert!(
        parsed.xml.contains("<mxGraphModel>"),
        "patch response must be a valid diagram, got: {}",
        parsed.xml.chars().take(200).collect::<String>()
    );
    assert!(parsed.xml.contains("id=\"2\""), "patched XML should keep cell 2");
}