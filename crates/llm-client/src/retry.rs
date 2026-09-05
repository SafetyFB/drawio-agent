//! Retrying transport — wraps an [`HttpTransport`] with exponential
//! backoff and jitter. Retryable errors: 5xx, 429, and network failures.
//! Non-retryable: 4xx (other than 429) and parse failures.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::transport::{HttpResponse, HttpTransport, TransportError};

/// Configuration for [`RetryingTransport`].
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum retries AFTER the initial attempt. `0` disables retries.
    pub max_retries: u32,
    /// Sleep before the first retry, in milliseconds.
    pub initial_backoff_ms: u64,
    /// Cap on backoff growth.
    pub max_backoff_ms: u64,
    /// Multiplier applied to the previous backoff on each retry (e.g. 2.0).
    pub backoff_multiplier: f64,
    /// Uniform jitter range in milliseconds. Final sleep is in
    /// `[base - jitter_ms, base + jitter_ms]`, clamped to `>= 0`.
    pub jitter_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff_ms: 500,
            max_backoff_ms: 30_000,
            backoff_multiplier: 2.0,
            jitter_ms: 100,
        }
    }
}

/// Pluggable time abstraction so tests can record sleeps without waiting.
#[async_trait]
pub trait Sleeper: Send + Sync {
    async fn sleep(&self, ms: u64);
}

/// Production sleeper backed by tokio's timer.
#[derive(Debug)]
pub struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, ms: u64) {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
}

/// Decorates any [`HttpTransport`] with retry behavior.
pub struct RetryingTransport {
    inner: Arc<dyn HttpTransport>,
    policy: RetryPolicy,
    sleeper: Arc<dyn Sleeper>,
}

impl std::fmt::Debug for RetryingTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryingTransport")
            .field("policy", &self.policy)
            .field("inner", &"<dyn HttpTransport>")
            .field("sleeper", &"<dyn Sleeper>")
            .finish()
    }
}

impl RetryingTransport {
    pub fn new(
        inner: Arc<dyn HttpTransport>,
        policy: RetryPolicy,
        sleeper: Arc<dyn Sleeper>,
    ) -> Self {
        Self {
            inner,
            policy,
            sleeper,
        }
    }
}

fn is_retryable(err: &TransportError) -> bool {
    match err {
        TransportError::Status { status, .. } => {
            *status == 429 || (*status >= 500 && *status < 600)
        }
        TransportError::Network(_) => true,
        TransportError::Invalid(_) => false,
    }
}

/// Compute the sleep duration for the Nth retry (N=0 for first retry).
fn compute_backoff(attempt: u32, policy: &RetryPolicy) -> u64 {
    let raw = (policy.initial_backoff_ms as f64)
        * policy.backoff_multiplier.powi(attempt as i32);
    let base = raw.min(policy.max_backoff_ms as f64).max(0.0) as u64;

    if policy.jitter_ms == 0 {
        return base;
    }

    // Uniform jitter in [base - jitter_ms, base + jitter_ms], clamped >= 0.
    let range = 2 * policy.jitter_ms + 1;
    let offset = rand::random::<u64>() % range;
    let delta = offset as i64 - policy.jitter_ms as i64;
    let total = base as i64 + delta;
    total.max(0) as u64
}

#[async_trait]
impl HttpTransport for RetryingTransport {
    async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<HttpResponse, TransportError> {
        let mut last_err: Option<TransportError> = None;

        for attempt in 0..=self.policy.max_retries {
            // The inner transport returns Ok(HttpResponse) for any HTTP
            // status. We convert non-2xx to errors so the retry decision
            // below sees a uniform error type. 2xx → Ok; everything else →
            // Err(TransportError::Status).
            let raw = self.inner.post_json(url, headers, body).await;
            let converted = match raw {
                Ok(resp) if (200..300).contains(&resp.status) => Ok(resp),
                Ok(resp) => {
                    let body_str =
                        serde_json::to_string(&resp.body).unwrap_or_default();
                    Err(TransportError::Status {
                        status: resp.status,
                        body: body_str,
                    })
                }
                Err(err) => Err(err),
            };

            match converted {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    if !is_retryable(&err) {
                        return Err(err);
                    }
                    last_err = Some(err);
                    if attempt < self.policy.max_retries {
                        let sleep_ms = compute_backoff(attempt, &self.policy);
                        self.sleeper.sleep(sleep_ms).await;
                    }
                }
            }
        }

        Err(last_err.expect("at least one attempt was made"))
    }
}
