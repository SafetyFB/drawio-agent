//! OpenAI-compatible chat completions provider.
//!
//! Implements [`LlmProvider`] on top of an [`HttpTransport`], targeting the
//! `/chat/completions` endpoint shape used by OpenAI and compatible services.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use base64::Engine;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::prompt::{
    codegen_system_prompt, codegen_user_prompt, fix_system_prompt, fix_user_prompt,
    patch_system_prompt, review_system_prompt,
};
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
#[derive(Debug, Clone, Default)]
pub struct GenerateRequest {
    pub user_prompt: String,
    pub current_xml: Option<String>,
    /// Serialized subgraph XML for patch mode.
    pub scope: Option<String>,
    /// Visual review issues to incorporate into the regenerated XML.
    pub feedback: Option<Vec<String>>,
    /// When true, request `response_format: {type: "json_object"}` and
    /// parse the assistant content as `{"xml": "...", "reasoning": "..."}`.
    pub json_mode: bool,
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

/// Input for the single-call multimodal fix step (v2 loop): the provider
/// builds a system+user message pair where the user message carries the
/// text parts built by [`crate::fix_user_prompt`] plus the rendered image.
///
/// `scope_xml` marks a Plan-B call: the model sees ONLY the subgraph and
/// must return a document containing just those cells (the server merges
/// them back). `current_xml` is the full applied state; when a scope is
/// given it is deliberately NOT sent (the model must not rewrite the full
/// diagram).
#[derive(Debug, Clone)]
pub struct FixRequest {
    /// User instruction for this round (base ask + loop feedback notes).
    pub instruction: String,
    /// Full current XML. `None` when `scope_xml` is `Some` (Plan-B).
    pub current_xml: Option<String>,
    /// Plan-B subgraph: the ONLY cells the model may modify.
    pub scope_xml: Option<String>,
    /// Self-reported issues from the previous round (stateless bridge
    /// between rounds until conversation memory lands in R2).
    pub issues: Vec<ReviewIssue>,
    /// Optional reviewer focus checks.
    pub checks: Vec<String>,
    /// Latest rendered PNG the model must visually review.
    pub image_png: Vec<u8>,
}

/// Parsed JSON envelope returned by the fix step:
/// `{"done", "xml", "issues", "reasoning"}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixEnvelope {
    /// `true` when the model fixed everything it can (or nothing needs
    /// fixing). Missing on the wire defaults to `true`.
    #[serde(default = "default_done")]
    pub done: bool,
    /// The resulting document: full mxfile, or scope-only when the request
    /// carried a `scope_xml`.
    pub xml: String,
    /// Items the model could not resolve / wants re-verified visually.
    #[serde(default)]
    pub issues: Vec<ReviewIssue>,
    #[serde(default)]
    pub reasoning: Option<String>,
}

fn default_done() -> bool {
    true
}

/// Parse the assistant's fix response into a [`FixEnvelope`]. Strict about
/// the fields the loop actually gates on (`xml`, `done`); missing `issues`
/// and `reasoning` are tolerated.
pub fn parse_fix_envelope(content: &str) -> Result<FixEnvelope, String> {
    let parsed: Value = serde_json::from_str(content).map_err(|e| {
        format!("fix response is not a JSON object ({e}); expected envelope \
                 {{\"done\": bool, \"xml\": \"<mxfile>…\"}}")
    })?;
    let xml = parsed
        .get("xml")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            "fix response missing string 'xml' field; expected envelope \
             {\"done\": bool, \"xml\": \"<mxfile>…\"}".to_string()
        })?;
    let done = parsed.get("done").and_then(Value::as_bool).unwrap_or(true);
    let issues: Vec<ReviewIssue> = parsed
        .get("issues")
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .map_err(|e| format!("fix response 'issues' is malformed: {e}"))?
        .unwrap_or_default();
    let reasoning = parsed
        .get("reasoning")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(FixEnvelope {
        done,
        xml,
        issues,
        reasoning,
    })
}

/// A typed response from an LLM call.
#[derive(Debug, Clone)]
pub struct LlmResponse<T> {
    pub content: T,
    pub usage: Usage,
    /// The full response body, preserved for callers that need raw access.
    pub raw: Value,
    pub duration_ms: u64,
    /// `finish_reason` from `choices[0].finish_reason` (e.g. `"stop"`).
    pub finish_reason: Option<String>,
}

/// A single chunk in a streaming response.
#[derive(Debug, Clone, Default)]
pub struct StreamChunk {
    /// Incremental text delta from the assistant.
    pub delta: String,
    /// Set on the final chunk (e.g. `"stop"` or `"length"`).
    pub finish_reason: Option<String>,
    /// Total token usage, present on the final chunk when the API reports it.
    pub usage: Option<Usage>,
}

/// A stream of [`StreamChunk`]s from a streaming generation call.
pub type LlmStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, ProviderError>> + Send>>;

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
    /// Stream incremental chunks of the generated content.
    async fn generate_streaming(
        &self,
        req: GenerateRequest,
    ) -> Result<LlmStream, ProviderError>;
    /// Visually review a rendered diagram.
    async fn review_visual(
        &self,
        req: ReviewRequest,
    ) -> Result<LlmResponse<ReviewResponse>, ProviderError>;
    /// Single-call multimodal self-review-and-fix (v2 loop). The model sees
    /// the latest render and edits the XML in one call; the assistant
    /// content is the raw JSON envelope (parse with
    /// [`parse_fix_envelope`]).
    ///
    /// Default: unsupported. Providers without this capability fail fast so
    /// callers can fall back to the split generate/review pipeline.
    async fn fix_diagram(
        &self,
        req: FixRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        let _ = req;
        Err(ProviderError::Provider(
            "fix_diagram: multimodal single-call fix not supported by this provider".into(),
        ))
    }
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
            tracing::error!(
                status = response.status,
                body = %response.body,
                "llm provider returned non-2xx status"
            );
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
        // A `scope` marks a patch call: the LLM sees only the selected
        // subgraph (plus the patch-specific system prompt) rather than the
        // full diagram, so it must edit in place instead of regenerating
        // coordinates from scratch.
        let (system_msg, user_msg) = if req.scope.is_some() {
            (
                patch_system_prompt(),
                codegen_user_prompt(
                    &req.user_prompt,
                    req.current_xml.as_deref(),
                    req.scope.as_deref(),
                    req.feedback.as_deref(),
                ),
            )
        } else {
            (codegen_system_prompt().to_string(), req.user_prompt.clone())
        };
        let mut body = json!({
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": system_msg},
                {"role": "user", "content": user_msg},
            ],
        });
        if req.json_mode {
            body["response_format"] = json!({"type": "json_object"});
        }
        let raw = self.post(&body).await?;
        // Measure provider-side wall-clock time so the trajectory reflects
        // the actual LLM call latency regardless of who records it.
        let duration_ms = start.elapsed().as_millis() as u64;

        // Parse content BEFORE usage: a missing `choices` is the more likely
        // (and more actionable) failure, so it must be reported first.
        let content = if req.json_mode {
            let content_str = parse_content(&raw)?;
            parse_json_codegen_content(&content_str)?
        } else {
            parse_content(&raw)?
        };
        let usage = parse_usage(&raw)?;
        let finish_reason = parse_finish_reason(&raw);

        Ok(LlmResponse {
            content,
            usage,
            raw,
            duration_ms,
            finish_reason,
        })
    }

    async fn generate_streaming(
        &self,
        req: GenerateRequest,
    ) -> Result<LlmStream, ProviderError> {
        let (system_msg, user_msg) = if req.scope.is_some() {
            (
                patch_system_prompt(),
                codegen_user_prompt(
                    &req.user_prompt,
                    req.current_xml.as_deref(),
                    req.scope.as_deref(),
                    req.feedback.as_deref(),
                ),
            )
        } else {
            (codegen_system_prompt().to_string(), req.user_prompt.clone())
        };
        let mut body = json!({
            "model": self.config.model,
            "stream": true,
            "messages": [
                {"role": "system", "content": system_msg},
                {"role": "user", "content": user_msg},
            ],
        });
        if req.json_mode {
            body["response_format"] = json!({"type": "json_object"});
        }

        let raw = self.post(&body).await?;
        let sse_text: String = raw
            .as_str()
            .ok_or_else(|| {
                ProviderError::Provider(
                    "streaming: expected response body as string".to_string(),
                )
            })?
            .to_string();

        Ok(Box::pin(parse_sse_stream(sse_text)))
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
        let usage = parse_usage(&raw)?;
        let finish_reason = parse_finish_reason(&raw);
        let duration_ms = start.elapsed().as_millis() as u64;
        Ok(LlmResponse {
            content,
            usage,
            raw,
            duration_ms,
            finish_reason,
        })
    }

    async fn fix_diagram(
        &self,
        req: FixRequest,
    ) -> Result<LlmResponse<String>, ProviderError> {
        let start = Instant::now();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&req.image_png);
        let image_url = format!("data:image/png;base64,{encoded}");
        let user_text = fix_user_prompt(
            &req.instruction,
            req.current_xml.as_deref(),
            req.scope_xml.as_deref(),
            &req.issues,
            &req.checks,
        );
        let mut body = json!({
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": fix_system_prompt()},
                {"role": "user", "content": [
                    {"type": "text", "text": user_text},
                    {"type": "image_url", "image_url": {"url": image_url}},
                ]},
            ],
        });
        // The envelope ({"done", "xml", "issues"}) is required for the loop
        // to gate convergence, so always request structured JSON output.
        body["response_format"] = json!({"type": "json_object"});
        let raw = self.post(&body).await?;
        let duration_ms = start.elapsed().as_millis() as u64;

        let content = parse_content(&raw)?;
        let usage = parse_usage(&raw)?;
        let finish_reason = parse_finish_reason(&raw);

        Ok(LlmResponse {
            content,
            usage,
            raw,
            duration_ms,
            finish_reason,
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
        None => Err(TransportError::Invalid(format!(
            "OpenAI response missing 'choices[0].message.content': {}",
            body_preview(body)
        ))),
    }
}

/// Extract `choices[0].finish_reason` as a string, if present.
fn parse_finish_reason(body: &Value) -> Option<String> {
    body.pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Parse the assistant content as the codegen JSON envelope
/// `{"xml": "...", "reasoning": "..."}`. Returns the `xml` field. The
/// `reasoning` field (if present) is preserved in the response's `raw`
/// payload by the caller.
fn parse_json_codegen_content(content_str: &str) -> Result<String, TransportError> {
    let parsed: Value = serde_json::from_str(content_str).map_err(|e| {
        TransportError::Invalid(format!("json_mode: response is not valid JSON: {e}"))
    })?;
    parsed
        .get("xml")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            TransportError::Invalid(
                "json_mode: response missing string 'xml' field".to_string(),
            )
        })
}

/// Parse an SSE-formatted response body into a stream of [`StreamChunk`]s.
///
/// Lines starting with `data: ` are JSON payloads. `data: [DONE]`
/// terminates the stream. Empty lines, SSE comments (`: ...`), and any
/// other lines are ignored.
///
/// Takes ownership of the SSE text so the returned stream is `'static`
/// and self-contained.
fn parse_sse_stream(
    sse_text: String,
) -> impl Stream<Item = Result<StreamChunk, ProviderError>> + Send {
    async_stream::try_stream! {
        for raw_line in sse_text.split('\n') {
            let line = raw_line.trim_end_matches('\r').trim();
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            let Some(payload) = line.strip_prefix("data: ") else {
                continue;
            };
            if payload == "[DONE]" {
                break;
            }
            let value: Value = serde_json::from_str(payload).map_err(|e| {
                ProviderError::Provider(format!("invalid SSE JSON payload: {e}"))
            })?;
            let delta = value
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let finish_reason = value
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                .map(str::to_string);
            let usage = value.get("usage").and_then(|u| {
                let input = u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
                let output = u.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
                if u.is_object() && (input > 0 || output > 0) {
                    Some(Usage {
                        input_tokens: input,
                        output_tokens: output,
                    })
                } else {
                    None
                }
            });

            yield StreamChunk {
                delta,
                finish_reason,
                usage,
            };
        }
    }
}

/// Extract `usage.prompt_tokens` / `usage.completion_tokens`. Errors when
/// the `usage` object is entirely absent so a silent zero-usage regression
/// surfaces in server logs instead of a plausible-but-wrong 0/0.
fn parse_usage(body: &Value) -> Result<Usage, TransportError> {
    let usage = body.get("usage").ok_or_else(|| {
        TransportError::Invalid(format!("OpenAI response missing 'usage': {}", body_preview(body)))
    })?;
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Ok(Usage {
        input_tokens,
        output_tokens,
    })
}

/// First 500 chars of a raw JSON body, for error messages and logs.
fn body_preview(value: &Value) -> String {
    value.to_string().chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic OpenAI chat-completions response body, exactly as the API
    /// sends it. Guards against the field-name regression (e.g. an LLM-client
    /// side that reads `usage.input_tokens` when the API sends
    /// `usage.prompt_tokens`) that produced silent zero-usage trajectory
    /// events in production.
    const OPENAI_COMPLETIONS_BODY: &str = r#"{
        "id": "chatcmpl-abc123",
        "object": "chat.completion",
        "created": 1725612345,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "<mxfile>...</mxfile>"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 234,
            "completion_tokens": 1023,
            "total_tokens": 1257
        }
    }"#;

    #[test]
    fn parse_usage_reads_openai_field_names() {
        let body: Value = serde_json::from_str(OPENAI_COMPLETIONS_BODY).unwrap();
        let usage = parse_usage(&body).unwrap();
        assert_eq!(usage.input_tokens, 234);
        assert_eq!(usage.output_tokens, 1023);
        assert_eq!(usage.total(), 1257);
    }

    #[test]
    fn parse_content_extracts_assistant_message() {
        let body: Value = serde_json::from_str(OPENAI_COMPLETIONS_BODY).unwrap();
        assert_eq!(parse_content(&body).unwrap(), "<mxfile>...</mxfile>");
        assert_eq!(parse_finish_reason(&body).as_deref(), Some("stop"));
    }

    #[test]
    fn parse_usage_errors_when_usage_object_missing() {
        let body: Value = serde_json::from_str(r#"{"choices": []}"#).unwrap();
        let err = parse_usage(&body).unwrap_err();
        assert!(
            err.to_string().contains("usage"),
            "error should name the missing field: {err}"
        );
    }

    #[test]
    fn parse_fix_envelope_requires_xml_and_reads_done() {
        let env = parse_fix_envelope(
            r#"{"done": false, "xml": "<mxfile/>", "issues": [], "reasoning": "wip"}"#,
        )
        .unwrap();
        assert!(!env.done);
        assert_eq!(env.xml, "<mxfile/>");
        assert!(env.issues.is_empty());
        assert_eq!(env.reasoning.as_deref(), Some("wip"));
    }

    #[test]
    fn parse_fix_envelope_defaults_done_to_true_and_tolerates_missing_fields() {
        let env = parse_fix_envelope(r#"{"xml": "<mxfile/>"}"#).unwrap();
        assert!(env.done, "missing done should default to true");
        assert!(env.issues.is_empty());
        assert!(env.reasoning.is_none());
    }

    #[test]
    fn parse_fix_envelope_reads_self_reported_issues() {
        let env = parse_fix_envelope(
            r#"{"done": false,
                "xml": "<mxfile/>",
                "issues": [{"kind": "overlap", "severity": "high",
                             "cell_ids": ["2", "3"], "description": "A overlaps B"}]}"#,
        )
        .unwrap();
        assert_eq!(env.issues.len(), 1);
        assert_eq!(env.issues[0].kind, "overlap");
        assert_eq!(env.issues[0].cell_ids, vec!["2", "3"]);
    }

    #[test]
    fn parse_fix_envelope_errors_when_xml_missing() {
        let err = parse_fix_envelope(r#"{"done": true}"#).unwrap_err();
        assert!(
            err.contains("xml"),
            "error should name the missing field: {err}"
        );
    }

    #[test]
    fn parse_fix_envelope_errors_on_non_json_content() {
        let err = parse_fix_envelope("<mxfile>bare xml, no envelope</mxfile>").unwrap_err();
        assert!(
            err.contains("JSON"),
            "bare XML must be rejected with a JSON hint: {err}"
        );
    }
}