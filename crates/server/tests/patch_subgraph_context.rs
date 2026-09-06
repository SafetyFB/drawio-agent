//! Plan B: the patch handler sends ONLY the selected subgraph as the LLM's
//! scope (never the full diagram), and `apply_subgraph` preserves new child
//! cells introduced by the LLM.

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
    AppState, CreateSessionRequest, PatchRequest as PatchReq, PatchResponse,
};
use drawio_agent_xml_core::{Cell, MxFile, Subgraph};
use serde_json::Value;
use tower::ServiceExt;

/// Two selected-able cells plus an unrelated cell (5) with no edges, so a
/// scope for `["2"]` must NOT include cell 5.
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
        <mxCell id="5" value="Unrelated" style="rounded=0;" vertex="1" parent="1">
          <mxGeometry x="600" y="300" width="120" height="60" as="geometry"/>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

/// What the mock LLM "returns": cell 2 modified, nothing else.
const PATCHED_SUBGRAPH_XML: &str = r#"<mxfile>
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

struct TestLlm {
    calls: AtomicU32,
    last_scope: Mutex<Option<String>>,
    last_current_xml: Mutex<Option<String>>,
    response_xml: Mutex<String>,
}

impl TestLlm {
    fn new() -> Self {
        Self {
            calls: AtomicU32::new(0),
            last_scope: Mutex::new(None),
            last_current_xml: Mutex::new(None),
            response_xml: Mutex::new(PATCHED_SUBGRAPH_XML.to_string()),
        }
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
        *self.last_current_xml.lock().unwrap() = req.current_xml.clone();
        let xml = self.response_xml.lock().unwrap().clone();
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

#[tokio::test]
async fn patch_sends_subgraph_scope_not_full_xml() {
    let llm = Arc::new(TestLlm::new());
    let state = test_state(llm.clone());
    let app = router(state);
    let sid = create_session_with_full_xml(app.clone()).await;

    // The session's stored XML is the full diagram.
    let resp = app
        .clone()
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
    let parsed: drawio_agent_server::SessionInfoResponse = serde_json::from_slice(&body).unwrap();
    let full_xml = parsed.current_xml.expect("session has current XML");

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/patch"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&PatchReq {
                        cell_ids: vec!["2".into()],
                        instruction: "make it red".into(),
                        json_mode: false,
            no_think: false,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(llm.calls.load(Ordering::SeqCst), 1);

    // The LLM must NOT receive the full diagram.
    let current_xml = llm.last_current_xml.lock().unwrap().clone();
    assert!(
        current_xml.is_none(),
        "patch must not send the full diagram as current_xml"
    );

    let scope = llm.last_scope.lock().unwrap().clone().expect("scope present");
    assert!(
        scope.len() < full_xml.len(),
        "scope ({}) must be shorter than full XML ({})",
        scope.len(),
        full_xml.len()
    );
    assert!(
        scope.contains("id=\"2\""),
        "scope must include the targeted cell: {scope}"
    );
    assert!(
        !scope.contains("Unrelated"),
        "scope must NOT include unrelated cell 5: {scope}"
    );
}

#[tokio::test]
async fn patch_still_applies_subgraph_back() {
    let llm = Arc::new(TestLlm::new());
    let state = test_state(llm);
    let app = router(state);
    let sid = create_session_with_full_xml(app.clone()).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/sessions/{sid}/patch"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&PatchReq {
                        cell_ids: vec!["2".into()],
                        instruction: "make it red".into(),
                        json_mode: false,
            no_think: false,
                    })
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
    let parsed: PatchResponse = serde_json::from_slice(&body).unwrap();
    assert!(
        parsed.xml.contains("value=\"Modified\""),
        "patched cell must reflect the LLM's change: {}",
        parsed.xml
    );
}

#[tokio::test]
async fn apply_subgraph_preserves_new_children() {
    // Model: cell 2 ("A") already has one child, 11.
    const MODEL_XML: &str = r#"<mxfile><diagram id="d"><mxGraphModel><root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="2" value="A" vertex="1" parent="1">
          <mxGeometry x="0" y="0" width="100" height="100" as="geometry"/>
        </mxCell>
        <mxCell id="11" value="Child" vertex="1" parent="2">
          <mxGeometry x="10" y="10" width="40" height="20" as="geometry"/>
        </mxCell>
    </root></mxGraphModel></diagram></mxfile>"#;
    let file = MxFile::parse(MODEL_XML.as_bytes()).unwrap();
    let mut model = file.diagrams[0].model.clone().unwrap();

    // Replacement cell 2 carries the existing child 11 PLUS a new child 100
    // (what the LLM "added", e.g. a new arrow).
    let mut replacement = Cell::new("2");
    replacement.value = Some("A".into());
    replacement.vertex = true;
    replacement.parent = Some("1".into());
    replacement.children.push(Cell::new("11"));
    let mut new_child = Cell::new("100");
    new_child.value = Some("Arrow".into());
    new_child.edge = true;
    new_child.parent = Some("2".into());
    replacement.children.push(new_child);

    let sub = Subgraph {
        primary: vec![replacement],
        edges: vec![],
        context: vec![],
        parents: vec![],
        missing: vec![],
    };
    let result = model.apply_subgraph(&sub);
    assert!(result.updated.contains(&"2".to_string()));

    let cell = model.get("2").unwrap();
    let child_ids: Vec<&str> = cell.children.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        child_ids,
        vec!["11", "100"],
        "existing child preserved AND new child appended, no duplicates"
    );
}

#[tokio::test]
async fn extract_subgraph_includes_parents() {
    // Swimlane 10 contains cell 11.
    const MODEL_XML: &str = r#"<mxfile><diagram id="d"><mxGraphModel><root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="10" value="Swimlane" vertex="1" parent="1">
          <mxGeometry x="0" y="0" width="400" height="200" as="geometry"/>
        </mxCell>
        <mxCell id="11" value="Inside" vertex="1" parent="10">
          <mxGeometry x="20" y="20" width="80" height="40" as="geometry"/>
        </mxCell>
    </root></mxGraphModel></diagram></mxfile>"#;
    let file = MxFile::parse(MODEL_XML.as_bytes()).unwrap();
    let model = file.diagrams[0].model.clone().unwrap();

    let sub = model.extract_subgraph(&["11"]);
    assert_eq!(sub.primary[0].id, "11");

    let parent_ids: Vec<&str> = sub.parents.iter().map(|c| c.id.as_str()).collect();
    assert!(
        parent_ids.contains(&"10"),
        "swimlane must be included in parents: {parent_ids:?}"
    );
}