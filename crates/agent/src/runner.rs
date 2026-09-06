//! Core Agent Loop state machine (v2 single-context loop).
//!
//! Per round: Render the current XML → ONE multimodal fix call (the LLM
//! sees the latest render and self-reviews + edits in a single call,
//! returning a `{done, xml, issues}` envelope) → validate → apply → repeat
//! until `done=true`, `max_iterations` is hit, or a dependency fails.
//!
//! The separate Review pass of v1 is gone: convergence is gated on the
//! model's own `done` flag plus deps-side validation (parse / no-op /
//! scope merge). Issues the model reports with `done=false` are carried
//! into the next round's request as the stateless bridge between rounds.

use drawio_agent_llm_client::ReviewIssue;
use drawio_agent_trajectory::{TrajectoryEvent, TrajectoryStore};
use tracing::{info, warn};

use crate::deps::FixError;
use crate::deps::FixRequest;
use crate::phase::{LoopPhase, LoopState};
use crate::{AgentDeps, AgentLoop, AgentOutcome, LoopError, ProgressCb};

/// Record a trajectory event into the loop's local store AND forward it
/// to the configured live-progress callback (if any). The callback runs
/// synchronously in record order, so subscribers see events in exactly
/// the order the loop recorded them — even while the run is still going.
async fn record_progress(store: &TrajectoryStore, cb: &Option<ProgressCb>, event: TrajectoryEvent) {
    store.record("agent", event.clone()).await;
    if let Some(cb) = cb {
        cb(event);
    }
}

/// Build the user-facing instruction for a fix round: the caller's ask
/// (kept stable across rounds so the model never loses the objective) plus
/// any auto-generated feedback notes from the previous round (rejected
/// output, no-op-without-done, …).
fn build_instruction(base: &str, notes: &[String]) -> String {
    if notes.is_empty() {
        base.to_string()
    } else {
        let mut out = base.to_string();
        out.push_str("\n\n系统反馈（上一轮）:\n- ");
        out.push_str(&notes.join("\n- "));
        out
    }
}

/// Run the Agent Loop against the given dependency implementations until a
/// fix round reports `done`, `max_iterations` is hit, or a dependency call
/// fails.
///
/// Records a [`TrajectoryEvent`] for every state transition and embeds
/// them all in the returned [`AgentOutcome`].
pub async fn run<D: AgentDeps + ?Sized>(
    config: AgentLoop,
    deps: &D,
) -> Result<AgentOutcome, LoopError> {
    let max = config.max_iterations.max(1);
    let mut state = LoopState {
        phase: LoopPhase::Pending,
        iteration: 0,
        current_xml: None,
        last_verdict: None,
        last_issue_count: 0,
        last_reasoning: None,
    };
    // Trajectory events are recorded through the store so each one gets a
    // stable id/seq/timestamp; the final list is embedded in the outcome.
    let store = TrajectoryStore::new();

    // Step 0: baseline XML — either caller-provided or generate once. The
    // initial Generate phase only runs (and only records trajectory
    // events) when the caller did not supply a starting XML.
    state.phase = LoopPhase::Generate;
    state.current_xml = match config.initial_xml.clone() {
        Some(xml) => Some(xml),
        None => {
            record_progress(
                &store,
                &config.progress_cb,
                make_event(state.phase, config.prompt.chars().count(), false),
            )
            .await;
            match deps
                .generate(drawio_agent_llm_client::GenerateRequest {
                    user_prompt: config.prompt.clone(),
                    current_xml: None,
                    scope: None,
                    feedback: None,
                    json_mode: false,
                    memory: config.memory.clone(),
                })
                .await
            {
                Ok(r) => {
                    record_progress(
                        &store,
                        &config.progress_cb,
                        complete_event(
                            state.phase,
                            r.usage.input_tokens,
                            r.usage.output_tokens,
                            r.duration_ms,
                            r.finish_reason.clone(),
                        ),
                    )
                    .await;
                    Some(r.content)
                }
                Err(message) => {
                    let lp = state.phase;
                    record_progress(
                        &store,
                        &config.progress_cb,
                        error_event(state.phase, &message),
                    )
                    .await;
                    return Err(LoopError::Llm { phase: lp, message });
                }
            }
        }
    };

    // Fix rounds: Render → single multimodal fix call → apply/validate.
    // Self-reported issues from a `done=false` round are carried into the
    // next round's scope + prompt.
    let mut pending_issues: Vec<ReviewIssue> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    while state.iteration < max {
        state.iteration += 1;
        info!(iteration = state.iteration, "agent loop round start");

        // Render the current state (the model's visual input).
        state.phase = LoopPhase::Render;
        let xml = match state.current_xml.as_deref() {
            Some(x) => x,
            None => {
                return Err(LoopError::EmptyResponse { phase: LoopPhase::Generate });
            }
        };
        record_progress(
            &store,
            &config.progress_cb,
            make_event(state.phase, xml.len(), false),
        )
        .await;
        let render_started = std::time::Instant::now();
        let _png = match deps.render(xml).await {
            Ok(p) => {
                record_progress(
                    &store,
                    &config.progress_cb,
                    complete_event_bytes(
                        state.phase,
                        p.len(),
                        render_started.elapsed().as_millis() as u64,
                    ),
                )
                .await;
                p
            }
            Err(e) => {
                let lp = state.phase;
                let message = e.to_string();
                record_progress(
                    &store,
                    &config.progress_cb,
                    error_event(state.phase, &message),
                )
                .await;
                return Err(LoopError::Render {
                    phase: lp,
                    message,
                });
            }
        };

        // Single multimodal fix call (self-review + edit in one).
        state.phase = LoopPhase::Fix;
        let scope_cells: Vec<String> = if config.patch_cell_ids.is_empty() {
            pending_issues
                .iter()
                .flat_map(|i| i.cell_ids.clone())
                .collect()
        } else {
            config.patch_cell_ids.clone()
        };
        let instruction = build_instruction(&config.prompt, &notes);
        record_progress(
            &store,
            &config.progress_cb,
            make_event(state.phase, instruction.chars().count(), true),
        )
        .await;
        let fix_req = FixRequest {
            xml: xml.to_string(),
            image_png: _png,
            instruction,
            cell_ids: scope_cells,
            prior_issues: pending_issues.clone(),
            checks: config.review_checks.clone(),
            memory: config.memory.clone(),
        };
        match deps.fix(&fix_req).await {
            Ok(out) => {
                record_progress(
                    &store,
                    &config.progress_cb,
                    complete_event(
                        state.phase,
                        out.usage.input_tokens,
                        out.usage.output_tokens,
                        out.duration_ms,
                        out.finish_reason.clone(),
                    ),
                )
                .await;
                state.last_verdict = Some(if out.done { "pass" } else { "issues" }.into());
                state.last_issue_count = out.issues.len() as u32;
                state.last_reasoning = out.reasoning.clone();
                if out.changed {
                    state.current_xml = Some(out.xml.clone());
                }
                if out.done {
                    state.phase = LoopPhase::Done;
                    info!(iteration = state.iteration, "agent loop converged");
                    return Ok(final_outcome(state, store.list("agent").await, true));
                }
                // done=false: carry the model's remaining items to the next
                // round. If it reported nothing, remind it of the contract.
                pending_issues = out.issues;
                notes.clear();
                if pending_issues.is_empty() {
                    notes.push(
                        "上一轮返回 done=false 但未列出待办 issues：若无剩余问题请输出 \
                         done=true；若仍有问题请逐条列出 kind/severity/cell_ids/description。"
                            .into(),
                    );
                } else if out.reasoning.as_deref().is_some_and(|r| !r.is_empty()) {
                    info!(iteration = state.iteration, reasoning = %out.reasoning.unwrap_or_default(), "fix round not done");
                }
            }
            Err(FixError::Rejected(reason)) => {
                // Unusable output: record it, tell the model why, retry.
                record_progress(
                    &store,
                    &config.progress_cb,
                    error_event(state.phase, &reason),
                )
                .await;
                warn!(iteration = state.iteration, %reason, "fix output rejected; will retry");
                notes.clear();
                notes.push(format!("你上一轮的输出被拒绝，原因：{reason}。请修正后重试。"));
            }
            Err(FixError::Llm(message)) => {
                let lp = state.phase;
                record_progress(
                    &store,
                    &config.progress_cb,
                    error_event(state.phase, &message),
                )
                .await;
                return Err(LoopError::Llm { phase: lp, message });
            }
        }
    }

    warn!(max, "agent loop giving up after max iterations");
    state.phase = LoopPhase::Failed;
    Ok(final_outcome(state, store.list("agent").await, false))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a trajectory event marking the START of a phase.
fn make_event(phase: LoopPhase, payload_size: usize, json_mode: bool) -> TrajectoryEvent {
    match phase {
        LoopPhase::Generate | LoopPhase::Fix => TrajectoryEvent::LlmCallStarted {
            prompt_chars: payload_size,
            json_mode,
        },
        LoopPhase::Render => TrajectoryEvent::RenderStarted { scale: 1.0 },
        LoopPhase::Done | LoopPhase::Failed | LoopPhase::Pending => {
            TrajectoryEvent::StateTransition {
                from: None,
                to: phase.label().to_string(),
            }
        }
    }
}

/// Build a trajectory event marking the COMPLETION of a phase.
fn complete_event(
    phase: LoopPhase,
    input_tokens: u64,
    output_tokens: u64,
    duration_ms: u64,
    finish_reason: Option<String>,
) -> TrajectoryEvent {
    match phase {
        LoopPhase::Generate | LoopPhase::Fix => TrajectoryEvent::LlmCallCompleted {
            input_tokens,
            output_tokens,
            duration_ms,
            finish_reason,
        },
        LoopPhase::Render => TrajectoryEvent::RenderCompleted {
            bytes: 0,
            duration_ms: 0,
        },
        LoopPhase::Done | LoopPhase::Failed | LoopPhase::Pending => {
            TrajectoryEvent::StateTransition {
                from: None,
                to: phase.label().to_string(),
            }
        }
    }
}

fn complete_event_bytes(phase: LoopPhase, bytes: usize, duration_ms: u64) -> TrajectoryEvent {
    match phase {
        LoopPhase::Render => TrajectoryEvent::RenderCompleted {
            bytes,
            duration_ms,
        },
        _ => complete_event(phase, 0, 0, duration_ms, None),
    }
}

fn error_event(phase: LoopPhase, message: &str) -> TrajectoryEvent {
    TrajectoryEvent::Error {
        stage: phase.label().to_string(),
        message: message.to_string(),
    }
}

fn final_outcome(
    state: LoopState,
    trajectory: Vec<drawio_agent_trajectory::Event>,
    converged: bool,
) -> AgentOutcome {
    let final_phase = if converged {
        LoopPhase::Done
    } else {
        state.phase
    };
    AgentOutcome {
        final_xml: state.current_xml.clone().unwrap_or_default(),
        iterations: state.iteration,
        converged: matches!(final_phase, LoopPhase::Done),
        final_phase,
        last_verdict: state.last_verdict.clone(),
        last_issue_count: state.last_issue_count,
        last_reasoning: state.last_reasoning.clone(),
        trajectory,
    }
}
