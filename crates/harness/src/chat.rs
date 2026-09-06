//! Minimal OpenAI-compatible chat client (text in, text out). Vision image
//! parts are a follow-up (same endpoint, `content` becomes an array).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: content.into() }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: content.into() }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".into(), content: content.into() }
    }
}

#[derive(Debug, Error)]
pub enum ChatError {
    #[error("llm endpoint not configured: set DRAWIO_LLM_BASE_URL, DRAWIO_LLM_MODEL, DRAWIO_LLM_API_KEY")]
    NotConfigured,
    #[error("http: {0}")]
    Http(String),
    #[error("api error: {0}")]
    Api(String),
    #[error("empty assistant reply")]
    Empty,
}

/// Sends chat-completion requests; mocked in tests via the trait.
#[async_trait::async_trait]
pub trait Chat: Send {
    async fn complete(&mut self, messages: &[Message]) -> Result<String, ChatError>;
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
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: RespMessage,
}

#[derive(Debug, Deserialize)]
struct RespMessage {
    content: Option<String>,
}

impl OpenAiChat {
    /// Read config from the environment; Err when incomplete.
    pub fn from_env() -> Result<Self, ChatError> {
        let base_url = std::env::var("DRAWIO_LLM_BASE_URL")
            .map_err(|_| ChatError::NotConfigured)?;
        let model = std::env::var("DRAWIO_LLM_MODEL")
            .map_err(|_| ChatError::NotConfigured)?;
        let api_key = std::env::var("DRAWIO_LLM_API_KEY")
            .unwrap_or_default();
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            api_key,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(180))
                .build()
                .map_err(|e| ChatError::Http(e.to_string()))?,
        })
    }

    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

#[async_trait::async_trait]
impl Chat for OpenAiChat {
    async fn complete(&mut self, messages: &[Message]) -> Result<String, ChatError> {
        let mut body = serde_json::Map::new();
        body.insert("model".into(), serde_json::json!(self.model));
        let msgs: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| serde_json::json!({ "role": m.role, "content": m.content }))
            .collect();
        body.insert("messages".into(), serde_json::Value::Array(msgs));
        body.insert(
            "temperature".into(),
            serde_json::json!(0.7),
        );

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
        parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .filter(|c| !c.trim().is_empty())
            .ok_or(ChatError::Empty)
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
