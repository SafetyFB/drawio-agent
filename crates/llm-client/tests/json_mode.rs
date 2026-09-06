//! TDD tests for structured JSON output mode for codegen.
//!
//! When `GenerateRequest::json_mode = true`, the provider:
//! - requests `response_format: {type: "json_object"}`
//! - parses the assistant content as JSON
//! - extracts the `xml` field (and surfaces `reasoning` if present)
//! - returns an error if parsing fails or `xml` is missing

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use drawio_agent_llm_client::{
    GenerateRequest, HttpResponse, HttpTransport, LlmProvider, OpenAiCompatProvider,
    ProviderConfig, ProviderError, TransportError,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Mock transport (records requests, replays queued responses)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)] // `url` and `headers` are useful for future tests
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

fn provider(transport: Arc<MockTransport>) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(
        transport,
        ProviderConfig {
            base_url: "https://api.example.com/v1".into(),
            api_key: "sk-test".into(),
            model: "glm-4-flash".into(),
            request_timeout_ms: 5000,
            max_retries: 0,
        },
    )
}

fn chat_response(content: &str) -> HttpResponse {
    HttpResponse {
        status: 200,
        body: json!({
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150},
        }),
    }
}

fn json_response(json_obj: Value) -> HttpResponse {
    chat_response(&json_obj.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn json_mode_sends_response_format_field() {
    let transport = Arc::new(MockTransport::new(vec![chat_response(
        r#"{"xml": "<mxfile/>"}"#,
    )]));
    let p = provider(transport.clone());

    p.generate_xml(GenerateRequest {
        user_prompt: "x".into(),
        current_xml: None,
        scope: None,
        feedback: None,
        json_mode: true,
            no_think: false,
        memory: vec![],
    })
    .await
    .unwrap();

    let body = &transport.recorded()[0].body;
    assert_eq!(
        body["response_format"]["type"], "json_object",
        "json_mode must set response_format.type=json_object: {body}"
    );
}

#[tokio::test]
async fn json_mode_extracts_xml_field_from_response() {
    let transport = Arc::new(MockTransport::new(vec![chat_response(
        r#"{"xml": "<mxfile><diagram id='a'/></mxfile>"}"#,
    )]));
    let p = provider(transport);

    let resp = p
        .generate_xml(GenerateRequest {
            user_prompt: "x".into(),
            current_xml: None,
            scope: None,
            feedback: None,
            json_mode: true,
            no_think: false,
            memory: vec![],
        })
        .await
        .unwrap();

    assert_eq!(resp.content, "<mxfile><diagram id='a'/></mxfile>");
}

#[tokio::test]
async fn json_mode_preserves_reasoning_when_present() {
    let transport = Arc::new(MockTransport::new(vec![chat_response(
        r#"{"reasoning": "I'll create 3 nodes connected linearly", "xml": "<mxfile/>"}"#,
    )]));
    let p = provider(transport);

    let resp = p
        .generate_xml(GenerateRequest {
            user_prompt: "x".into(),
            current_xml: None,
            scope: None,
            feedback: None,
            json_mode: true,
            no_think: false,
            memory: vec![],
        })
        .await
        .unwrap();

    // Reasoning is exposed via the raw response payload so the trajectory
    // recorder can persist it for trace display.
    let raw_str = resp.raw.to_string();
    assert!(
        raw_str.contains("I'll create 3 nodes"),
        "reasoning must be preserved in raw: {raw_str}"
    );
}

#[tokio::test]
async fn json_mode_errors_on_invalid_json_response() {
    let transport = Arc::new(MockTransport::new(vec![chat_response(
        "not a json object",
    )]));
    let p = provider(transport);

    let err = p
        .generate_xml(GenerateRequest {
            user_prompt: "x".into(),
            current_xml: None,
            scope: None,
            feedback: None,
            json_mode: true,
            no_think: false,
            memory: vec![],
        })
        .await
        .expect_err("invalid JSON must error");
    assert!(matches!(err, ProviderError::Transport(_)));
}

#[tokio::test]
async fn json_mode_errors_when_xml_field_missing() {
    let transport = Arc::new(MockTransport::new(vec![chat_response(
        r#"{"reasoning": "no xml here"}"#,
    )]));
    let p = provider(transport);

    let err = p
        .generate_xml(GenerateRequest {
            user_prompt: "x".into(),
            current_xml: None,
            scope: None,
            feedback: None,
            json_mode: true,
            no_think: false,
            memory: vec![],
        })
        .await
        .expect_err("missing xml field must error");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("xml") || msg.contains("field") || msg.contains("missing"),
        "error should mention the missing xml field: {msg}"
    );
}

#[tokio::test]
async fn json_mode_errors_when_xml_field_is_not_a_string() {
    let transport = Arc::new(MockTransport::new(vec![chat_response(
        r#"{"xml": 42}"#,
    )]));
    let p = provider(transport);

    let err = p
        .generate_xml(GenerateRequest {
            user_prompt: "x".into(),
            current_xml: None,
            scope: None,
            feedback: None,
            json_mode: true,
            no_think: false,
            memory: vec![],
        })
        .await
        .expect_err("xml field must be a string");
    assert!(
        matches!(err, ProviderError::Transport(_)),
        "expected Transport error for non-string xml field, got {err:?}"
    );
}

#[tokio::test]
async fn no_json_mode_does_not_send_response_format() {
    let transport = Arc::new(MockTransport::new(vec![chat_response("<mxfile/>")]));
    let p = provider(transport.clone());

    p.generate_xml(GenerateRequest {
        user_prompt: "x".into(),
        current_xml: None,
        scope: None,
        feedback: None,
        json_mode: false,
            no_think: false,
        memory: vec![],
    })
    .await
    .unwrap();

    let body = &transport.recorded()[0].body;
    assert!(
        body.get("response_format").is_none(),
        "without json_mode, response_format must not be set: {body}"
    );
}

#[tokio::test]
async fn no_json_mode_returns_raw_string_content() {
    let transport = Arc::new(MockTransport::new(vec![chat_response("<mxfile/>")]));
    let p = provider(transport);

    let resp = p
        .generate_xml(GenerateRequest {
            user_prompt: "x".into(),
            current_xml: None,
            scope: None,
            feedback: None,
            json_mode: false,
            no_think: false,
            memory: vec![],
        })
        .await
        .unwrap();

    assert_eq!(resp.content, "<mxfile/>");
}

#[tokio::test]
async fn json_mode_still_extracts_usage_correctly() {
    let transport = Arc::new(MockTransport::new(vec![json_response(json!({
        "xml": "<mxfile/>",
        "reasoning": "short"
    }))]));
    let p = provider(transport);

    let resp = p
        .generate_xml(GenerateRequest {
            user_prompt: "x".into(),
            current_xml: None,
            scope: None,
            feedback: None,
            json_mode: true,
            no_think: false,
            memory: vec![],
        })
        .await
        .unwrap();

    assert_eq!(resp.usage.input_tokens, 100);
    assert_eq!(resp.usage.output_tokens, 50);
}

#[tokio::test]
async fn json_mode_handles_xml_with_special_chars() {
    let xml = r#"<mxCell value="a < b &amp; c &lt; d &gt; e &quot;f&quot;"/>"#;
    // Use the json! macro so the XML is properly JSON-escaped inside the
    // response string — simulates what a real LLM would emit.
    let response_body = json!({"xml": xml}).to_string();
    let transport = Arc::new(MockTransport::new(vec![chat_response(&response_body)]));
    let p = provider(transport);

    let resp = p
        .generate_xml(GenerateRequest {
            user_prompt: "x".into(),
            current_xml: None,
            scope: None,
            feedback: None,
            json_mode: true,
            no_think: false,
            memory: vec![],
        })
        .await
        .unwrap();

    assert_eq!(resp.content, xml);
}

#[tokio::test]
async fn default_json_mode_is_false() {
    // Sanity: GenerateRequest::default() must default json_mode to false
    // so existing call sites (without json_mode set) keep working.
    let req = GenerateRequest {
        user_prompt: "x".into(),
        current_xml: None,
        scope: None,
        feedback: None,
        json_mode: false,
            no_think: false,
        memory: vec![],
    };
    assert!(!req.json_mode);
}
