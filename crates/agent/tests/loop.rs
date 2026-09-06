//! TDD tests for the Agent Loop state machine (v2 single-context loop).
//!
//! The loop runs Generate (baseline) → per-round Render → ONE multimodal
//! fix call until a round reports `done=true` or `max_iterations` is hit.

use std::sync::Arc;

use drawio_agent_agent::{run, AgentDeps, AgentLoop, FixError, FixOutcome, FixRequest, LoopPhase};
use drawio_agent_llm_client::{GenerateRequest, LlmResponse, ReviewIssue, Usage};
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

fn issue(kind: &str, cell_ids: &[&str]) -> ReviewIssue {
    ReviewIssue {
        kind: kind.to_string(),
        severity: "high".to_string(),
        cell_ids: cell_ids.iter().map(|s| s.to_string()).collect(),
        description: format!("{kind} on {}", cell_ids.join(",")),
    }
}

// ---------------------------------------------------------------------------
// Scripted stub deps
// ---------------------------------------------------------------------------

/// One canned response for a fix round.
#[derive(Clone)]
struct RoundScript {
    done: bool,
    changed: bool,
    issues: Vec<ReviewIssue>,
    /// When Some, fix() returns Err(Rejected(reason)) instead of Ok.
    reject: Option<String>,
}

/// Stub that plays a script of fix rounds and records every request so
/// tests can assert what the runner passed (scope cells, prior issues,
/// instruction notes).
#[derive(Default)]
struct StubDeps {
    initial_xml: String,
    rounds: std::sync::Mutex<Vec<RoundScript>>,
    /// Every (scope_cell_ids, prior_issue_count, instruction) received.
    fix_calls: std::sync::Mutex<Vec<(Vec<String>, usize, String)>>,
    /// Memory lines received on every fix call.
    fix_memories: std::sync::Mutex<Vec<Vec<String>>>,
}

impl StubDeps {
    fn new(initial_xml: &str, rounds: Vec<RoundScript>) -> Self {
        Self {
            initial_xml: initial_xml.to_string(),
            rounds: std::sync::Mutex::new(rounds),
            fix_calls: std::sync::Mutex::new(Vec::new()),
            fix_memories: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<(Vec<String>, usize, String)> {
        self.fix_calls.lock().unwrap().clone()
    }

    fn memories(&self) -> Vec<Vec<String>> {
        self.fix_memories.lock().unwrap().clone()
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
            finish_reason: None,
        })
    }

    async fn render(&self, _xml: &str) -> Result<Vec<u8>, RenderError> {
        Ok(PNG_1X1.to_vec())
    }

    async fn fix(&self, req: &FixRequest) -> Result<FixOutcome, FixError> {
        self.fix_calls.lock().unwrap().push((
            req.cell_ids.clone(),
            req.prior_issues.len(),
            req.instruction.clone(),
        ));
        self.fix_memories.lock().unwrap().push(req.memory.clone());
        let mut rounds = self.rounds.lock().unwrap();
        let script = rounds
            .first()
            .cloned()
            .unwrap_or(RoundScript {
                done: true,
                changed: false,
                issues: vec![],
                reject: None,
            });
        if rounds.len() > 1 {
            rounds.remove(0);
        }
        if let Some(reason) = script.reject {
            return Err(FixError::Rejected(reason));
        }
        Ok(FixOutcome {
            done: script.done,
            issues: script.issues,
            reasoning: Some("stub reasoning".into()),
            xml: req.xml.clone(),
            changed: script.changed,
            usage: Usage {
                input_tokens: 30,
                output_tokens: 10,
            },
            duration_ms: 7,
            finish_reason: Some("stop".into()),
        })
    }
}

fn config(prompt: &str, initial_xml: Option<&str>, max_iterations: u32) -> AgentLoop {
    AgentLoop {
        prompt: prompt.into(),
        initial_xml: initial_xml.map(str::to_string),
        max_iterations,
        patch_cell_ids: vec![],
        review_checks: vec![],
        no_think: false,
        memory: vec![],
        progress_cb: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn converges_immediately_when_fix_round_says_done() {
    let config = config("improve", Some(INITIAL_XML), 5);
    let deps = StubDeps::new(
        INITIAL_XML,
        vec![RoundScript {
            done: true,
            changed: true,
            issues: vec![],
            reject: None,
        }],
    );
    let outcome = run(config, &deps).await.expect("loop should converge");

    assert!(outcome.converged(), "expected Done, got {:?}", outcome.final_phase);
    assert_eq!(outcome.final_phase, LoopPhase::Done);
    assert_eq!(outcome.iterations, 1, "one fix round → 1 iteration");
    assert_eq!(outcome.last_verdict.as_deref(), Some("pass"));
    assert_eq!(outcome.last_issue_count, 0);
}

#[tokio::test]
async fn runs_full_initial_generate_when_initial_xml_is_none() {
    let config = config("draw something", None, 5);
    let deps = StubDeps::new(INITIAL_XML, vec![RoundScript {
        done: true,
        changed: false,
        issues: vec![],
        reject: None,
    }]);
    let outcome = run(config, &deps).await.expect("loop should converge");

    assert!(outcome.converged());
    assert_eq!(outcome.iterations, 1);
    let kinds: Vec<TrajectoryEventKind> =
        outcome.trajectory.iter().map(|e| e.kind.kind()).collect();
    assert!(
        kinds.contains(&TrajectoryEventKind::LlmCallStarted),
        "trajectory missing initial LlmCallStarted, got: {kinds:?}"
    );
}

#[tokio::test]
async fn done_false_round_carries_issues_and_scope_to_next_round() {
    let config = config("fix it", Some(INITIAL_XML), 5);
    let deps = StubDeps::new(
        INITIAL_XML,
        vec![
            RoundScript {
                done: false,
                changed: true,
                issues: vec![issue("overlap", &["2", "3"])],
                reject: None,
            },
            RoundScript {
                done: true,
                changed: false,
                issues: vec![],
                reject: None,
            },
        ],
    );
    let outcome = run(config, &deps).await.expect("loop should converge after 2 rounds");

    assert!(outcome.converged(), "got {:?}", outcome.final_phase);
    assert_eq!(outcome.iterations, 2);
    assert_eq!(outcome.last_verdict.as_deref(), Some("pass"));

    // Round 1: no prior issues (fresh loop) → no scope cells.
    // Round 2: prior issues from round 1 drive BOTH the scope cells and the
    //          prior_issues bridge the runner hands the deps impl.
    let calls = deps.calls();
    assert_eq!(calls.len(), 2, "runner must call fix exactly twice");
    assert!(calls[0].0.is_empty(), "round 1 has nothing to scope by");
    assert_eq!(calls[0].1, 0, "round 1 has no prior issues");
    assert_eq!(calls[1].0, vec!["2", "3"], "round 2 scope = issue cells");
    assert_eq!(calls[1].1, 1, "round 2 must carry round 1's issues");
}

#[tokio::test]
async fn explicit_patch_cell_ids_scope_every_round() {
    let config = AgentLoop {
        patch_cell_ids: vec!["7".into()],
        max_iterations: 1, // stop after one round; we only assert scope routing
        ..config("fix it", Some(INITIAL_XML), 5)
    };
    let deps = StubDeps::new(
        INITIAL_XML,
        vec![RoundScript {
            done: false,
            changed: true,
            issues: vec![issue("overlap", &["2"])],
            reject: None,
        }],
    );
    // max_iterations=1 → loop ends Failed after one round.
    let outcome = run(config, &deps).await.expect("loop terminates");
    assert!(!outcome.converged(), "single round must end Failed");
    let calls = deps.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].0,
        vec!["7"],
        "explicit user selection must override issue cells"
    );
}

#[tokio::test]
async fn rejected_output_is_recorded_and_retried() {
    let config = config("fix it", Some(INITIAL_XML), 5);
    let deps = StubDeps::new(
        INITIAL_XML,
        vec![
            RoundScript {
                done: false,
                changed: false,
                issues: vec![],
                reject: Some("your XML does not parse".into()),
            },
            RoundScript {
                done: true,
                changed: true,
                issues: vec![],
                reject: None,
            },
        ],
    );
    let outcome = run(config, &deps).await.expect("loop should converge after retry");

    assert!(outcome.converged(), "got {:?}", outcome.final_phase);
    assert_eq!(outcome.iterations, 2);

    // Rejection must be in the trajectory as an Error on the fix stage.
    let errors: Vec<(String, String)> = outcome
        .trajectory
        .iter()
        .filter_map(|e| match &e.kind {
            drawio_agent_trajectory::TrajectoryEvent::Error { stage, message } => {
                Some((stage.clone(), message.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, "fix");
    assert!(errors[0].1.contains("does not parse"));

    // The retry instruction must carry the rejection reason as feedback.
    let calls = deps.calls();
    assert_eq!(calls.len(), 2);
    assert!(
        calls[1].2.contains("系统反馈"),
        "round 2 instruction must carry the rejection note: {}",
        calls[1].2
    );
    assert!(
        calls[1].2.contains("does not parse"),
        "rejection reason must reach the model: {}",
        calls[1].2
    );
}

#[tokio::test]
async fn no_op_with_done_false_generates_feedback_note() {
    let config = config("fix it", Some(INITIAL_XML), 5);
    let deps = StubDeps::new(
        INITIAL_XML,
        vec![
            RoundScript {
                done: false,
                changed: false, // no-op: contradicting done=false
                issues: vec![],
                reject: None,
            },
            RoundScript {
                done: true,
                changed: false,
                issues: vec![],
                reject: None,
            },
        ],
    );
    let outcome = run(config, &deps).await.expect("loop should converge");

    assert!(outcome.converged(), "got {:?}", outcome.final_phase);
    let calls = deps.calls();
    assert_eq!(calls.len(), 2);
    assert!(
        calls[1].2.contains("done=true"),
        "round 2 must remind the model of the done contract: {}",
        calls[1].2
    );
}

#[tokio::test]
async fn gives_up_after_max_iterations() {
    let config = config("fix it", Some(INITIAL_XML), 2);
    let deps = StubDeps::new(
        INITIAL_XML,
        vec![
            RoundScript {
                done: false,
                changed: true,
                issues: vec![issue("overlap", &["2"])],
                reject: None,
            },
            RoundScript {
                done: false,
                changed: true,
                issues: vec![issue("overlap", &["2"])],
                reject: None,
            },
            RoundScript {
                done: true,
                changed: false,
                issues: vec![],
                reject: None,
            },
        ],
    );
    let outcome = run(config, &deps).await.expect("loop terminates");

    assert!(!outcome.converged(), "expected Failed, got {:?}", outcome.final_phase);
    assert_eq!(outcome.final_phase, LoopPhase::Failed);
    assert_eq!(outcome.iterations, 2, "should stop at max_iterations");
    assert_eq!(outcome.last_verdict.as_deref(), Some("issues"));
    assert_eq!(deps.calls().len(), 2, "third scripted round must never run");
}

#[tokio::test]
async fn trajectory_records_phase_transitions_in_order() {
    let config = config("improve", Some(INITIAL_XML), 5);
    let deps = StubDeps::new(INITIAL_XML, vec![RoundScript {
        done: true,
        changed: true,
        issues: vec![],
        reject: None,
    }]);
    let outcome = run(config, &deps).await.unwrap();

    let kinds: Vec<TrajectoryEventKind> =
        outcome.trajectory.iter().map(|e| e.kind.kind()).collect();
    // One converged round: [RenderStarted, RenderCompleted, LlmCallStarted
    // (fix), LlmCallCompleted (fix)]
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
        async fn fix(&self, _req: &FixRequest) -> Result<FixOutcome, FixError> {
            unreachable!()
        }
    }

    let config = config("draw", None, 5);
    let result = run(config, &FailingDeps).await;
    assert!(result.is_err(), "expected error when generate fails");
}

#[tokio::test]
async fn returns_error_on_deps_failure_during_render() {
    struct FailingRenderDeps;
    #[async_trait::async_trait]
    impl AgentDeps for FailingRenderDeps {
        async fn generate(
            &self,
            _req: GenerateRequest,
        ) -> Result<LlmResponse<String>, String> {
            unreachable!()
        }
        async fn render(&self, _xml: &str) -> Result<Vec<u8>, RenderError> {
            Err(RenderError::Browser("no browser".into()))
        }
        async fn fix(&self, _req: &FixRequest) -> Result<FixOutcome, FixError> {
            unreachable!()
        }
    }

    let config = config("improve", Some(INITIAL_XML), 5);
    let result = run(config, &FailingRenderDeps).await;
    assert!(matches!(result, Err(drawio_agent_agent::LoopError::Render { .. })));
}

#[tokio::test]
async fn returns_error_on_llm_failure_during_fix() {
    struct FailingFixDeps;
    #[async_trait::async_trait]
    impl AgentDeps for FailingFixDeps {
        async fn generate(
            &self,
            _req: GenerateRequest,
        ) -> Result<LlmResponse<String>, String> {
            unreachable!()
        }
        async fn render(&self, _xml: &str) -> Result<Vec<u8>, RenderError> {
            Ok(PNG_1X1.to_vec())
        }
        async fn fix(&self, _req: &FixRequest) -> Result<FixOutcome, FixError> {
            Err(FixError::Llm("api down".into()))
        }
    }

    let config = config("improve", Some(INITIAL_XML), 5);
    let result = run(config, &FailingFixDeps).await;
    assert!(matches!(result, Err(drawio_agent_agent::LoopError::Llm { .. })));
}

#[tokio::test]
async fn streams_progress_events_via_callback_in_order() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_cb = seen.clone();
    let config = AgentLoop {
        progress_cb: Some(Arc::new(
            move |e: drawio_agent_trajectory::TrajectoryEvent| {
                seen_cb.lock().unwrap().push(e.kind());
            },
        )),
        ..config("improve", Some(INITIAL_XML), 5)
    };
    let deps = StubDeps::new(INITIAL_XML, vec![RoundScript {
        done: true,
        changed: true,
        issues: vec![],
        reject: None,
    }]);
    let outcome = run(config, &deps).await.expect("loop should converge");

    let cb_kinds = seen.lock().unwrap().clone();
    let traj_kinds: Vec<TrajectoryEventKind> =
        outcome.trajectory.iter().map(|e| e.kind.kind()).collect();
    assert!(!cb_kinds.is_empty(), "callback should have seen events");
    assert_eq!(
        cb_kinds, traj_kinds,
        "callback order must match recorded trajectory order"
    );
}

#[tokio::test]
async fn session_memory_is_passed_to_every_fix_round() {
    // R2: AgentLoop.memory (earlier-turn summaries assembled by the server)
    // must reach the deps fix request unchanged on every round.
    let config = AgentLoop {
        memory: vec![
            "User asked earlier: draw a payment flow".to_string(),
            "Earlier done: generated the initial diagram (v3)".to_string(),
        ],
        ..config("fix it", Some(INITIAL_XML), 5)
    };
    let deps = StubDeps::new(
        INITIAL_XML,
        vec![RoundScript {
            done: false,
            changed: true,
            issues: vec![issue("overlap", &["2"])],
            reject: None,
        }],
    );
    let _outcome = run(config, &deps).await.expect("loop terminates");
    let memories = deps.memories();
    assert!(!memories.is_empty());
    assert!(
        memories.iter().all(|m| m == &vec![
            "User asked earlier: draw a payment flow".to_string(),
            "Earlier done: generated the initial diagram (v3)".to_string(),
        ]),
        "memory must be forwarded verbatim on every round: {memories:?}"
    );
}
