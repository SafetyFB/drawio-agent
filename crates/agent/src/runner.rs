//! Core Agent Loop state machine.

use drawio_agent_llm_client::{
    GenerateRequest, LlmResponse, ReviewIssue, ReviewResponse,
};
use drawio_agent_trajectory::{TrajectoryEvent, TrajectoryStore};
use tracing::{info, warn};

use crate::deps::AgentDeps;
use crate::phase::{LoopPhase, LoopState};
use crate::{AgentLoop, AgentOutcome, LoopError, ProgressCb};

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

/// Run the Agent Loop against the given dependency implementations until
/// the reviewer reports verdict "pass", `max_iterations` is hit, or a
/// dependency call fails.
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
    };
    // Trajectory events are recorded through the store so each one gets a
    // stable id/seq/timestamp; the final list is embedded in the outcome.
    let store = TrajectoryStore::new();

    // Step 0: initial XML — either caller-provided or generate once. The
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
                .generate(GenerateRequest {
                    user_prompt: config.prompt.clone(),
                    current_xml: None,
                    scope: None,
                    feedback: None,
                    json_mode: false,
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

    // Iteration loop: Render → Review → (Patch if issues).
    loop {
        state.iteration += 1;
        info!(iteration = state.iteration, "agent loop iteration start");

        if state.iteration > max {
            warn!(max, "agent loop giving up after max iterations");
            state.phase = LoopPhase::Failed;
            return Ok(final_outcome(config, state, store.list("agent").await, false));
        }

        // Render.
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
        let _png = match deps.render(xml).await {
            Ok(p) => {
                record_progress(
                    &store,
                    &config.progress_cb,
                    complete_event_bytes(state.phase, p.len()),
                )
                .await;
                p
            }
            Err(e) => {
                let lp = state.phase;
                record_progress(
                    &store,
                    &config.progress_cb,
                    error_event(state.phase, &e.to_string()),
                )
                .await;
                return Err(LoopError::Render {
                    phase: lp,
                    message: e.to_string(),
                });
            }
        };

        // Review.
        state.phase = LoopPhase::Review;
        record_progress(
            &store,
            &config.progress_cb,
            make_event(state.phase, config.review_checks.len(), false),
        )
        .await;
        let review = match deps.review(xml, &_png).await {
            Ok(r) => r,
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
        };
        record_progress(
            &store,
            &config.progress_cb,
            complete_event(
                state.phase,
                review_estimate_input(&review),
                review_estimate_output(&review),
                0,
                None,
            ),
        )
        .await;
        state.last_verdict = Some(review.verdict.clone());
        state.last_issue_count = review.issues.len() as u32;

        if review.verdict.eq_ignore_ascii_case("pass") {
            state.phase = LoopPhase::Done;
            info!(iteration = state.iteration, "agent loop converged");
            return Ok(final_outcome(config, state, store.list("agent").await, true));
        }

        if state.iteration >= max {
            state.phase = LoopPhase::Failed;
            return Ok(final_outcome(config, state, store.list("agent").await, false));
        }

        // Patch: rebuild the LLM call with feedback scoped to issue cells.
        state.phase = LoopPhase::Patch;
        let cell_ids: Vec<String> = if config.patch_cell_ids.is_empty() {
            review.issues.iter().flat_map(|i| i.cell_ids.clone()).collect()
        } else {
            config.patch_cell_ids.clone()
        };
        let instructions = format!(
            "Fix the {} issue(s) flagged by the visual reviewer.",
            review.issues.len()
        );
        // The recorded `prompt_chars` is the LLM's user-message size, not
        // the cell-ids count. Use the instructions text length as a
        // reasonable proxy; cell_ids.len() is wrong because it can be 0
        // when the vision review returned issues without cell_ids (the
        // Phase 37 Q2 gap) — which made the trajectory show "0 chars"
        // even though the LLM was called with a real prompt.
        record_progress(
            &store,
            &config.progress_cb,
            make_event(state.phase, instructions.chars().count(), true),
        )
        .await;
        match deps.patch(xml, &cell_ids, &instructions).await {
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
                state.current_xml = Some(r.content);
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
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a trajectory event marking the START of a phase.
fn make_event(phase: LoopPhase, payload_size: usize, json_mode: bool) -> TrajectoryEvent {
    match phase {
        LoopPhase::Generate | LoopPhase::Patch => TrajectoryEvent::LlmCallStarted {
            prompt_chars: payload_size,
            json_mode,
        },
        LoopPhase::Render => TrajectoryEvent::RenderStarted { scale: 1.0 },
        LoopPhase::Review => TrajectoryEvent::LlmCallStarted {
            prompt_chars: payload_size,
            json_mode: false,
        },
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
        LoopPhase::Generate | LoopPhase::Patch | LoopPhase::Review => {
            TrajectoryEvent::LlmCallCompleted {
                input_tokens,
                output_tokens,
                duration_ms,
                finish_reason,
            }
        }
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

fn complete_event_bytes(phase: LoopPhase, bytes: usize) -> TrajectoryEvent {
    match phase {
        LoopPhase::Render => TrajectoryEvent::RenderCompleted {
            bytes,
            duration_ms: 0,
        },
        _ => complete_event(phase, 0, 0, 0, None),
    }
}

fn error_event(phase: LoopPhase, message: &str) -> TrajectoryEvent {
    TrajectoryEvent::Error {
        stage: phase.label().to_string(),
        message: message.to_string(),
    }
}

fn final_outcome(
    _config: AgentLoop,
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
        final_phase,
        last_verdict: state.last_verdict,
        last_issue_count: state.last_issue_count,
        trajectory,
    }
}

fn review_estimate_input(_r: &ReviewResponse) -> u64 {
    // The Agent Loop doesn't have direct access to VLM token counts via
    // the deps trait; we surface zero so callers don't get bogus totals.
    // The review LLM call's own trajectory events (Started/Completed) are
    // emitted by the LLM provider directly in production wiring.
    0
}

fn review_estimate_output(r: &ReviewResponse) -> u64 {
    let _ = r;
    0
}

#[doc(hidden)]
pub fn _suppress_unused(_r: &LlmResponse<String>, _i: &ReviewIssue) {}
