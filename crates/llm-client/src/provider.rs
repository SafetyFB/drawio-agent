//! OpenAI-compatible chat completions provider.
//!
//! Implements [`LlmProvider`] on top of an [`HttpTransport`], targeting the
//! `/chat/completions` endpoint shape used by OpenAI and compatible services.

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::prompt::{codegen_system_prompt, review_system_prompt};
use crate::transport::{HttpTransport, TransportError};
use crate::Usage;

/// Configuration for an OpenAI-compatible provider.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub request_timeout_ms: u64,
    pub max_retries: u32,
}

/// Input for a single XML generation call.
#[derive(Debug, Clone)]
pub struct GenerateRequest {
    pub user_prompt: String,
    pub current_xml: Option<String>,
    /// Serialized subgraph XML for patch mode.
    pub scope: Option<String>,
    /// Visual review issues to incorporate into the regenerated XML.
    pub feedback: Option<Vec<String>>,
}

/// A single issue found during visual review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewIssue {
    pub kind: String,
    pub severity: String,
    pub cell_ids: Vec<String>,
    pub description: String,
}

/// Structured result of a visual review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewResponse {
    /// `"pass"` or `"issues"`.
    pub verdict: String,
    pub issues: Vec<ReviewIssue>,
}

/// Input for a visual review call.
#[derive(Debug, Clone)]
pub struct ReviewRequest {
    pub image_png: Vec<u8>,
    pub xml: String,
    pub checks: Vec<String>,
}

/// A typed response from an LLM call.
#[derive(Debug, Clone)]
pub struct LlmResponse<T> {
    pub content: T,
    pub usage: Usage,
    /// The full response body, preserved for callers that need raw access.
    pub raw: Value,
    pub duration_ms: u64,
}

/// Errors surfaced by a provider call.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("provider: {0}")]
    Provider(String),
}

/// Abstraction over LLM providers.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Provider name, used for diagnostics and accounting.
    fn name(&self) -> &str;
    /// Generate Draw.io XML from a user prompt.
    async fn generate_xml(
        &self,
        req: GenerateRequest,
    ) -> Result<LlmResponse<String>, ProviderError>;
    /// Visually review a rendered diagram.
    async fn review_visual(
        &self,
        req: ReviewRequest,
    ) -> Result<LlmResponse<ReviewResponse>, ProviderError>;
}

/// An OpenAI-compatible provider speaking the `/chat/completions` protocol.
pub struct OpenAiCompatProvider {
    transport: Arc<dyn HttpTransport>,
    config: ProviderConfig,
}

impl fmt::Debug for OpenAiCompatProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatProvider {
    /// Create a provider backed by the given transport and configuration.
    pub fn new(transport: Arc<dyn HttpTransport>, config: ProviderConfig) -> Self {
        Self { transport, config }
    }

    /// POST a JSON body to `{base_url}/chat/completions`, returning the raw
    /// response body on success or a transport error on non-2xx status.
    async fn post(&self, body: &Value) -> Result<Value, ProviderError> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let auth = format!("Bearer {}", self.config.api_key);
        let headers: &[(&str, &str)] = &[
            ("Authorization", &auth),
            ("Content-Type", "application/json"),
        ];
        let response = self.transport.post_json(&url, headers, body).await?;
        if !(200..300).contains(&response.status) {
            return Err(ProviderError::Transport(TransportError::Status {
                status: response.status,
                body: response.body.to_string(),
            }));
        }
        Ok(response.body)
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        &self.config.model
    }

    async fn generate_xml(
        &self,
        req: GenerateRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        let start = Instant::now();
        let body = json!({
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": codegen_system_prompt()},
                {"role": "user", "content": req.user_prompt},
            ],
        });
        let raw = self.post(&body).await?;
        let content = parse_content(&raw)?;
        let usage = parse_usage(&raw);
        let duration_ms = start.elapsed().as_millis() as u64;
        Ok(LlmResponse {
            content,
            usage,
            raw,
            duration_ms,
        })
    }

    async fn review_visual(
        &self,
        req: ReviewRequest,
    ) -> Result<LlmResponse<ReviewResponse>, ProviderError> {
        let start = Instant::now();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&req.image_png);
        let image_url = format!("data:image/png;base64,{encoded}");
        let user_text = format!(
            "Review the following Draw.io XML:\n{}\n\nChecks to perform: {}",
            req.xml,
            req.checks.join(", ")
        );
        let body = json!({
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": review_system_prompt()},
                {"role": "user", "content": [
                    {"type": "text", "text": user_text},
                    {"type": "image_url", "image_url": {"url": image_url}},
                ]},
            ],
        });
        let raw = self.post(&body).await?;
        let content_str = parse_content(&raw)?;
        let content = serde_json::from_str(&content_str)
            .map_err(|e| ProviderError::Provider(format!("invalid review JSON: {e}")))?;
        let usage = parse_usage(&raw);
        let duration_ms = start.elapsed().as_millis() as u64;
        Ok(LlmResponse {
            content,
            usage,
            raw,
            duration_ms,
        })
    }
}

/// Extract `choices[0].message.content` as a string.
fn parse_content(body: &Value) -> Result<String, TransportError> {
    let content = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str);
    match content {
        Some(content) => Ok(content.to_string()),
        None => Err(TransportError::Invalid(
            "no assistant message content in choices".to_string(),
        )),
    }
}

/// Extract `usage.prompt_tokens` / `usage.completion_tokens`; missing values
/// default to zero.
fn parse_usage(body: &Value) -> Usage {
    let input_tokens = body
        .pointer("/usage/prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = body
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        input_tokens,
        output_tokens,
    }
}