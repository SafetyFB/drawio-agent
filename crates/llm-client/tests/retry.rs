//! TDD tests for RetryingTransport — exponential backoff + jitter.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use drawio_agent_llm_client::{
    HttpResponse, HttpTransport, RetryPolicy, RetryingTransport, Sleeper, TransportError,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Mocks
// ---------------------------------------------------------------------------

struct MockTransport {
    responses: Mutex<VecDeque<HttpResponse>>,
}

impl MockTransport {
    fn new(responses: Vec<HttpResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }
}

#[async_trait]
impl HttpTransport for MockTransport {
    async fn post_json(
        &self,
        _url: &str,
        _headers: &[(&str, &str)],
        _body: &Value,
    ) -> Result<HttpResponse, TransportError> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| TransportError::Invalid("no queued response".into()))
    }
}

struct MockSleeper {
    sleeps: Mutex<Vec<u64>>,
}

impl MockSleeper {
    fn new() -> Self {
        Self {
            sleeps: Mutex::new(Vec::new()),
        }
    }
    fn recorded(&self) -> Vec<u64> {
        self.sleeps.lock().unwrap().clone()
    }
}

#[async_trait]
impl Sleeper for MockSleeper {
    async fn sleep(&self, ms: u64) {
        self.sleeps.lock().unwrap().push(ms);
    }
}

fn status(code: u16) -> HttpResponse {
    HttpResponse {
        status: code,
        body: json!({}),
    }
}

fn default_policy() -> RetryPolicy {
    RetryPolicy {
        max_retries: 3,
        initial_backoff_ms: 100,
        max_backoff_ms: 10_000,
        backoff_multiplier: 2.0,
        jitter_ms: 0, // tests override for jitter cases
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn succeeds_first_try_no_retry() {
    let inner = Arc::new(MockTransport::new(vec![status(200)]));
    let sleeper = Arc::new(MockSleeper::new());
    let transport = RetryingTransport::new(inner, default_policy(), sleeper.clone());

    let resp = transport.post_json("http://x", &[], &json!({})).await.unwrap();
    assert_eq!(resp.status, 200);
    assert!(
        sleeper.recorded().is_empty(),
        "no sleeps on first-try success"
    );
}

#[tokio::test]
async fn retries_on_5xx_then_succeeds() {
    let inner = Arc::new(MockTransport::new(vec![status(503), status(200)]));
    let sleeper = Arc::new(MockSleeper::new());
    let transport = RetryingTransport::new(inner, default_policy(), sleeper.clone());

    let resp = transport.post_json("http://x", &[], &json!({})).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(sleeper.recorded().len(), 1, "one retry = one sleep");
}

#[tokio::test]
async fn fails_after_max_retries() {
    let inner = Arc::new(MockTransport::new(vec![
        status(503),
        status(503),
        status(503),
        status(503), // exceeds max_retries=3 (original + 3 retries = 4 attempts)
    ]));
    let sleeper = Arc::new(MockSleeper::new());
    let transport = RetryingTransport::new(inner, default_policy(), sleeper);

    let err = transport
        .post_json("http://x", &[], &json!({}))
        .await
        .expect_err("should give up");
    match err {
        TransportError::Status { status, .. } => assert_eq!(status, 503),
        other => panic!("expected Status error, got {other:?}"),
    }
}

#[tokio::test]
async fn does_not_retry_on_4xx() {
    let inner = Arc::new(MockTransport::new(vec![status(400)]));
    let sleeper = Arc::new(MockSleeper::new());
    let transport = RetryingTransport::new(inner, default_policy(), sleeper.clone());

    let err = transport
        .post_json("http://x", &[], &json!({}))
        .await
        .expect_err("400 must error");
    match err {
        TransportError::Status { status, .. } => assert_eq!(status, 400),
        other => panic!("expected Status 400, got {other:?}"),
    }
    assert!(
        sleeper.recorded().is_empty(),
        "no retry on 4xx client errors"
    );
}

#[tokio::test]
async fn does_not_retry_on_401() {
    let inner = Arc::new(MockTransport::new(vec![status(401)]));
    let sleeper = Arc::new(MockSleeper::new());
    let transport = RetryingTransport::new(inner, default_policy(), sleeper.clone());

    let _ = transport.post_json("http://x", &[], &json!({})).await;
    assert!(sleeper.recorded().is_empty());
}

#[tokio::test]
async fn exponential_backoff_grows_each_attempt() {
    let inner = Arc::new(MockTransport::new(vec![
        status(503),
        status(503),
        status(503),
        status(200),
    ]));
    let sleeper = Arc::new(MockSleeper::new());
    let transport = RetryingTransport::new(
        inner,
        RetryPolicy {
            max_retries: 5,
            initial_backoff_ms: 100,
            max_backoff_ms: 100_000,
            backoff_multiplier: 2.0,
            jitter_ms: 0,
        },
        sleeper.clone(),
    );

    transport.post_json("http://x", &[], &json!({})).await.unwrap();
    let sleeps = sleeper.recorded();
    assert_eq!(sleeps.len(), 3, "3 retries before success");
    // 100ms * 2.0^n, n=0,1,2 → 100, 200, 400
    assert_eq!(sleeps[0], 100);
    assert_eq!(sleeps[1], 200);
    assert_eq!(sleeps[2], 400);
}

#[tokio::test]
async fn max_backoff_caps_exponential_growth() {
    let inner = Arc::new(MockTransport::new(vec![
        status(503),
        status(503),
        status(503),
        status(503),
        status(200),
    ]));
    let sleeper = Arc::new(MockSleeper::new());
    let transport = RetryingTransport::new(
        inner,
        RetryPolicy {
            max_retries: 5,
            initial_backoff_ms: 100,
            max_backoff_ms: 250,
            backoff_multiplier: 2.0,
            jitter_ms: 0,
        },
        sleeper.clone(),
    );

    transport.post_json("http://x", &[], &json!({})).await.unwrap();
    let sleeps = sleeper.recorded();
    assert_eq!(sleeps.len(), 4);
    // Raw series would be 100, 200, 400, 800 → capped to 250 from 3rd onward
    assert_eq!(sleeps[0], 100);
    assert_eq!(sleeps[1], 200);
    assert_eq!(sleeps[2], 250);
    assert_eq!(sleeps[3], 250);
}

#[tokio::test]
async fn jitter_keeps_sleep_within_range() {
    // initial=100, jitter=20 → each sleep in [80, 120]
    for _ in 0..10 {
        let inner = Arc::new(MockTransport::new(vec![status(503), status(200)]));
        let sleeper = Arc::new(MockSleeper::new());
        let transport = RetryingTransport::new(
            inner,
            RetryPolicy {
                max_retries: 3,
                initial_backoff_ms: 100,
                max_backoff_ms: 10_000,
                backoff_multiplier: 2.0,
                jitter_ms: 20,
            },
            sleeper.clone(),
        );
        transport.post_json("http://x", &[], &json!({})).await.unwrap();
        let sleeps = sleeper.recorded();
        assert_eq!(sleeps.len(), 1);
        let s = sleeps[0];
        assert!((80..=120).contains(&s), "sleep {s} out of [80, 120]");
    }
}

#[tokio::test]
async fn jitter_zero_means_exact_backoff() {
    let inner = Arc::new(MockTransport::new(vec![status(503), status(200)]));
    let sleeper = Arc::new(MockSleeper::new());
    let transport = RetryingTransport::new(
        inner,
        RetryPolicy {
            max_retries: 3,
            initial_backoff_ms: 100,
            max_backoff_ms: 10_000,
            backoff_multiplier: 2.0,
            jitter_ms: 0,
        },
        sleeper.clone(),
    );
    transport.post_json("http://x", &[], &json!({})).await.unwrap();
    assert_eq!(sleeper.recorded(), vec![100]);
}

#[tokio::test]
async fn no_retries_means_no_sleeps_even_on_5xx() {
    let inner = Arc::new(MockTransport::new(vec![status(503)]));
    let sleeper = Arc::new(MockSleeper::new());
    let transport = RetryingTransport::new(
        inner,
        RetryPolicy {
            max_retries: 0,
            ..RetryPolicy::default()
        },
        sleeper.clone(),
    );
    let err = transport.post_json("http://x", &[], &json!({})).await.unwrap_err();
    assert!(matches!(err, TransportError::Status { status: 503, .. }));
    assert!(sleeper.recorded().is_empty());
}

#[tokio::test]
async fn retry_on_500_server_error() {
    let inner = Arc::new(MockTransport::new(vec![status(500), status(200)]));
    let sleeper = Arc::new(MockSleeper::new());
    let transport = RetryingTransport::new(inner, default_policy(), sleeper.clone());
    let resp = transport.post_json("http://x", &[], &json!({})).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(sleeper.recorded().len(), 1);
}

#[tokio::test]
async fn retry_on_429_rate_limit() {
    let inner = Arc::new(MockTransport::new(vec![status(429), status(200)]));
    let sleeper = Arc::new(MockSleeper::new());
    let transport = RetryingTransport::new(inner, default_policy(), sleeper.clone());
    let resp = transport.post_json("http://x", &[], &json!({})).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(sleeper.recorded().len(), 1);
}

#[tokio::test]
async fn retry_policy_default_values() {
    let p = RetryPolicy::default();
    assert!(p.max_retries > 0, "default must allow retries");
    assert!(p.initial_backoff_ms > 0);
    assert!(p.backoff_multiplier > 1.0);
    assert!(p.max_backoff_ms >= p.initial_backoff_ms);
}
