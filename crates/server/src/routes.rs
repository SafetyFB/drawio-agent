//! axum router + route handlers for the Draw.io Agent server.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use axum::extract::ws::{Message, WebSocketUpgrade};
use serde::Serialize;
use serde_json::json;
use tokio::sync::broadcast;

use crate::{
    state::WsEvent, AgentLoopRequest, AppState, CreateSessionRequest, CreateSessionResponse,
    GenerateRequest, GenerateResponse, PatchRequest, PatchResponse, RenderResponse,
    ReviewRequest, ServerAgentDeps, SessionInfoResponse, ServerError, VersionsResponse,
};

/// Record a TrajectoryEvent to the store AND emit it on the EventBus as
/// a `WsEvent::Trajectory`. Best-effort: failures here don't fail the
/// caller's request, they just leave one of the two sinks out of date.
async fn record_and_emit(
    state: &AppState,
    session_id: &crate::state::SessionId,
    event: drawio_agent_trajectory::TrajectoryEvent,
) {
    state
        .trajectory
        .record(&session_id.0, event.clone())
        .await;
    state
        .events
        .emit(
            session_id,
            WsEvent::Trajectory(Box::new(drawio_agent_trajectory::Event {
                id: uuid::Uuid::new_v4(),
                seq: 0, // overwritten by the store's own seq; ignored on read
                at: std::time::SystemTime::now(),
                session_id: session_id.0.clone(),
                kind: event,
            })),
        )
        .await;
}

/// Error body returned to clients.
#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

/// Convert a ServerError into an HTTP response with appropriate status code.
impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            ServerError::SessionNotFound(_) => {
                (StatusCode::NOT_FOUND, self.to_string())
            }
            ServerError::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            ServerError::Llm(_) => (StatusCode::BAD_GATEWAY, self.to_string()),
            ServerError::Render(_) => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            ServerError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };
        (status, Json(ErrorBody { error: msg })).into_response()
    }
}

/// Build the full axum router with all routes wired up.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/sessions", post(create_session))
        .route("/api/sessions/:id", get(get_session))
        .route("/api/sessions/:id/versions", get(list_versions))
        .route("/api/sessions/:id/generate", post(generate))
        .route("/api/sessions/:id/patch", post(patch))
        .route("/api/sessions/:id/render", post(render))
        .route("/api/sessions/:id/review", post(review))
        .route("/api/sessions/:id/agent-loop", post(run_agent_loop))
        .route("/api/sessions/:id/events", get(events))
        .route("/api/sessions/:id/trajectory", get(get_trajectory))
        .with_state(Arc::new(state))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<CreateSessionResponse>), ServerError> {
    let id = state.sessions.write().await.create().await;
    if let Some(xml) = req.initial_xml {
        let _ = state
            .sessions
            .write()
            .await
            .append_version(&id, "initial", None, &xml)
            .await;
    }
    Ok((StatusCode::CREATED, Json(CreateSessionResponse { session_id: id })))
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SessionInfoResponse>, ServerError> {
    let session_id = crate::state::SessionId(id);
    let store = state.sessions.read().await;
    let data = store
        .get(&session_id)
        .await
        .ok_or_else(|| ServerError::SessionNotFound(session_id.clone()))?;
    let current_xml = store.current_xml(&session_id).await;
    Ok(Json(SessionInfoResponse {
        id: session_id,
        meta: data.meta,
        current_xml,
    }))
}

async fn list_versions(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<VersionsResponse>, ServerError> {
    let session_id = crate::state::SessionId(id);
    let store = state.sessions.read().await;
    if !store.contains(&session_id).await {
        return Err(ServerError::SessionNotFound(session_id));
    }
    let versions = store.versions(&session_id).await;
    Ok(Json(VersionsResponse { versions }))
}

async fn generate(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<GenerateRequest>,
) -> Result<Json<GenerateResponse>, ServerError> {
    let session_id = crate::state::SessionId(id);
    {
        let store = state.sessions.read().await;
        if !store.contains(&session_id).await {
            return Err(ServerError::SessionNotFound(session_id.clone()));
        }
    }
    let llm_req = drawio_agent_llm_client::GenerateRequest {
        user_prompt: req.prompt.clone(),
        current_xml: None,
        scope: None,
        feedback: None,
        json_mode: req.json_mode,
    };

    // Trajectory: log LlmCallStarted before dispatch (record + emit).
    record_and_emit(
        &state,
        &session_id,
        drawio_agent_trajectory::TrajectoryEvent::LlmCallStarted {
            prompt_chars: req.prompt.chars().count(),
            json_mode: req.json_mode,
        },
    )
    .await;

    let resp = match state.llm.generate_xml(llm_req).await {
        Ok(r) => r,
        Err(e) => {
            state
                .trajectory
                .record(
                    &session_id.0,
                    drawio_agent_trajectory::TrajectoryEvent::Error {
                        stage: "generate".into(),
                        message: e.to_string(),
                    },
                )
                .await;
            state
                .events
                .emit(
                    &session_id,
                    WsEvent::Error {
                        session_id: session_id.clone(),
                        message: e.to_string(),
                    },
                )
                .await;
            return Err(ServerError::Llm(e.to_string()));
        }
    };

    // Trajectory: log LlmCallCompleted (record + emit).
    record_and_emit(
        &state,
        &session_id,
        drawio_agent_trajectory::TrajectoryEvent::LlmCallCompleted {
            input_tokens: resp.usage.input_tokens,
            output_tokens: resp.usage.output_tokens,
            duration_ms: resp.duration_ms,
            finish_reason: resp.finish_reason.clone(),
        },
    )
    .await;

    let summary = truncate_summary(&req.prompt, 80);
    let version_id = state
        .sessions
        .write()
        .await
        .append_version(&session_id, "generate", Some(summary), &resp.content)
        .await
        .ok_or_else(|| ServerError::Internal("session vanished mid-flight".into()))?;
    state
        .events
        .emit(
            &session_id,
            WsEvent::VersionCreated {
                session_id: session_id.clone(),
                version_id,
                kind: "generate".into(),
            },
        )
        .await;
    Ok(Json(GenerateResponse {
        xml: resp.content,
        version_id,
    }))
}

/// Insert event-emit calls into patch + review. These mirror the generate
/// pattern: emit `VersionCreated` on success, `Error` on LLM failure.

/// Truncate a user prompt to `max_chars` for use as a version summary.
fn truncate_summary(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{truncated}…")
    }
}

async fn patch(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<PatchRequest>,
) -> Result<Json<PatchResponse>, ServerError> {
    let session_id = crate::state::SessionId(id);

    // 1. Verify session exists.
    {
        let store = state.sessions.read().await;
        if !store.contains(&session_id).await {
            return Err(ServerError::SessionNotFound(session_id.clone()));
        }
    }

    // 2. Get current XML.
    let current_xml = state
        .sessions
        .read()
        .await
        .current_xml(&session_id)
        .await
        .ok_or_else(|| {
            ServerError::BadRequest(
                "session has no current XML — call /generate first".into(),
            )
        })?;

    // 3. Parse and grab the model mutably.
    let mut file = drawio_agent_xml_core::MxFile::parse(current_xml.as_bytes())
        .map_err(|e| ServerError::BadRequest(format!("parse current XML: {e}")))?;
    let model = file
        .diagrams
        .first_mut()
        .ok_or_else(|| ServerError::BadRequest("no diagram in current XML".into()))?
        .model
        .as_mut()
        .ok_or_else(|| ServerError::BadRequest("no model in current XML".into()))?;

    let cell_id_refs: Vec<&str> = req.cell_ids.iter().map(|s| s.as_str()).collect();
    // (The subgraph itself isn't sent to the LLM in this stub; we send the
    // full current XML so the LLM stub can respond with a coherent diagram.
    // A future iteration can serialize the subgraph more compactly.)
    let _subgraph = model.extract_subgraph(&cell_id_refs);

    // 4. Call LLM.
    let llm_req = drawio_agent_llm_client::GenerateRequest {
        user_prompt: format!("Patch: {}", req.instruction),
        current_xml: Some(current_xml.clone()),
        scope: Some(current_xml.clone()),
        feedback: None,
        json_mode: req.json_mode,
    };

    // Trajectory: log LlmCallStarted before dispatch (record + emit).
    record_and_emit(
        &state,
        &session_id,
        drawio_agent_trajectory::TrajectoryEvent::LlmCallStarted {
            prompt_chars: req.instruction.chars().count(),
            json_mode: req.json_mode,
        },
    )
    .await;

    let resp = match state.llm.generate_xml(llm_req).await {
        Ok(r) => r,
        Err(e) => {
            state
                .trajectory
                .record(
                    &session_id.0,
                    drawio_agent_trajectory::TrajectoryEvent::Error {
                        stage: "patch".into(),
                        message: e.to_string(),
                    },
                )
                .await;
            return Err(ServerError::Llm(e.to_string()));
        }
    };
    record_and_emit(
        &state,
        &session_id,
        drawio_agent_trajectory::TrajectoryEvent::LlmCallCompleted {
            input_tokens: resp.usage.input_tokens,
            output_tokens: resp.usage.output_tokens,
            duration_ms: resp.duration_ms,
            finish_reason: resp.finish_reason.clone(),
        },
    )
    .await;

    // 5. Parse LLM response and extract the same subgraph.
    let patched = drawio_agent_xml_core::MxFile::parse(resp.content.as_bytes())
        .map_err(|e| ServerError::Llm(format!("parse LLM response: {e}")))?;
    let patched_subgraph = patched
        .diagrams
        .first()
        .ok_or_else(|| ServerError::Llm("no diagram in LLM response".into()))?
        .model
        .as_ref()
        .ok_or_else(|| ServerError::Llm("no model in LLM response".into()))?
        .extract_subgraph(&cell_id_refs);

    // 6. Apply patched subgraph to the original model.
    model.apply_subgraph(&patched_subgraph);

    // 7. Serialize the updated file.
    let updated_xml = file
        .to_xml()
        .map_err(|e| ServerError::Internal(format!("serialize updated XML: {e}")))?;

    // 8. Store new version.
    let summary = truncate_summary(&req.instruction, 80);
    let version_id = state
        .sessions
        .write()
        .await
        .append_version(&session_id, "patch", Some(summary), &updated_xml)
        .await
        .ok_or_else(|| ServerError::Internal("session vanished mid-flight".into()))?;
    state
        .events
        .emit(
            &session_id,
            WsEvent::VersionCreated {
                session_id: session_id.clone(),
                version_id,
                kind: "patch".into(),
            },
        )
        .await;

    Ok(Json(PatchResponse {
        xml: updated_xml,
        version_id,
    }))
}

async fn render(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ServerError> {
    let session_id = crate::state::SessionId(id);

    // 1. Verify session exists (404 if not).
    {
        let store = state.sessions.read().await;
        if !store.contains(&session_id).await {
            return Err(ServerError::SessionNotFound(session_id.clone()));
        }
    }

    // 2. Get current XML (400 if session exists but has no XML yet).
    let xml = state
        .sessions
        .read()
        .await
        .current_xml(&session_id)
        .await
        .ok_or_else(|| {
            ServerError::BadRequest(
                "session has no current XML — call /generate first".into(),
            )
        })?;

    let opts = drawio_agent_renderer::RenderOptions::default();

    // Trajectory: log RenderStarted (record + emit).
    record_and_emit(
        &state,
        &session_id,
        drawio_agent_trajectory::TrajectoryEvent::RenderStarted { scale: opts.scale },
    )
    .await;
    let render_started = std::time::Instant::now();

    let png = match state.renderer.render(&xml, &opts).await {
        Ok(p) => p,
        Err(e) => {
            state
                .trajectory
                .record(
                    &session_id.0,
                    drawio_agent_trajectory::TrajectoryEvent::Error {
                        stage: "render".into(),
                        message: e.to_string(),
                    },
                )
                .await;
            return Err(ServerError::Render(e.to_string()));
        }
    };
    record_and_emit(
        &state,
        &session_id,
        drawio_agent_trajectory::TrajectoryEvent::RenderCompleted {
            bytes: png.len(),
            duration_ms: render_started.elapsed().as_millis() as u64,
        },
    )
    .await;

    let len = png.len();
    let body = RenderResponse {
        png_base64: base64::engine::general_purpose::STANDARD.encode(&png),
        bytes: len,
    };
    Ok(Json(body).into_response())
}

async fn review(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ReviewRequest>,
) -> Result<Response, ServerError> {
    let session_id = crate::state::SessionId(id);

    // 1. Verify session exists.
    {
        let store = state.sessions.read().await;
        if !store.contains(&session_id).await {
            return Err(ServerError::SessionNotFound(session_id.clone()));
        }
    }

    // 2. Determine XML: explicit override wins; else fall back to session's
    //    current XML (400 if neither is available).
    let xml = match req.xml {
        Some(x) => x,
        None => state
            .sessions
            .read()
            .await
            .current_xml(&session_id)
            .await
            .ok_or_else(|| {
                ServerError::BadRequest(
                    "no XML available — pass it in the request body or call /generate first"
                        .into(),
                )
            })?,
    };

    // 3. Render XML to PNG (503 on failure; emit Error event).
    let opts = drawio_agent_renderer::RenderOptions::default();
    let png = match state.renderer.render(&xml, &opts).await {
        Ok(p) => p,
        Err(e) => {
            state
                .events
                .emit(
                    &session_id,
                    WsEvent::Error {
                        session_id: session_id.clone(),
                        message: e.to_string(),
                    },
                )
                .await;
            state
                .trajectory
                .record(
                    &session_id.0,
                    drawio_agent_trajectory::TrajectoryEvent::Error {
                        stage: "render".into(),
                        message: e.to_string(),
                    },
                )
                .await;
            return Err(ServerError::Render(e.to_string()));
        }
    };

    // Trajectory: log the render (record + emit).
    record_and_emit(
        &state,
        &session_id,
        drawio_agent_trajectory::TrajectoryEvent::RenderCompleted {
            bytes: png.len(),
            duration_ms: 0, // review path is best-effort; not timed in this stub
        },
    )
    .await;

    // 4. Call LLM for visual review (502 on failure; emit Error event).
    // Trajectory record goes FIRST so we can borrow req.checks before
    // moving it into the LLM request.
    record_and_emit(
        &state,
        &session_id,
        drawio_agent_trajectory::TrajectoryEvent::LlmCallStarted {
            prompt_chars: req.checks.iter().map(|s| s.len()).sum(),
            json_mode: false,
        },
    )
    .await;
    let llm_req = drawio_agent_llm_client::ReviewRequest {
        image_png: png,
        xml: xml.clone(),
        checks: req.checks,
    };
    let resp = match state.llm.review_visual(llm_req).await {
        Ok(r) => r,
        Err(e) => {
            state
                .events
                .emit(
                    &session_id,
                    WsEvent::Error {
                        session_id: session_id.clone(),
                        message: e.to_string(),
                    },
                )
                .await;
            state
                .trajectory
                .record(
                    &session_id.0,
                    drawio_agent_trajectory::TrajectoryEvent::Error {
                        stage: "review".into(),
                        message: e.to_string(),
                    },
                )
                .await;
            return Err(ServerError::Llm(e.to_string()));
        }
    };
    record_and_emit(
        &state,
        &session_id,
        drawio_agent_trajectory::TrajectoryEvent::LlmCallCompleted {
            input_tokens: resp.usage.input_tokens,
            output_tokens: resp.usage.output_tokens,
            duration_ms: resp.duration_ms,
            finish_reason: resp.finish_reason.clone(),
        },
    )
    .await;

    // 5. Return the parsed ReviewResponse directly.
    Ok(Json(resp.content).into_response())
}

// ---------------------------------------------------------------------------
// POST /api/sessions/:id/agent-loop
// ---------------------------------------------------------------------------

/// Run the Agent Loop (Generate → Render → Review → Patch) against the
/// session's current XML. Streams trajectory events to WS subscribers,
/// then stores the final XML as a new `agent-loop` version.
async fn run_agent_loop(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AgentLoopRequest>,
) -> Result<Json<drawio_agent_agent::AgentOutcome>, ServerError> {
    let session_id = crate::state::SessionId(id);

    // 404 if the session doesn't exist.
    {
        let store = state.sessions.read().await;
        if !store.contains(&session_id).await {
            return Err(ServerError::SessionNotFound(session_id));
        }
    }

    // 400 if there's no XML to start from.
    let initial_xml = state
        .sessions
        .read()
        .await
        .current_xml(&session_id)
        .await
        .ok_or_else(|| {
            ServerError::BadRequest(
                "session has no current XML — run /generate first".into(),
            )
        })?;

    // Bridge the server's shared providers into the Agent Loop.
    let deps = ServerAgentDeps {
        llm: state.llm.clone(),
        renderer: state.renderer.clone(),
    };

    let config = drawio_agent_agent::AgentLoop {
        prompt: req.prompt.clone(),
        initial_xml: Some(initial_xml),
        max_iterations: req.max_iterations,
        patch_cell_ids: req.patch_cell_ids,
        review_checks: req.review_checks,
    };

    let outcome = drawio_agent_agent::run(config, &deps).await.map_err(|e| {
        use drawio_agent_agent::LoopError as Le;
        match e {
            Le::Llm { .. } => ServerError::Llm(e.to_string()),
            Le::Render { .. } => ServerError::Render(e.to_string()),
            Le::MaxIterations(_) | Le::EmptyResponse { .. } => {
                ServerError::Internal(e.to_string())
            }
        }
    })?;

    // Stream each trajectory event to WS subscribers.
    for traj_event in &outcome.trajectory {
        state
            .events
            .emit(
                &session_id,
                WsEvent::Trajectory(Box::new(traj_event.clone())),
            )
            .await;
    }

    // Store the final XML as a new version.
    let version_id = state
        .sessions
        .write()
        .await
        .append_version(
            &session_id,
            "agent-loop",
            Some(truncate_summary(&req.prompt, 80)),
            &outcome.final_xml,
        )
        .await
        .ok_or_else(|| ServerError::Internal("session vanished mid-flight".into()))?;
    state
        .events
        .emit(
            &session_id,
            WsEvent::VersionCreated {
                session_id: session_id.clone(),
                version_id,
                kind: "agent-loop".into(),
            },
        )
        .await;

    Ok(Json(outcome))
}

// ---------------------------------------------------------------------------
// WebSocket events
// ---------------------------------------------------------------------------

async fn events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let session_id = crate::state::SessionId(id);

    // Verify the session exists (404 if not).
    if !state.sessions.read().await.contains(&session_id).await {
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    }

    ws.on_upgrade(move |mut socket| async move {
        // Force-create the channel and subscribe unconditionally. A session
        // with no events yet must simply wait for the first emit — returning
        // early here would close the socket immediately on an empty session.
        let mut rx = state.events.get_or_create(&session_id).await.subscribe();
        // Forward events until the client disconnects or the channel closes.
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let json = match serde_json::to_string(&event) {
                        Ok(j) => j,
                        Err(_) => continue,
                    };
                    if socket.send(Message::Text(json)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

// ---------------------------------------------------------------------------
// GET /api/sessions/:id/trajectory
// ---------------------------------------------------------------------------

async fn get_trajectory(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<drawio_agent_trajectory::Event>>, ServerError> {
    let session_id = crate::state::SessionId(id);
    if !state.sessions.read().await.contains(&session_id).await {
        return Err(ServerError::SessionNotFound(session_id));
    }
    let events = state.trajectory.list(&session_id.0).await;
    Ok(Json(events))
}
