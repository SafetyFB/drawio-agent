//! axum router + route handlers for the Draw.io Agent server.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::Serialize;
use serde_json::json;

use crate::{
    AppState, CreateSessionRequest, CreateSessionResponse, GenerateRequest, GenerateResponse,
    PatchRequest, PatchResponse, RenderResponse, ReviewRequest, SessionInfoResponse, ServerError,
    VersionsResponse,
};

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
    let _ = req; // initial_xml wiring deferred — Phase 4 stub ignores it
    let id = state.sessions.write().await.create().await;
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
    let resp = state
        .llm
        .generate_xml(llm_req)
        .await
        .map_err(|e| ServerError::Llm(e.to_string()))?;
    let summary = truncate_summary(&req.prompt, 80);
    let version_id = state
        .sessions
        .write()
        .await
        .append_version(&session_id, "generate", Some(summary), &resp.content)
        .await
        .ok_or_else(|| ServerError::Internal("session vanished mid-flight".into()))?;
    Ok(Json(GenerateResponse {
        xml: resp.content,
        version_id,
    }))
}

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
    Json(_req): Json<PatchRequest>,
) -> Result<Json<PatchResponse>, ServerError> {
    let session_id = crate::state::SessionId(id);
    let store = state.sessions.read().await;
    if !store.contains(&session_id).await {
        return Err(ServerError::SessionNotFound(session_id.clone()));
    }
    drop(store);
    Err(ServerError::Internal(
        "patch endpoint not yet implemented".into(),
    ))
}

async fn render(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ServerError> {
    let session_id = crate::state::SessionId(id);
    let xml = state
        .sessions
        .read()
        .await
        .current_xml(&session_id)
        .await
        .ok_or_else(|| ServerError::SessionNotFound(session_id.clone()))?;
    let opts = drawio_agent_renderer::RenderOptions::default();
    let png = state
        .renderer
        .render(&xml, &opts)
        .await
        .map_err(|e| ServerError::Render(e.to_string()))?;
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
    Json(_req): Json<ReviewRequest>,
) -> Result<Response, ServerError> {
    let session_id = crate::state::SessionId(id);
    let store = state.sessions.read().await;
    if !store.contains(&session_id).await {
        return Err(ServerError::SessionNotFound(session_id.clone()));
    }
    drop(store);
    Err(ServerError::Internal(
        "review endpoint not yet implemented".into(),
    ))
}
