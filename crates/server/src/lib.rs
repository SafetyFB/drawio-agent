//! Draw.io Agent server: HTTP + WebSocket façade over llm-client +
//! xml-core + renderer.
//!
//! Phase 4 skeleton. All state is in-memory; persistence is out of scope.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

use std::sync::Arc;

use axum::Router;
use drawio_agent_llm_client::LlmProvider;
use drawio_agent_renderer::{MockDriver, RenderDriver};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;
use uuid::Uuid;

pub mod agent_deps;
pub mod routes;
pub mod run;
pub mod state;

pub use agent_deps::ServerAgentDeps;
pub use routes::router;
pub use run::{
    build_app_state, run_server, shutdown_signal, ConfigError, LlmProviderKind,
    RendererKind, ReqwestHttpTransport, ServerConfig, StubLlm,
};
pub use state::{
    EventBus, SessionData, SessionId, SessionMeta, SessionStore, VersionEntry, VersionMeta,
    WsEvent,
};

/// Errors surfaced by the server (per-route handlers translate to HTTP status).
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),
    #[error("invalid input: {0}")]
    BadRequest(String),
    #[error("llm error: {0}")]
    Llm(String),
    #[error("render error: {0}")]
    Render(String),
    #[error("internal: {0}")]
    Internal(String),
}

/// Application state shared across handlers.
#[derive(Clone)]
pub struct AppState {
    pub sessions: Arc<RwLock<SessionStore>>,
    pub llm: Arc<dyn LlmProvider>,
    pub renderer: Arc<dyn RenderDriver>,
    pub events: EventBus,
    pub trajectory: drawio_agent_trajectory::TrajectoryStore,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState").finish_non_exhaustive()
    }
}

impl AppState {
    /// Construct state with a `MockDriver` renderer — convenient for tests
    /// and for local dev where the headless Chromium path is unavailable.
    pub fn with_mock_renderer(llm: Arc<dyn LlmProvider>) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(SessionStore::new())),
            llm,
            renderer: Arc::new(MockDriver::new()),
            events: EventBus::new(),
            trajectory: drawio_agent_trajectory::TrajectoryStore::new(),
        }
    }
}

/// Build the application router with all routes wired up.
pub fn build_router(state: AppState) -> Router {
    router(state)
}

/// Request body for `POST /api/sessions`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    #[serde(default)]
    pub initial_xml: Option<String>,
}

/// Response body for `POST /api/sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: SessionId,
}

/// Response body for `GET /api/sessions/:id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfoResponse {
    pub id: SessionId,
    pub meta: SessionMeta,
    pub current_xml: Option<String>,
}

/// Response body for `GET /api/sessions/:id/versions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionsResponse {
    pub versions: Vec<VersionMeta>,
}

/// Request body for `POST /api/sessions/:id/generate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateRequest {
    pub prompt: String,
    #[serde(default)]
    pub json_mode: bool,
}

/// Response body for `POST /api/sessions/:id/generate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateResponse {
    pub xml: String,
    pub version_id: Uuid,
}

/// Request body for `POST /api/sessions/:id/patch`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchRequest {
    pub cell_ids: Vec<String>,
    pub instruction: String,
    #[serde(default)]
    pub json_mode: bool,
}

/// Response body for `POST /api/sessions/:id/patch`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchResponse {
    pub xml: String,
    pub version_id: Uuid,
}

/// Response body for `POST /api/sessions/:id/render`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderResponse {
    /// PNG bytes, base64-encoded for JSON transport.
    pub png_base64: String,
    pub bytes: usize,
}

/// Request body for `POST /api/sessions/:id/review`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRequest {
    /// Optional override XML; defaults to the session's current.
    #[serde(default)]
    pub xml: Option<String>,
    #[serde(default)]
    pub checks: Vec<String>,
}

/// Request body for `POST /api/sessions/:id/agent-loop`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLoopRequest {
    /// Prompt for the Agent Loop (used for patch instructions).
    pub prompt: String,
    /// Maximum render-review-patch iterations. Defaults to 5.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Cells to focus on during Patch (e.g. specific node IDs).
    #[serde(default)]
    pub patch_cell_ids: Vec<String>,
    /// Optional reviewer checks.
    #[serde(default)]
    pub review_checks: Vec<String>,
}

fn default_max_iterations() -> u32 {
    5
}
