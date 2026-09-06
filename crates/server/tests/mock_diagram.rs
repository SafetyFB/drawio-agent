//! The mock LLM (StubLlm) must return a diagram that exercises the full
//! shape vocabulary — not just two plain rectangles — so mock sessions look
//! like real LLM output in the UI (ellipses, a rhombus/decision, a cylinder,
//! labeled + dashed edges).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drawio_agent_server::{
    build_app_state, build_router, LlmProviderKind, RendererKind, ServerConfig,
};
use tower::ServiceExt;

async fn mock_state() -> Arc<drawio_agent_server::AppState> {
    let config = ServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        llm_provider: LlmProviderKind::Mock,
        renderer: RendererKind::Mock,
        static_dir: None,
    };
    build_app_state(&config).await.map(Arc::new).unwrap()
}

/// Recursively collect every cell (root + descendants).
fn collect_cells<'a>(
    cell: &'a drawio_agent_xml_core::Cell,
    out: &mut Vec<&'a drawio_agent_xml_core::Cell>,
) {
    out.push(cell);
    for child in &cell.children {
        collect_cells(child, out);
    }
}

#[tokio::test]
async fn mock_generate_returns_rich_shape_vocabulary() {
    let app = build_router((*mock_state().await).clone());

    // Create an empty session, then generate (fresh → no current_xml, so the
    // stub returns its fixed MOCK_DIAGRAM).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&drawio_agent_server::CreateSessionRequest::default())
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

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/generate"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"prompt": "draw something"})).unwrap(),
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

    // Parses as a valid mxfile with a model.
    let file = drawio_agent_xml_core::MxFile::parse(gen.xml.as_bytes())
        .expect("mock generate XML must parse as a valid mxfile");
    let model = file.diagrams[0]
        .model
        .as_ref()
        .expect("diagram must have a model");

    // At least 5 visible cells (the current mock has 12: 6 vertices + 6 edges).
    let mut cells = Vec::new();
    collect_cells(&model.root, &mut cells);
    let visible: Vec<_> = cells
        .iter()
        .filter(|c| c.id != "0" && c.id != "1")
        .collect();
    assert!(
        visible.len() >= 5,
        "expected >= 5 cells, got {}",
        visible.len()
    );

    // Non-rectangle shapes must be present: an ellipse, a rhombus, a cylinder.
    let all_styles = visible
        .iter()
        .filter_map(|c| c.style.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    for needle in ["ellipse", "rhombus", "shape=cylinder3"] {
        assert!(
            all_styles.contains(needle),
            "mock diagram must include a '{needle}' style, got:\n{all_styles}"
        );
    }
}