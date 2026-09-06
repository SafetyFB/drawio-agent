//! TDD #3: patch endpoint.
//!
//! POST /api/sessions/:id/patch extracts the subgraph for `cell_ids`,
//! sends it to the LLM with the instruction, applies the LLM's updated
//! subgraph back to the model, and stores the new version.
//!
//! Critical invariant: cells NOT in `cell_ids` must remain byte-identical.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drawio_agent_llm_client::{
    GenerateRequest, LlmProvider, LlmResponse, LlmStream, ProviderError, ReviewRequest,
    ReviewResponse, Usage,
};
use drawio_agent_server::{
    AppState, CreateSessionRequest, GenerateRequest as GenReq, PatchRequest as PatchReq,
    PatchResponse,
};
use serde_json::Value;
use tower::ServiceExt;

const FULL_XML: &str = r#"<mxfile host="app.diagrams.net">
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
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

/// Patched subgraph the LLM "returns": cell 2's value changed to Modified,
/// everything else byte-identical to the original. (Same as if the LLM
/// just touched the selected cell and left the rest alone.)
const PATCHED_SUBGRAPH_XML: &str = r#"<mxfile>
  <diagram id="d">
    <mxGraphModel>
      <root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="2" value="Modified" style="rounded=0;" vertex="1" parent="1">
          <mxGeometry x="100" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="3" value="World" style="rounded=0;" vertex="1" parent="1">
          <mxGeometry x="300" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="4" style="edgeStyle=orthogonalEdgeStyle;" edge="1" parent="1" source="2" target="3">
          <mxGeometry relative="1" as="geometry"/>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

struct TestLlm {
    calls: AtomicU32,
    last_scope: Mutex<Option<String>>,
    last_instruction: Mutex<Option<String>>,
    last_cell_ids: Mutex<Option<Vec<String>>>,
    response_xml: Mutex<Option<String>>,
    fail_with: Mutex<Option<String>>,
}

impl TestLlm {
    fn new() -> Self {
        Self {
            calls: AtomicU32::new(0),
            last_scope: Mutex::new(None),
            last_instruction: Mutex::new(None),
            last_cell_ids: Mutex::new(None),
            response_xml: Mutex::new(Some(PATCHED_SUBGRAPH_XML.to_string())),
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
        req: GenerateRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_scope.lock().unwrap() = req.scope.clone();
        *self.last_instruction.lock().unwrap() = Some(req.user_prompt.clone());
        *self.last_cell_ids.lock().unwrap() = req
            .scope
            .as_ref()
            .and_then(|s| extract_cell_ids_from_scope(s));
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
}

/// Best-effort extract cell ids from a scope XML by scanning for `id="N"`.
/// Used for test assertions only — not the same code path the handler uses.
fn extract_cell_ids_from_scope(scope: &str) -> Option<Vec<String>> {
    let mut ids = Vec::new();
    let bytes = scope.as_bytes();
    let mut i = 0;
    while i + 4 < bytes.len() {
        if &bytes[i..i + 4] == b"id=\"" {
            let start = i + 4;
            if let Some(end) = bytes[start..].iter().position(|&b| b == b'"') {
                let id = std::str::from_utf8(&bytes[start..start + end])
                    .ok()?
                    .to_string();
                if !ids.contains(&id) {
                    ids.push(id);
                }
                i = start + end + 1;
                continue;
            }
        }
        i += 1;
    }
    Some(ids)
}

fn test_state(llm: Arc<TestLlm>) -> Arc<AppState> {
    let llm_dyn: Arc<dyn LlmProvider> = llm;
    Arc::new(AppState::with_mock_renderer(llm_dyn))
}

fn router(state: Arc<AppState>) -> axum::Router {
    drawio_agent_server::build_router((*state).clone())
}

async fn create_session_with_full_xml(app: axum::Router) -> String {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&CreateSessionRequest {
                        initial_xml: Some(FULL_XML.to_string()),
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

async fn call_patch(app: axum::Router, sid: &str, req: PatchReq) -> (StatusCode, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/patch"))
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
async fn patch_extracts_subgraph_and_calls_llm_with_scope() {
    let llm = Arc::new(TestLlm::new());
    let state = test_state(llm.clone());
    let app = router(state);
    let sid = create_session_with_full_xml(app.clone()).await;

    let (status, _body) = call_patch(
        app,
        &sid,
        PatchReq {
            cell_ids: vec!["2".into()],
            instruction: "rename to Modified".into(),
            json_mode: false,
        },
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
    // The LLM received a scope containing at least the requested cell.
    let scope = llm.last_scope.lock().unwrap().clone().unwrap();
    assert!(
        scope.contains("id=\"2\""),
        "LLM scope must include cell 2: {scope}"
    );
    // And the instruction is preserved in the user prompt.
    let instr = llm.last_instruction.lock().unwrap().clone().unwrap();
    assert!(
        instr.contains("rename to Modified"),
        "instruction must be in user prompt: {instr}"
    );
}

#[tokio::test]
async fn patch_returns_404_for_unknown_session() {
    let state = test_state(Arc::new(TestLlm::new()));
    let app = router(state);
    let (status, _) = call_patch(
        app,
        "no-such",
        PatchReq {
            cell_ids: vec!["2".into()],
            instruction: "x".into(),
            json_mode: false,
        },
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn patch_returns_400_when_session_has_no_current_xml() {
    let state = test_state(Arc::new(TestLlm::new()));
    let app = router(state);
    // Create an empty session (no initial_xml, no generate yet).
    let sid = {
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
        let parsed: drawio_agent_server::CreateSessionResponse =
            serde_json::from_slice(&body).unwrap();
        parsed.session_id.as_str().to_string()
    };

    let (status, _) = call_patch(
        app,
        &sid,
        PatchReq {
            cell_ids: vec!["2".into()],
            instruction: "x".into(),
            json_mode: false,
        },
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn patch_returns_502_when_llm_errors() {
    let llm = Arc::new(TestLlm::new());
    llm.set_failure("llm down");
    let state = test_state(llm);
    let app = router(state);
    let sid = create_session_with_full_xml(app.clone()).await;

    let (status, _) = call_patch(
        app,
        &sid,
        PatchReq {
            cell_ids: vec!["2".into()],
            instruction: "x".into(),
            json_mode: false,
        },
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn patch_preserves_unrelated_cells() {
    let state = test_state(Arc::new(TestLlm::new()));
    let app = router(state);
    let sid = create_session_with_full_xml(app.clone()).await;

    let (status, body) = call_patch(
        app,
        &sid,
        PatchReq {
            cell_ids: vec!["2".into()],
            instruction: "x".into(),
            json_mode: false,
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let parsed: PatchResponse = serde_json::from_str(&body).unwrap();
    // The patched cell value must reflect the LLM's change.
    assert!(
        parsed.xml.contains("value=\"Modified\""),
        "patched cell 2 must have new value: {}",
        parsed.xml
    );
    // The non-selected cell (cell 3) must keep its original value.
    assert!(
        parsed.xml.contains("value=\"World\""),
        "unselected cell 3 must be preserved: {}",
        parsed.xml
    );
    // The edge (cell 4) must still connect 2 → 3.
    assert!(
        parsed.xml.contains("source=\"2\""),
        "edge source must be preserved: {}",
        parsed.xml
    );
    assert!(
        parsed.xml.contains("target=\"3\""),
        "edge target must be preserved: {}",
        parsed.xml
    );
}

#[tokio::test]
async fn patch_appends_new_version_with_kind_patch() {
    let state = test_state(Arc::new(TestLlm::new()));
    let app = router(state);
    let sid = create_session_with_full_xml(app.clone()).await;

    call_patch(
        app.clone(),
        &sid,
        PatchReq {
            cell_ids: vec!["2".into()],
            instruction: "x".into(),
            json_mode: false,
        },
    )
    .await;

    // List versions.
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
    // create_session_with_full_xml stores the initial XML as version 0;
    // patch adds version 1 with kind "patch".
    assert_eq!(parsed.versions.len(), 2, "patch must add one version on top of the initial");
    assert_eq!(parsed.versions.last().unwrap().kind, "patch");
}

#[tokio::test]
async fn patch_propagates_multiple_cell_ids_to_scope() {
    let llm = Arc::new(TestLlm::new());
    let state = test_state(llm.clone());
    let app = router(state);
    let sid = create_session_with_full_xml(app.clone()).await;

    call_patch(
        app,
        &sid,
        PatchReq {
            cell_ids: vec!["2".into(), "3".into()],
            instruction: "color both".into(),
            json_mode: false,
        },
    )
    .await;

    let captured = llm.last_cell_ids.lock().unwrap().clone().unwrap();
    assert!(captured.contains(&"2".to_string()));
    assert!(captured.contains(&"3".to_string()));
}

// Suppress unused-import warnings from items we keep for type clarity.
#[allow(dead_code)]
fn _phantom(_: GenReq) {}
