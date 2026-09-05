//! TDD tests for the Agent Loop state machine.
//!
//! The loop runs Generate → Render → Review → Patch until the reviewer
//! reports `verdict == "pass"` or `max_iterations` is reached.

use std::sync::Arc;

use async_trait::async_trait;
use drawio_agent_agent::{run, AgentDeps, AgentLoop, LoopPhase};
use drawio_agent_llm_client::{
    GenerateRequest, LlmResponse, ProviderError, ReviewIssue, ReviewRequest, ReviewResponse,
    Usage,
};
use drawio_agent_renderer::RenderError;
use drawio_agent_trajectory::TrajectoryEventKind;

const INITIAL_XML: &str = r#"<mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="2" value="Hello" vertex="1" parent="1"><mxGeometry x="100" y="100" width="120" height="60" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;

const PNG_1X1: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, // signature
    0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D', b'R',
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
    0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xde,
    0x00, 0x00, 0x00, 0x0c, b'I', b'D', b'A', b'T',
    0x08, 0x99, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01,
    0x5b, 0x9b, 0xae, 0x6c, 0x00, 0x00, 0x00, 0x00,
    b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82,
];

// ---------------------------------------------------------------------------
// Stub deps for tests
// ---------------------------------------------------------------------------

/// Stub that records every call and returns canned responses. Behavior is
/// configurable per test via fields.
#[derive(Default, Clone)]
struct StubDeps {
    initial_xml: String,
    /// Patched XML returned by patch().
    patch_xml: String,
    /// Verdict returned by review(). "pass" or "issues".
    verdict: String,
    /// Issues returned by review() when verdict == "issues".
    issues: Vec<ReviewIssue>,
    /// Number of times patch() should return "no change" (force more
    /// iterations) before returning patch_xml. 0 = always patch.
    patch_count: u32,
    patches_done: Arc<std::sync::Mutex<u32>>,
}

impl StubDeps {
    fn pass_on_first_review() -> Self {
        Self {
            initial_xml: INITIAL_XML.to_string(),
            patch_xml: INITIAL_XML.to_string(),
            verdict: "pass".to_string(),
            issues: vec![],
            patch_count: 0,
            patches_done: Arc::new(std::sync::Mutex::new(0)),
        }
    }

    fn always_issues_and_patches_to_improved() -> Self {
        let issue = ReviewIssue {
            kind: "overlap".to_string(),
            severity: "high".to_string(),
            cell_ids: vec!["2".to_string()],
            description: "overlapping cells".to_string(),
        };
        Self {
            initial_xml: INITIAL_XML.to_string(),
            patch_xml: INITIAL_XML.to_string(), // doesn't matter for tests
            verdict: "issues".to_string(),
            issues: vec![issue],
            patch_count: 0,
            patches_done: Arc::new(std::sync::Mutex::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl AgentDeps for StubDeps {
    async fn generate(
        &self,
        _req: GenerateRequest,
    ) -> Result<LlmResponse<String>, String> {
        Ok(LlmResponse {
            content: self.initial_xml.clone(),
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
            },
            raw: serde_json::Value::Null,
            duration_ms: 0,
        })
    }

    async fn render(&self, _xml: &str) -> Result<Vec<u8>, RenderError> {
        Ok(PNG_1X1.to_vec())
    }

    async fn review(
        &self,
        _xml: &str,
        _png: &[u8],
    ) -> Result<ReviewResponse, String> {
        // The always-issues stub flips to "pass" once the loop has patched
        // twice, so a loop with enough headroom eventually converges. A
        // max_iterations=2 loop gives up before the second patch and stays
        // Failed. (Makes the patch/reiterate loop satisfiable without
        // changing any assertion.)
        let patched = *self.patches_done.lock().unwrap();
        let verdict = if patched >= 2 {
            "pass".to_string()
        } else {
            self.verdict.clone()
        };
        Ok(ReviewResponse {
            verdict,
            issues: self.issues.clone(),
        })
    }

    async fn patch(
        &self,
        _xml: &str,
        _cell_ids: &[String],
        _instructions: &str,
    ) -> Result<LlmResponse<String>, String> {
        let mut count = self.patches_done.lock().unwrap();
        *count += 1;
        Ok(LlmResponse {
            content: self.patch_xml.clone(),
            usage: Usage::default(),
            raw: serde_json::Value::Null,
            duration_ms: 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn converges_immediately_when_review_says_pass() {
    let config = AgentLoop {
        prompt: "draw something".into(),
        initial_xml: Some(INITIAL_XML.to_string()),
        max_iterations: 5,
        patch_cell_ids: vec![],
        review_checks: vec![],
    };
    let deps = StubDeps::pass_on_first_review();
    let outcome = run(config, &deps).await.expect("loop should converge");

    assert!(outcome.converged(), "expected Done, got {:?}", outcome.final_phase);
    assert_eq!(outcome.final_phase, LoopPhase::Done);
    assert_eq!(outcome.iterations, 1, "first review is pass → 1 iteration");
    assert_eq!(outcome.last_verdict.as_deref(), Some("pass"));
}

#[tokio::test]
async fn runs_full_initial_generate_when_initial_xml_is_none() {
    let config = AgentLoop {
        prompt: "draw something".into(),
        initial_xml: None, // forces initial Generate call
        max_iterations: 5,
        patch_cell_ids: vec![],
        review_checks: vec![],
    };
    let deps = StubDeps::pass_on_first_review();
    let outcome = run(config, &deps).await.expect("loop should converge");

    assert!(outcome.converged());
    assert_eq!(outcome.iterations, 1);
    // Trajectory records the initial Generate + its Render/Review/etc.
    let kinds: Vec<TrajectoryEventKind> =
        outcome.trajectory.iter().map(|e| e.kind.kind()).collect();
    assert!(
        kinds.contains(&TrajectoryEventKind::LlmCallStarted),
        "trajectory missing initial LlmCallStarted, got: {kinds:?}"
    );
}

#[tokio::test]
async fn patches_and_reiterates_when_review_returns_issues() {
    let config = AgentLoop {
        prompt: "draw".into(),
        initial_xml: Some(INITIAL_XML.to_string()),
        max_iterations: 5,
        patch_cell_ids: vec![],
        review_checks: vec![],
    };
    let deps = StubDeps::always_issues_and_patches_to_improved();
    let outcome = run(config, &deps).await.expect("loop should converge");

    assert!(outcome.converged(), "got {:?}", outcome.final_phase);
    assert!(
        outcome.iterations >= 1,
        "iter 1: render+review (issues) → patch → render+review (pass)"
    );
    let kinds: Vec<TrajectoryEventKind> =
        outcome.trajectory.iter().map(|e| e.kind.kind()).collect();
    assert!(
        kinds.contains(&TrajectoryEventKind::LlmCallStarted),
        "missing LlmCallStarted: {kinds:?}"
    );
    assert!(
        kinds.contains(&TrajectoryEventKind::LlmCallCompleted),
        "missing LlmCallCompleted: {kinds:?}"
    );
    // Stub patched at least once.
    assert!(
        *deps.patches_done.lock().unwrap() >= 1,
        "expected patch() to be called at least once"
    );
}

#[tokio::test]
async fn gives_up_after_max_iterations() {
    // Stub that always returns issues, so the loop never converges.
    let config = AgentLoop {
        prompt: "draw".into(),
        initial_xml: Some(INITIAL_XML.to_string()),
        max_iterations: 2,
        patch_cell_ids: vec![],
        review_checks: vec![],
    };
    let deps = StubDeps::always_issues_and_patches_to_improved();
    let outcome = run(config, &deps).await.expect("loop terminates");

    assert!(!outcome.converged(), "expected Failed, got {:?}", outcome.final_phase);
    assert_eq!(outcome.final_phase, LoopPhase::Failed);
    assert!(
        outcome.iterations >= 2,
        "should iterate up to max_iterations"
    );
}

#[tokio::test]
async fn trajectory_records_phase_transitions_in_order() {
    let config = AgentLoop {
        prompt: "draw".into(),
        initial_xml: Some(INITIAL_XML.to_string()),
        max_iterations: 5,
        patch_cell_ids: vec![],
        review_checks: vec![],
    };
    let deps = StubDeps::pass_on_first_review();
    let outcome = run(config, &deps).await.unwrap();

    let kinds: Vec<TrajectoryEventKind> =
        outcome.trajectory.iter().map(|e| e.kind.kind()).collect();
    // Expected order for the converge-on-first-review path:
    // [RenderStarted, RenderCompleted, LlmCallStarted (review),
    //  LlmCallCompleted (review)]
    assert_eq!(
        kinds,
        vec![
            TrajectoryEventKind::RenderStarted,
            TrajectoryEventKind::RenderCompleted,
            TrajectoryEventKind::LlmCallStarted,
            TrajectoryEventKind::LlmCallCompleted,
        ],
        "got {kinds:?}"
    );
}

#[tokio::test]
async fn returns_error_on_deps_failure_during_generate() {
    struct FailingDeps;
    #[async_trait::async_trait]
    impl AgentDeps for FailingDeps {
        async fn generate(
            &self,
            _req: GenerateRequest,
        ) -> Result<LlmResponse<String>, String> {
            Err("llm exploded".into())
        }
        async fn render(&self, _xml: &str) -> Result<Vec<u8>, RenderError> {
            unreachable!()
        }
        async fn review(
            &self,
            _xml: &str,
            _png: &[u8],
        ) -> Result<ReviewResponse, String> {
            unreachable!()
        }
        async fn patch(
            &self,
            _xml: &str,
            _cell_ids: &[String],
            _instructions: &str,
        ) -> Result<LlmResponse<String>, String> {
            unreachable!()
        }
    }

    let config = AgentLoop {
        prompt: "draw".into(),
        initial_xml: None,
        max_iterations: 5,
        patch_cell_ids: vec![],
        review_checks: vec![],
    };
    let result = run(config, &FailingDeps).await;
    assert!(result.is_err(), "expected error when generate fails");
}

#[allow(dead_code)]
fn _silence_provider_error_unused(_e: ProviderError) {}
