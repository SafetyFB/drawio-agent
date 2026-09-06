//! Minimal OpenAI-compatible chat client (text in, text out). Vision image
//! parts are a follow-up (same endpoint, `content` becomes an array).

use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

/// One content part of a chat message. Text is the default; images are sent
/// as OpenAI-style `image_url` parts with a base64 data URI (the format GLM
/// and most OpenAI-compatible vision endpoints accept).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Part {
    Text(String),
    /// Raw PNG bytes; serialized to `data:image/png;base64,…`.
    ImagePng(Vec<u8>),
}

impl Part {
    pub fn text(s: impl Into<String>) -> Self {
        Part::Text(s.into())
    }
    pub fn image_png(bytes: Vec<u8>) -> Self {
        Part::ImagePng(bytes)
    }

    fn to_json(&self) -> Value {
        match self {
            Part::Text(t) => json!({ "type": "text", "text": t }),
            Part::ImagePng(png) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(png);
                json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:image/png;base64,{b64}")}
                })
            }
        }
    }
}

/// A chat message: role + ordered parts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), parts: vec![Part::Text(content.into())] }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: "assistant".into(), parts: vec![Part::Text(content.into())] }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".into(), parts: vec![Part::Text(content.into())] }
    }
    pub fn with_parts(role: impl Into<String>, parts: Vec<Part>) -> Self {
        Self { role: role.into(), parts }
    }

    /// Serialize `content` the way the wire protocol wants it: a bare string
    /// for pure-text messages (max compatibility), an array of typed parts
    /// otherwise.
    pub fn content_json(&self) -> Value {
        if self.parts.len() == 1 {
            if let Part::Text(t) = &self.parts[0] {
                return json!(t);
            }
        }
        Value::Array(self.parts.iter().map(|p| p.to_json()).collect())
    }
}

#[derive(Debug, Error)]
pub enum ChatError {
    #[error("llm not configured: {0}")]
    NotConfigured(String),
    #[error("http: {0}")]
    Http(String),
    #[error("api error: {0}")]
    Api(String),
    #[error("empty assistant reply")]
    Empty,
}

/// Token usage reported by the API (OpenAI `usage` block).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// Per-call options (thinking mode).
#[derive(Debug, Clone, Copy, Default)]
pub struct CallOpts {
    /// `thinking: {"type": "disabled"}` — GLM 4.6+ fast path.
    pub no_think: bool,
}

/// Result of one chat completion: the assistant text plus usage.
#[derive(Debug, Clone)]
pub struct Reply {
    pub text: String,
    pub usage: Usage,
}

/// Sends chat-completion requests; mocked in tests via the trait.
#[async_trait::async_trait]
pub trait Chat: Send {
    async fn complete(&mut self, messages: &[Message], opts: &CallOpts) -> Result<Reply, ChatError>;
}

#[derive(Debug, Clone)]
pub struct OpenAiChat {
    pub base_url: String,
    pub model: String,
    api_key: String,
    client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: RespMessage,
}

#[derive(Debug, Deserialize)]
struct RespMessage {
    content: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

impl OpenAiChat {
    pub fn from_settings(s: &crate::config::LlmSettings) -> Result<Self, ChatError> {
        if s.base_url.is_empty() || s.model.is_empty() {
            return Err(ChatError::NotConfigured(
                "base_url 与 model 不能为空".into(),
            ));
        }
        Ok(Self {
            base_url: s.base_url.trim_end_matches('/').to_string(),
            model: s.model.clone(),
            api_key: s.api_key.clone(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(180))
                .build()
                .map_err(|e| ChatError::Http(e.to_string()))?,
        })
    }

    /// Read the effective configuration (config file first, env fallback).
    pub fn from_effective() -> Result<Self, ChatError> {
        crate::config::effective_settings()
            .ok_or_else(|| {
                ChatError::NotConfigured(crate::config::UNCONFIGURED_MSG.to_string())
            })
            .and_then(|s| Self::from_settings(&s))
    }

    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

#[async_trait::async_trait]
impl Chat for OpenAiChat {
    async fn complete(&mut self, messages: &[Message], opts: &CallOpts) -> Result<Reply, ChatError> {
        let mut body = serde_json::Map::new();
        body.insert("model".into(), serde_json::json!(self.model));
        let msgs: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| serde_json::json!({ "role": m.role, "content": m.content_json() }))
            .collect();
        body.insert("messages".into(), serde_json::Value::Array(msgs));
        body.insert(
            "temperature".into(),
            serde_json::json!(0.7),
        );
        if opts.no_think {
            body.insert("thinking".into(), serde_json::json!({"type": "disabled"}));
        }

        let mut req = self
            .client
            .post(self.endpoint())
            .json(&body)
            .header("content-type", "application/json");
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ChatError::Http(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ChatError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(ChatError::Api(format!("HTTP {status}: {}", truncate(&text, 400))));
        }
        let parsed: ChatResponse = serde_json::from_str(&text)
            .map_err(|e| ChatError::Api(format!("bad response shape: {e}: {}", truncate(&text, 400))))?;
        let usage = parsed.usage.unwrap_or_default();
        let text = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .filter(|c| !c.trim().is_empty())
            .ok_or(ChatError::Empty)?;
        Ok(Reply {
            text,
            usage: Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
            },
        })
    }
}

fn truncate(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_only_content_is_bare_string() {
        let m = Message::user("hi");
        assert_eq!(m.content_json(), json!("hi"));
    }

    #[test]
    fn image_part_serializes_as_data_uri_array() {
        let m = Message::with_parts(
            "user",
            vec![
                Part::text("看这张图"),
                Part::image_png(vec![0x89, b'P', b'N', b'G', 1, 2, 3]),
            ],
        );
        let v = m.content_json();
        let arr = v.as_array().unwrap();
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[1]["type"], "image_url");
        let url = arr[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
        assert!(url.contains("ECAw"), "raw bytes present in base64: {url}");
    }
}
