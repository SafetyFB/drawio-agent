//! HTTP transport abstraction for LLM API calls.

use async_trait::async_trait;
use serde_json::Value;

/// A raw HTTP response from the transport layer.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Value,
}

/// Errors produced by the HTTP transport layer.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The server returned a non-2xx status.
    #[error("http status {status}: {body}")]
    Status { status: u16, body: String },
    /// The response could not be interpreted as expected.
    #[error("invalid response: {0}")]
    Invalid(String),
    /// A network-level failure occurred.
    #[error("network: {0}")]
    Network(String),
}

/// Minimal HTTP POST contract used by providers.
///
/// Implementors are responsible for their own client and concurrency; this
/// trait keeps providers mockable and dependency-light.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// POST a JSON body to `url` with the given headers.
    async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<HttpResponse, TransportError>;
}