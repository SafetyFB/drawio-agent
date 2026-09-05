//! TDD tests for HttpTransport + LlmProvider + OpenAiCompatProvider.
//!
//! All HTTP interactions are mocked via `MockTransport`, so tests stay
//! hermetic and run without network access.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use drawio_agent_llm_client::{
    GenerateRequest, HttpResponse, HttpTransport, LlmProvider, OpenAiCompatProvider,
    ProviderConfig, ReviewRequest, TransportError,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Mock transport
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RecordedRequest {
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
}

struct MockTransport {
    responses: Mutex<VecDeque<HttpResponse>>,
    recorded: Mutex<Vec<RecordedRequest>>,
}

impl MockTransport {
    fn new(responses: Vec<HttpResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            recorded: Mutex::new(Vec::new()),
        }
    }

    fn recorded(&self) -> Vec<RecordedRequest> {
        self.recorded.lock().unwrap().clone()
    }
}

#[async_trait]
impl HttpTransport for MockTransport {
    async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<HttpResponse, TransportError> {
        let req = RecordedRequest {
            url: url.to_string(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            body: body.clone(),
        };
        self.recorded.lock().unwrap().push(req);

        let mut q = self.responses.lock().unwrap();
        q.pop_front()
            .ok_or_else(|| TransportError::Invalid("no queued response".into()))
    }
}

fn provider(transport: Arc<MockTransport>, model: &str) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(
        transport,
        ProviderConfig {
            base_url: "https://api.example.com/v1".into(),
            api_key: "sk-test-key".into(),
            model: model.to_string(),
            request_timeout_ms: 5000,
            max_retries: 0,
        },
    )
}

fn chat_completion_response(content: &str, prompt_tokens: u64, completion_tokens: u64) -> HttpResponse {
    HttpResponse {
        status: 200,
        body: json!({
            "id": "chatcmpl-abc",
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop",
            }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            },
        }),
    }
}

// ---------------------------------------------------------------------------
// generate_xml tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn generate_posts_to_chat_completions_endpoint() {
    let transport = Arc::new(MockTransport::new(vec![chat_completion_response(
        "<mxfile></mxfile>",
        10,
        5,
    )]));
    let p = provider(transport.clone(), "glm-4-flash");

    let resp = p
        .generate_xml(GenerateRequest {
            user_prompt: "Draw a circle".into(),
            current_xml: None,
            scope: None,
            feedback: None,
            ..Default::default()
        })
        .await
        .expect("generate should succeed");

    let reqs = transport.recorded();
    assert_eq!(reqs.len(), 1, "exactly one HTTP call");
    assert_eq!(
        reqs[0].url,
        "https://api.example.com/v1/chat/completions",
        "must hit chat/completions"
    );
    assert_eq!(resp.content, "<mxfile></mxfile>");
}

#[tokio::test]
async fn generate_includes_bearer_authorization_header() {
    let transport = Arc::new(MockTransport::new(vec![chat_completion_response(
        "<mxfile/>", 1, 1,
    )]));
    let p = provider(transport.clone(), "glm-4-flash");

    p.generate_xml(GenerateRequest {
        user_prompt: "x".into(),
        current_xml: None,
        scope: None,
        feedback: None,
        ..Default::default()
    })
    .await
    .unwrap();

    let recorded = transport.recorded();
    let auth = recorded[0]
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .expect("authorization header present");
    assert_eq!(auth.1, "Bearer sk-test-key");
}

#[tokio::test]
async fn generate_request_body_has_model_and_messages() {
    let transport = Arc::new(MockTransport::new(vec![chat_completion_response(
        "<mxfile/>", 1, 1,
    )]));
    let p = provider(transport.clone(), "qwen-vl-plus");

    p.generate_xml(GenerateRequest {
        user_prompt: "Draw a node".into(),
        current_xml: None,
        scope: None,
        feedback: None,
        ..Default::default()
    })
    .await
    .unwrap();

    let body = &transport.recorded()[0].body;
    assert_eq!(body["model"], "qwen-vl-plus");
    let messages = body["messages"].as_array().expect("messages array");
    assert!(messages.len() >= 2, "system + user at minimum");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "user");
}

#[tokio::test]
async fn generate_response_usage_maps_to_usage_type() {
    let transport = Arc::new(MockTransport::new(vec![chat_completion_response(
        "<mxfile/>", 850, 240,
    )]));
    let p = provider(transport, "glm-4-flash");

    let resp = p
        .generate_xml(GenerateRequest {
            user_prompt: "x".into(),
            current_xml: None,
            scope: None,
            feedback: None,
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(resp.usage.input_tokens, 850);
    assert_eq!(resp.usage.output_tokens, 240);
    assert_eq!(resp.usage.total(), 1090);
}

#[tokio::test]
async fn generate_error_on_non_2xx_response() {
    let transport = Arc::new(MockTransport::new(vec![HttpResponse {
        status: 401,
        body: json!({"error": {"message": "invalid api key"}}),
    }]));
    let p = provider(transport, "glm-4-flash");

    let err = p
        .generate_xml(GenerateRequest {
            user_prompt: "x".into(),
            current_xml: None,
            scope: None,
            feedback: None,
            ..Default::default()
        })
        .await
        .expect_err("must error on 401");

    let msg = err.to_string();
    assert!(
        msg.contains("401") || msg.to_lowercase().contains("status"),
        "error should reference status code: {msg}"
    );
}

#[tokio::test]
async fn generate_error_when_no_assistant_message_in_response() {
    let transport = Arc::new(MockTransport::new(vec![HttpResponse {
        status: 200,
        body: json!({"choices": []}),
    }]));
    let p = provider(transport, "glm-4-flash");

    let err = p
        .generate_xml(GenerateRequest {
            user_prompt: "x".into(),
            current_xml: None,
            scope: None,
            feedback: None,
            ..Default::default()
        })
        .await
        .expect_err("missing choices must error");
    assert!(err.to_string().to_lowercase().contains("choice") || err.to_string().to_lowercase().contains("invalid"));
}

#[tokio::test]
async fn provider_name_is_configured_model() {
    let transport = Arc::new(MockTransport::new(vec![]));
    let p = provider(transport, "my-custom-model");
    assert_eq!(p.name(), "my-custom-model");
}

#[tokio::test]
async fn response_raw_payload_is_preserved() {
    let transport = Arc::new(MockTransport::new(vec![chat_completion_response(
        "<mxfile/>", 1, 1,
    )]));
    let p = provider(transport, "glm-4-flash");

    let resp = p
        .generate_xml(GenerateRequest {
            user_prompt: "x".into(),
            current_xml: None,
            scope: None,
            feedback: None,
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(resp.raw["id"], "chatcmpl-abc");
}

#[tokio::test]
async fn generate_response_plumbs_finish_reason() {
    // Regression: the server used to hardcode finish_reason: None. A real
    // OpenAI response carries finish_reason="stop", and the provider must
    // copy it through to LlmResponse.
    let transport = Arc::new(MockTransport::new(vec![chat_completion_response(
        "<mxfile/>", 234, 1023,
    )]));
    let p = provider(transport, "glm-4-flash");

    let resp = p
        .generate_xml(GenerateRequest {
            user_prompt: "x".into(),
            current_xml: None,
            scope: None,
            feedback: None,
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
    assert_eq!(resp.usage.input_tokens, 234);
    assert_eq!(resp.usage.output_tokens, 1023);
}

// ---------------------------------------------------------------------------
// review_visual tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn review_sends_image_as_base64_data_url() {
    let transport = Arc::new(MockTransport::new(vec![chat_completion_response(
        r#"{"verdict":"pass","issues":[]}"#,
        100,
        20,
    )]));
    let p = provider(transport.clone(), "qwen-vl-plus");

    let png_bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]; // PNG magic
    p.review_visual(ReviewRequest {
        image_png: png_bytes.clone(),
        xml: "<mxfile/>".into(),
        checks: vec!["overlap".into(), "text_overflow".into()],
    })
    .await
    .unwrap();

    let body = &transport.recorded()[0].body;
    let user_msg = &body["messages"][1];
    let content = user_msg["content"].as_array().expect("multi-part content");
    assert!(content.len() >= 2, "image + text at minimum");

    let image_part = content
        .iter()
        .find(|c| c["type"] == "image_url")
        .expect("image_url part present");
    let url = image_part["image_url"]["url"].as_str().expect("data URL string");
    assert!(url.starts_with("data:image/png;base64,"));
    // Base64 of the PNG magic should appear after the prefix.
    use base64::Engine;
    let expected_b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    assert!(url.contains(&expected_b64), "encoded PNG must be in data URL");
}

#[tokio::test]
async fn review_response_parses_structured_issues() {
    let review_json = r#"{
        "verdict": "issues",
        "issues": [
            {"kind": "overlap", "severity": "high",
             "cell_ids": ["5", "7"],
             "description": "cells 5 and 7 overlap by 20px"},
            {"kind": "text_overflow", "severity": "medium",
             "cell_ids": ["12"],
             "description": "label of cell 12 overflows its container"}
        ]
    }"#;
    let transport = Arc::new(MockTransport::new(vec![chat_completion_response(
        review_json,
        200,
        80,
    )]));
    let p = provider(transport, "qwen-vl-plus");

    let resp = p
        .review_visual(ReviewRequest {
            image_png: vec![0u8; 16],
            xml: "<mxfile/>".into(),
            checks: vec!["overlap".into()],
        })
        .await
        .unwrap();

    assert_eq!(resp.content.verdict, "issues");
    assert_eq!(resp.content.issues.len(), 2);
    assert_eq!(resp.content.issues[0].kind, "overlap");
    assert_eq!(resp.content.issues[0].cell_ids, vec!["5", "7"]);
    assert_eq!(resp.usage.input_tokens, 200);
    assert_eq!(resp.usage.output_tokens, 80);
}
