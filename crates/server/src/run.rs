//! Server runtime: config loading, app state assembly, HTTP serve loop
//! with graceful shutdown. Public surface for integration tests.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use drawio_agent_llm_client::{HttpTransport, LlmProvider, Usage};
use drawio_agent_renderer::{HeadlessChromiumDriver, RenderDriver};
use crate::{
    build_provider, build_router, config_file_path, effective_settings, mask_secret,
    AppState, EventBus, RuntimeLlm, SessionStore,
};
use drawio_agent_trajectory::TrajectoryStore;
use thiserror::Error;
use tracing::info;

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

/// Static configuration: bind address, static dir and the path of the
/// persistent settings file (if any). The LLM provider is resolved at
/// startup from the settings file → env vars → mock stub, and can be
/// changed at runtime through the settings UI (`PUT /api/config`).
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind_addr: SocketAddr,
    pub static_dir: Option<PathBuf>,
    pub config_path: Option<std::path::PathBuf>,
    /// Explicit LLM settings override (tests inject the mock here so they
    /// never depend on the ambient env). `None` = resolve from config file
    /// → env → mock.
    pub llm: Option<crate::llm_config::LlmSettings>,
}

impl ServerConfig {
    /// Load bind addr/static dir from environment. Defaults chosen so
    /// `cargo run` Just Works: 127.0.0.1:8080, `<manifest>/static`.
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_addr = std::env::var("DRAWIO_AGENT_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
            .parse::<SocketAddr>()
            .map_err(|e| ConfigError::BindAddr(e.to_string()))?;

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
            static_dir,
            config_path: config_file_path(),
            llm: None,
        })
    }
}

/// Build the [`AppState`] from a [`ServerConfig`].
///
/// - LLM provider: resolved from the persistent settings file (if any),
///   then env vars, then the mock stub; wrapped in a hot-swappable
///   [`RuntimeLlm`] so the settings UI can change it at runtime.
/// - Renderer: ALWAYS the bundled headless Chromium shell (mocks exist
///   only in tests). A launch failure is fatal — silent "blank PNG"
///   sessions are worse than a clear startup error.
pub async fn build_app_state(
    config: &ServerConfig,
) -> Result<AppState, Box<dyn std::error::Error + Send + Sync>> {
    build_app_state_with_renderer(config, None).await
}

/// [`build_app_state`] with an explicit renderer (tests inject the mock;
/// the running server always uses the bundled headless Chromium shell).
pub async fn build_app_state_with_renderer(
    config: &ServerConfig,
    renderer_override: Option<Arc<dyn RenderDriver>>,
) -> Result<AppState, Box<dyn std::error::Error + Send + Sync>> {
    let settings = match &config.llm {
        Some(s) => s.clone(),
        None => effective_settings(config.config_path.as_deref()),
    };
    let masked = mask_secret(&settings.api_key);
    info!(
        target: "llm",
        kind = %settings.kind,
        base_url = %settings.base_url,
        model = %settings.model,
        api_key = %masked,
        "llm provider configured (change it anytime via the settings UI)"
    );
    let provider = build_provider(&settings);
    let llm = Arc::new(RuntimeLlm::new(provider));

    let renderer: Arc<dyn RenderDriver> = match renderer_override {
        Some(r) => r,
        None => {
            info!(target: "renderer", "launching headless chromium (bundled chrome-headless-shell)");
            let driver = HeadlessChromiumDriver::launch().await?;
            info!(target: "renderer", "headless chromium launched");
            Arc::new(driver) as Arc<dyn RenderDriver>
        }
    };

    Ok(AppState {
        sessions: Arc::new(tokio::sync::RwLock::new(SessionStore::new())),
        llm,
        renderer,
        events: EventBus::new(),
        trajectory: TrajectoryStore::new(),
        llm_settings: std::sync::Arc::new(std::sync::RwLock::new(settings)),
        config_path: config.config_path.clone(),
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

/// No-op LLM stub for mock mode. For a fresh generate it returns a richer
/// fixed diagram (2 ellipses, 2 rectangles, 1 rhombus/decision, 1 cylinder,
/// 6 edges with "Yes"/"No" labels and a dashed error path) so mock sessions
/// exercise the full shape vocabulary in the UI. When patching (the request
/// carries `current_xml`) it echoes that XML back so the patch handler gets a
/// valid diagram to parse. (Previously it always returned `<mxfile/>`, which
/// has no `<diagram>` — every patch failed with "no diagram in LLM response"
/// after recording LlmCallCompleted, and the canvas never updated.)
#[derive(Debug)]
pub struct StubLlm;

const MOCK_DIAGRAM: &str = r#"<mxfile host="app.diagrams.net">
  <diagram id="mock" name="Page-1">
    <mxGraphModel dx="800" dy="600" grid="1" guides="1" tooltips="1" connect="1" arrows="1" fold="1" page="1" pageScale="1" pageWidth="850" pageHeight="1100" math="0" shadow="0">
      <root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="2" value="Start" style="ellipse;whiteSpace=wrap;html=1;fillColor=#d5e8d4;strokeColor=#82b366;" vertex="1" parent="1">
          <mxGeometry x="200" y="80" width="80" height="40" as="geometry"/>
        </mxCell>
        <mxCell id="3" value="Process Input" style="rounded=1;whiteSpace=wrap;html=1;fillColor=#dae8fc;strokeColor=#6c8ebf;" vertex="1" parent="1">
          <mxGeometry x="160" y="180" width="160" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="4" value="Valid?" style="rhombus;whiteSpace=wrap;html=1;fillColor=#fff2cc;strokeColor=#d6b656;" vertex="1" parent="1">
          <mxGeometry x="180" y="300" width="120" height="80" as="geometry"/>
        </mxCell>
        <mxCell id="5" value="Save to DB" style="shape=cylinder3;whiteSpace=wrap;boundedLbl=1;backgroundOutline=1;size=15;fillColor=#f8cecc;strokeColor=#b85450;" vertex="1" parent="1">
          <mxGeometry x="320" y="320" width="80" height="80" as="geometry"/>
        </mxCell>
        <mxCell id="6" value="Show Error" style="rounded=0;whiteSpace=wrap;html=1;fillColor=#f8cecc;strokeColor=#b85450;" vertex="1" parent="1">
          <mxGeometry x="40" y="320" width="100" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="7" value="End" style="ellipse;whiteSpace=wrap;html=1;fillColor=#d5e8d4;strokeColor=#82b366;" vertex="1" parent="1">
          <mxGeometry x="200" y="440" width="80" height="40" as="geometry"/>
        </mxCell>
        <mxCell id="8" style="endArrow=classic;html=1;rounded=0;" edge="1" parent="1" source="2" target="3">
          <mxGeometry relative="1" as="geometry"/>
        </mxCell>
        <mxCell id="9" style="endArrow=classic;html=1;rounded=0;" edge="1" parent="1" source="3" target="4">
          <mxGeometry relative="1" as="geometry"/>
        </mxCell>
        <mxCell id="10" value="Yes" style="endArrow=classic;html=1;rounded=0;" edge="1" parent="1" source="4" target="5">
          <mxGeometry relative="1" as="geometry"/>
        </mxCell>
        <mxCell id="11" value="No" style="endArrow=classic;html=1;rounded=0;" edge="1" parent="1" source="4" target="6">
          <mxGeometry relative="1" as="geometry"/>
        </mxCell>
        <mxCell id="12" style="endArrow=classic;html=1;rounded=0;" edge="1" parent="1" source="5" target="7">
          <mxGeometry relative="1" as="geometry"/>
        </mxCell>
        <mxCell id="13" style="endArrow=classic;html=1;rounded=0;dashed=1;" edge="1" parent="1" source="6" target="7">
          <mxGeometry relative="1" as="geometry"/>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

#[async_trait::async_trait]
impl LlmProvider for StubLlm {
    fn name(&self) -> &str {
        "stub"
    }
    async fn generate_xml(
        &self,
        req: drawio_agent_llm_client::GenerateRequest,
    ) -> Result<drawio_agent_llm_client::LlmResponse<String>, drawio_agent_llm_client::ProviderError>
    {
        let content = req
            .current_xml
            .unwrap_or_else(|| MOCK_DIAGRAM.to_string());
        Ok(drawio_agent_llm_client::LlmResponse {
            content,
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

    async fn fix_diagram(
        &self,
        req: drawio_agent_llm_client::FixRequest,
    ) -> Result<
        drawio_agent_llm_client::LlmResponse<String>,
        drawio_agent_llm_client::ProviderError,
    > {
        // Mock single-call fix: echo the current state back with done=true.
        // The loop then converges on the first round (no change needed).
        let xml = req
            .current_xml
            .or(req.scope_xml)
            .unwrap_or_else(|| MOCK_DIAGRAM.to_string());
        let content = serde_json::json!({
            "done": true,
            "xml": xml,
            "issues": [],
            "reasoning": "mock provider: nothing to fix",
        })
        .to_string();
        Ok(drawio_agent_llm_client::LlmResponse {
            content,
            usage: Usage::default(),
            raw: serde_json::Value::Null,
            duration_ms: 0,
            finish_reason: None,
        })
    }
}

/// Thin [`HttpTransport`] wrapper around reqwest for the OpenAI-compat
/// provider when `DRAWIO_AGENT_LLM_PROVIDER=openai_compat`.
///
/// The underlying client carries a TOTAL request timeout (connect + headers +
/// body). Without it a stalled gateway leaves the run spinning forever —
/// real-model runs have shown hangs of 10+ minutes with no response at all.
#[derive(Debug)]
pub struct ReqwestHttpTransport {
    client: reqwest::Client,
}

impl ReqwestHttpTransport {
    /// `request_timeout_ms` bounds the whole LLM call (the configured
    /// provider may legitimately need 60-120s: reasoning + long XML out).
    pub fn with_timeout(request_timeout_ms: u64) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_millis(request_timeout_ms))
            .build()
            .expect("reqwest client build");
        Self { client }
    }
}

impl Default for ReqwestHttpTransport {
    fn default() -> Self {
        Self::with_timeout(180_000)
    }
}

#[async_trait::async_trait]
impl HttpTransport for ReqwestHttpTransport {
    async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &serde_json::Value,
    ) -> Result<drawio_agent_llm_client::HttpResponse, drawio_agent_llm_client::TransportError>
    {
        let mut req = self.client.post(url).json(body);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().await.map_err(|e| {
            let note = if e.is_timeout() {
                " (timed out after the configured request budget — the provider or "
                    .to_string()
                    + "gateway may be stalled; check the model endpoint or retry)"
            } else {
                String::new()
            };
            drawio_agent_llm_client::TransportError::Network(format!("{e}{note}"))
        })?;
        let status = resp.status().as_u16();
        let body: serde_json::Value = resp.json().await.map_err(|e| {
            drawio_agent_llm_client::TransportError::Network(e.to_string())
        })?;
        Ok(drawio_agent_llm_client::HttpResponse { status, body })
    }
}
