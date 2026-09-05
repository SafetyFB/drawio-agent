//! Server runtime: config loading, app state assembly, HTTP serve loop
//! with graceful shutdown. Public surface for integration tests.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use drawio_agent_llm_client::{
    HttpTransport, LlmProvider, OpenAiCompatProvider, ProviderConfig, Usage,
};
use drawio_agent_renderer::{HeadlessChromiumDriver, MockDriver, RenderDriver};
use crate::{build_router, AppState, EventBus, SessionStore};
use drawio_agent_trajectory::TrajectoryStore;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid bind addr: {0}")]
    BindAddr(String),
    #[error("invalid llm_base_url: {0}")]
    BaseUrl(#[from] url::ParseError),
    #[error("static dir not found: {0}")]
    StaticDir(String),
    #[error("env var: {0}")]
    Env(#[from] std::env::VarError),
}

/// Static configuration loaded from environment at startup. Defaults are
/// chosen so `cargo run` against the repo Just Works with no env vars
/// (mock LLM, MockDriver renderer, embedded static dir).
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind_addr: SocketAddr,
    pub llm_provider: LlmProviderKind,
    pub renderer: RendererKind,
    pub static_dir: Option<PathBuf>,
}

/// Which renderer backend to use.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RendererKind {
    /// Canned PNG bytes; deterministic, no browser required.
    #[default]
    Mock,
    /// Real headless Chromium via CDP.
    Chromium,
}

impl std::str::FromStr for RendererKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "mock" => Ok(Self::Mock),
            "chromium" => Ok(Self::Chromium),
            other => Err(format!("unknown renderer kind: {other}")),
        }
    }
}

#[derive(Debug, Clone)]
pub enum LlmProviderKind {
    Mock,
    OpenAiCompat {
        base_url: String,
        api_key: String,
        model: String,
    },
}

impl ServerConfig {
    /// Load from environment. Defaults: 127.0.0.1:8080, Mock LLM,
    /// `<crate manifest>/static`.
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_addr = std::env::var("DRAWIO_AGENT_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
            .parse::<SocketAddr>()
            .map_err(|e| ConfigError::BindAddr(e.to_string()))?;

        let llm_provider = match std::env::var("DRAWIO_AGENT_LLM_PROVIDER").as_deref() {
            Ok("mock") | Err(_) => LlmProviderKind::Mock,
            Ok("openai_compat") | Ok("openai-compat") => LlmProviderKind::OpenAiCompat {
                base_url: std::env::var("DRAWIO_AGENT_LLM_BASE_URL")?,
                api_key: std::env::var("DRAWIO_AGENT_LLM_API_KEY")?,
                model: std::env::var("DRAWIO_AGENT_LLM_MODEL")?,
            },
            Ok(other) => {
                // Both spelling variants are accepted above; anything else is a
                // real mistake. Print to stderr as well as tracing: the binary
                // has no tracing subscriber, so a silent fallback to the mock
                // LLM previously produced zero-usage trajectories with no
                // explanation.
                eprintln!(
                    "WARNING: unknown DRAWIO_AGENT_LLM_PROVIDER={other} \
                     (expected 'mock' | 'openai_compat' | 'openai-compat'); \
                     falling back to the mock LLM"
                );
                tracing::warn!(provider = %other, "unknown DRAWIO_AGENT_LLM_PROVIDER, falling back to Mock");
                LlmProviderKind::Mock
            }
        };

        // Renderer: $DRAWIO_AGENT_RENDERER (mock|chromium), default mock.
        let renderer = match std::env::var("DRAWIO_AGENT_RENDERER") {
            Ok(s) => s.parse::<RendererKind>().unwrap_or_else(|e| {
                warn!(renderer = %s, "unknown DRAWIO_AGENT_RENDERER ({e}), falling back to Mock");
                RendererKind::Mock
            }),
            Err(_) => RendererKind::Mock,
        };

        // Static dir: $DRAWIO_AGENT_STATIC_DIR or <manifest>/static.
        let static_dir = match std::env::var("DRAWIO_AGENT_STATIC_DIR") {
            Ok(s) => Some(PathBuf::from(s)),
            Err(_) => {
                let manifest = std::env::var("CARGO_MANIFEST_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| PathBuf::from("."));
                Some(manifest.join("static"))
            }
        };
        if let Some(dir) = &static_dir {
            if !dir.exists() {
                return Err(ConfigError::StaticDir(dir.display().to_string()));
            }
        }

        Ok(Self {
            bind_addr,
            llm_provider,
            renderer,
            static_dir,
        })
    }
}

/// Build the [`AppState`] from a [`ServerConfig`]. The LLM provider is
/// the OpenAI-compat implementation when configured, otherwise a
/// no-op stub that returns `<mxfile/>` for any prompt (useful for
/// local smoke-testing without API keys).
///
/// The renderer is [`HeadlessChromiumDriver`] when configured; launch
/// failures fall back to [`MockDriver`] so the server still starts.
pub async fn build_app_state(
    config: &ServerConfig,
) -> Result<AppState, Box<dyn std::error::Error + Send + Sync>> {
    let llm: Arc<dyn LlmProvider> = match &config.llm_provider {
        LlmProviderKind::Mock => Arc::new(StubLlm),
        LlmProviderKind::OpenAiCompat {
            base_url,
            api_key,
            model,
        } => {
            let transport: Arc<dyn HttpTransport> = Arc::new(ReqwestHttpTransport);
            Arc::new(OpenAiCompatProvider::new(
                transport,
                ProviderConfig {
                    base_url: base_url.clone(),
                    api_key: api_key.clone(),
                    model: model.clone(),
                    request_timeout_ms: 60_000,
                    max_retries: 0,
                },
            ))
        }
    };
    let renderer: Arc<dyn RenderDriver> = match config.renderer {
        RendererKind::Mock => Arc::new(MockDriver::new()),
        RendererKind::Chromium => match HeadlessChromiumDriver::launch().await {
            Ok(d) => {
                info!(target: "renderer", "headless chromium launched");
                Arc::new(d) as Arc<dyn RenderDriver>
            }
            Err(e) => {
                warn!(target: "renderer", "chromium launch failed ({e}); falling back to mock");
                Arc::new(MockDriver::new())
            }
        },
    };
    Ok(AppState {
        sessions: Arc::new(tokio::sync::RwLock::new(SessionStore::new())),
        llm,
        renderer,
        events: EventBus::new(),
        trajectory: TrajectoryStore::new(),
    })
}

/// Run the server with a pre-bound [`tokio::net::TcpListener`] until the
/// `shutdown` future resolves. Public for tests.
pub async fn run_server<L>(
    listener: tokio::net::TcpListener,
    state: AppState,
    static_dir: PathBuf,
    shutdown: L,
) -> Result<(), std::io::Error>
where
    L: Future<Output = ()> + Send + 'static,
{
    let app = build_router(state).fallback_service(
        tower_http::services::ServeDir::new(static_dir).append_index_html_on_directories(true),
    );
    let local_addr = listener.local_addr().ok();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    if let Some(addr) = local_addr {
        info!(%addr, "server stopped");
    }
    Ok(())
}

/// Wait for SIGINT (Ctrl-C) or SIGTERM (Unix).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

// ---------------------------------------------------------------------------
// Stub LLM + reqwest transport (used when LlmProviderKind::Mock is selected
// or when we need a real HTTP transport for OpenAiCompatProvider).
// ---------------------------------------------------------------------------

/// No-op LLM stub: returns `<mxfile/>` for any generate call. Lets the
/// server run without API keys.
#[derive(Debug)]
pub struct StubLlm;

#[async_trait::async_trait]
impl LlmProvider for StubLlm {
    fn name(&self) -> &str {
        "stub"
    }
    async fn generate_xml(
        &self,
        _req: drawio_agent_llm_client::GenerateRequest,
    ) -> Result<drawio_agent_llm_client::LlmResponse<String>, drawio_agent_llm_client::ProviderError>
    {
        Ok(drawio_agent_llm_client::LlmResponse {
            content: "<mxfile/>".to_string(),
            usage: Usage::default(),
            raw: serde_json::Value::Null,
            duration_ms: 0,
            finish_reason: None,
        })
    }
    async fn generate_streaming(
        &self,
        _req: drawio_agent_llm_client::GenerateRequest,
    ) -> Result<drawio_agent_llm_client::LlmStream, drawio_agent_llm_client::ProviderError>
    {
        Err(drawio_agent_llm_client::ProviderError::Provider(
            "streaming not supported by stub".into(),
        ))
    }
    async fn review_visual(
        &self,
        _req: drawio_agent_llm_client::ReviewRequest,
    ) -> Result<
        drawio_agent_llm_client::LlmResponse<drawio_agent_llm_client::ReviewResponse>,
        drawio_agent_llm_client::ProviderError,
    > {
        Ok(drawio_agent_llm_client::LlmResponse {
            content: drawio_agent_llm_client::ReviewResponse {
                verdict: "pass".to_string(),
                issues: vec![],
            },
            usage: Usage::default(),
            raw: serde_json::Value::Null,
            duration_ms: 0,
            finish_reason: None,
        })
    }
}

/// Thin [`HttpTransport`] wrapper around reqwest for the OpenAI-compat
/// provider when `DRAWIO_AGENT_LLM_PROVIDER=openai_compat`.
#[derive(Debug)]
pub struct ReqwestHttpTransport;

#[async_trait::async_trait]
impl HttpTransport for ReqwestHttpTransport {
    async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &serde_json::Value,
    ) -> Result<drawio_agent_llm_client::HttpResponse, drawio_agent_llm_client::TransportError>
    {
        let mut req = reqwest::Client::new()
            .post(url)
            .json(body);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().await.map_err(|e| {
            drawio_agent_llm_client::TransportError::Network(e.to_string())
        })?;
        let status = resp.status().as_u16();
        let body: serde_json::Value = resp.json().await.map_err(|e| {
            drawio_agent_llm_client::TransportError::Network(e.to_string())
        })?;
        Ok(drawio_agent_llm_client::HttpResponse { status, body })
    }
}
