//! Runtime LLM configuration: settings, persistence, provider building,
//! and a hot-swappable [`RuntimeLlm`] proxy so the UI can change the
//! provider without a restart.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use drawio_agent_llm_client::{
    FixRequest, GenerateRequest, LlmProvider, LlmResponse, LlmStream, OpenAiCompatProvider,
    ProviderConfig, ProviderError, ReviewRequest, ReviewResponse,
};

use crate::run::StubLlm;

/// Which LLM backend to use. The UI writes these through `PUT /api/config`;
/// env vars are only the default when no config file exists yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LlmKind {
    /// No provider configured yet (no config file, no env, nothing the
    /// user saved). Every LLM call fails fast with a clear message that
    /// points at the settings UI. Never a silent fallback.
    #[serde(rename = "unconfigured")]
    Unconfigured,
    /// Built-in stub (no network; returns canned diagrams). Only used by
    /// tests and explicit API/CLI opt-in — the UI does not offer it.
    Mock,
    /// Any OpenAI-compatible `/chat/completions` endpoint.
    #[serde(rename = "openai-compat")]
    OpenAiCompat,
}

impl std::fmt::Display for LlmKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unconfigured => "unconfigured",
            Self::Mock => "mock",
            Self::OpenAiCompat => "openai-compat",
        })
    }
}

/// The user-facing LLM settings (what the UI edits and what persists).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmSettings {
    pub kind: LlmKind,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub model: String,
}

impl Default for LlmSettings {
    fn default() -> Self {
        Self {
            kind: LlmKind::Unconfigured,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
        }
    }
}

impl LlmSettings {
    /// `api_key_masked` for display (GET /api/config never leaks the key).
    pub fn mask_key(&self) -> String {
        mask_secret(&self.api_key)
    }
}

pub fn mask_secret(secret: &str) -> String {
    if secret.is_empty() {
        return String::new();
    }
    if secret.len() <= 8 {
        return "***".to_string();
    }
    format!("{}…{}", &secret[..4], &secret[secret.len() - 4..])
}

/// Error message shown for every LLM call while no provider is configured.
pub const UNCONFIGURED_MSG: &str =
    "LLM 未配置：请点击右上角 ⚙ 打开设置，填写 Base URL / API Key / Model 并保存";

/// A provider that fails every call with a clear configuration hint.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnconfiguredLlm;

#[async_trait]
impl LlmProvider for UnconfiguredLlm {
    fn name(&self) -> &str {
        "unconfigured"
    }
    async fn generate_xml(
        &self,
        _req: GenerateRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        Err(ProviderError::Provider(UNCONFIGURED_MSG.into()))
    }
    async fn generate_streaming(
        &self,
        _req: GenerateRequest,
    ) -> Result<LlmStream, ProviderError> {
        Err(ProviderError::Provider(UNCONFIGURED_MSG.into()))
    }
    async fn review_visual(
        &self,
        _req: ReviewRequest,
    ) -> Result<LlmResponse<ReviewResponse>, ProviderError> {
        Err(ProviderError::Provider(UNCONFIGURED_MSG.into()))
    }
    async fn fix_diagram(
        &self,
        _req: FixRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        Err(ProviderError::Provider(UNCONFIGURED_MSG.into()))
    }
}

/// Build a live provider for the given settings (used at startup, on PUT,
/// and by the connection test).
pub fn build_provider(settings: &LlmSettings) -> Arc<dyn LlmProvider> {
    match settings.kind {
        LlmKind::Unconfigured => Arc::new(UnconfiguredLlm),
        LlmKind::Mock => Arc::new(StubLlm),
        LlmKind::OpenAiCompat => build_openai_provider(settings, None)
    }
}

/// Shared openai-compat construction (startup, UI hot-swap, connection
/// test). `timeout_ms`: request budget for a whole LLM call; GLM reasoning
/// plus a full diagram can legitimately take 60-120s, but a stalled gateway
/// must fail loudly instead of hanging the run forever.
pub fn build_openai_provider(
    settings: &LlmSettings,
    timeout_ms: Option<u64>,
) -> Arc<dyn LlmProvider> {
    let request_timeout_ms = timeout_ms.unwrap_or(150_000);
    let transport: Arc<dyn drawio_agent_llm_client::HttpTransport> =
        Arc::new(crate::run::ReqwestHttpTransport::with_timeout(request_timeout_ms));
    Arc::new(OpenAiCompatProvider::new(
        transport,
        ProviderConfig {
            base_url: settings.base_url.trim_end_matches('/').to_string(),
            api_key: settings.api_key.clone(),
            model: settings.model.clone(),
            request_timeout_ms,
            max_retries: 0,
        },
    ))
}

/// Config file shape on disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub llm: LlmSettings,
}

/// Resolve the config file path: `DRAWIO_AGENT_CONFIG_FILE`, else
/// `~/.drawio-agent/config.json`.
pub fn config_file_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DRAWIO_AGENT_CONFIG_FILE") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".drawio-agent").join("config.json"))
}

pub fn load_config_file(path: &Path) -> Option<LlmSettings> {
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed: ConfigFile = serde_json::from_str(&raw).ok()?;
    Some(parsed.llm)
}

pub fn save_config_file(path: &Path, llm: &LlmSettings) -> Result<(), String> {
    let dir = path.parent().ok_or_else(|| "config path has no parent".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create config dir: {e}"))?;
    let json = serde_json::to_string_pretty(&ConfigFile { llm: llm.clone() })
        .map_err(|e| format!("serialize config: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("write config file: {e}"))
}

/// Resolve the effective settings: config file wins (the UI is the source
/// of truth once it saved), then env vars. Nothing found → [`LlmKind::Unconfigured`]:
/// the server runs, but every LLM call fails fast with a settings hint —
/// never a silent mock fallback.
pub fn effective_settings(file: Option<&Path>) -> LlmSettings {
    if let Some(p) = file {
        if let Some(s) = load_config_file(p) {
            if matches!(s.kind, LlmKind::OpenAiCompat)
                && (s.base_url.is_empty() || s.model.is_empty())
            {
                // A saved openai-compat config missing fields is broken;
                // fall through to env rather than silently shipping an
                // unusable provider.
                tracing::warn!("config file has an incomplete openai-compat block; ignoring it");
            } else {
                return s;
            }
        }
    }
    env_settings().unwrap_or_else(|| LlmSettings {
        kind: LlmKind::Unconfigured,
        ..Default::default()
    })
}

pub fn env_settings() -> Option<LlmSettings> {
    match std::env::var("DRAWIO_AGENT_LLM_PROVIDER").as_deref() {
        Ok("mock") => Some(LlmSettings {
            kind: LlmKind::Mock,
            ..Default::default()
        }),
        Ok("openai_compat") | Ok("openai-compat") => {
            let base_url = std::env::var("DRAWIO_AGENT_LLM_BASE_URL").ok()?;
            let api_key = std::env::var("DRAWIO_AGENT_LLM_API_KEY").ok()?;
            let model = std::env::var("DRAWIO_AGENT_LLM_MODEL").ok()?;
            Some(LlmSettings {
                kind: LlmKind::OpenAiCompat,
                base_url,
                api_key,
                model,
            })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Hot-swappable provider proxy
// ---------------------------------------------------------------------------

/// A [`LlmProvider`] whose inner provider can be swapped at runtime
/// (settings UI → `PUT /api/config`). All trait methods forward to the
/// current inner provider; the swap is atomic for callers.
pub struct RuntimeLlm {
    inner: RwLock<Arc<dyn LlmProvider>>,
}

impl std::fmt::Debug for RuntimeLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeLlm").finish_non_exhaustive()
    }
}

impl RuntimeLlm {
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            inner: RwLock::new(provider),
        }
    }

    /// Replace the active provider. All in-flight calls finish against the
    /// old one; new calls pick up the new one.
    pub async fn swap(&self, provider: Arc<dyn LlmProvider>) {
        *self.inner.write().await = provider;
    }

    pub async fn snapshot(&self) -> Arc<dyn LlmProvider> {
        self.inner.read().await.clone()
    }
}

#[async_trait]
impl LlmProvider for RuntimeLlm {
    fn name(&self) -> &str {
        // Name is a diagnostic nicety; the real provider name is dynamic,
        // so report the proxy role. Callers needing the actual model read
        // the settings via GET /api/config.
        "runtime"
    }

    async fn generate_xml(
        &self,
        req: GenerateRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        self.inner.read().await.generate_xml(req).await
    }

    async fn generate_streaming(
        &self,
        req: GenerateRequest,
    ) -> Result<LlmStream, ProviderError> {
        self.inner.read().await.generate_streaming(req).await
    }

    async fn review_visual(
        &self,
        req: ReviewRequest,
    ) -> Result<LlmResponse<ReviewResponse>, ProviderError> {
        self.inner.read().await.review_visual(req).await
    }

    async fn fix_diagram(
        &self,
        req: FixRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        self.inner.read().await.fix_diagram(req).await
    }
}
