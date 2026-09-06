//! TDD tests for streaming generation via SSE.
//!
//! Phase 2 v2 uses a "parse-once" streaming model: the provider POSTs
//! with `stream: true`, receives the full SSE body, then parses it into
//! chunks. Real incremental streaming can layer on top later.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use drawio_agent_llm_client::{
    GenerateRequest, HttpResponse, HttpTransport, LlmProvider, OpenAiCompatProvider,
    ProviderConfig, TransportError,
};
use futures::StreamExt;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Mock transport
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
        self.recorded.lock().unwrap().push(RecordedRequest {
            url: url.to_string(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            body: body.clone(),
        });
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

fn sse_response(chunks: &[String]) -> HttpResponse {
    let body_text = chunks.join("\n\n") + "\n\n";
    HttpResponse {
        status: 200,
        body: json!(body_text),
    }
}

fn chunk_with_content(content: &str) -> String {
    format!(
        r#"data: {{"choices":[{{"delta":{{"content":"{}"}},"finish_reason":null,"index":0}}]}}"#,
        content.replace('\n', "\\n").replace('"', "\\\"")
    )
}

fn chunk_with_finish(reason: &str) -> String {
    format!(
        r#"data: {{"choices":[{{"delta":{{}},"finish_reason":"{reason}","index":0}}],"usage":{{"prompt_tokens":42,"completion_tokens":17,"total_tokens":59}}}}"#
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_yields_chunks_in_order() {
    let sse = vec![
        chunk_with_content("Hello"),
        chunk_with_content(", "),
        chunk_with_content("World"),
        chunk_with_finish("stop"),
        "data: [DONE]".to_string(),
    ];
    let transport = Arc::new(MockTransport::new(vec![sse_response(&sse)]));
    let p = provider(transport);

    let mut stream = p
        .generate_streaming(GenerateRequest {
            user_prompt: "x".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let mut collected = Vec::new();
    while let Some(item) = stream.next().await {
        collected.push(item.expect("chunk should be Ok"));
    }

    let deltas: Vec<&str> = collected.iter().map(|c| c.delta.as_str()).collect();
    // Every SSE event is yielded, including the finish-only chunk with an
    // empty delta that carries `finish_reason` and `usage`.
    assert_eq!(deltas, vec!["Hello", ", ", "World", ""]);
}

#[tokio::test]
async fn streaming_concatenates_to_full_content() {
    let sse = vec![
        chunk_with_content("<mxfile>"),
        chunk_with_content("<diagram "),
        chunk_with_content("id=\"x\"/>"),
        chunk_with_content("</mxfile>"),
        chunk_with_finish("stop"),
        "data: [DONE]".to_string(),
    ];
    let transport = Arc::new(MockTransport::new(vec![sse_response(&sse)]));
    let p = provider(transport);

    let mut stream = p
        .generate_streaming(GenerateRequest {
            user_prompt: "x".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let mut full = String::new();
    while let Some(item) = stream.next().await {
        let chunk = item.unwrap();
        full.push_str(&chunk.delta);
    }
    assert_eq!(full, "<mxfile><diagram id=\"x\"/></mxfile>");
}

#[tokio::test]
async fn streaming_final_chunk_reports_finish_reason() {
    let sse = vec![
        chunk_with_content("a"),
        chunk_with_finish("stop"),
        "data: [DONE]".to_string(),
    ];
    let transport = Arc::new(MockTransport::new(vec![sse_response(&sse)]));
    let p = provider(transport);

    let mut stream = p
        .generate_streaming(GenerateRequest {
            user_prompt: "x".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item.unwrap());
    }

    // The last chunk (before [DONE]) carries finish_reason.
    let last = chunks.last().expect("at least one chunk");
    assert_eq!(last.finish_reason.as_deref(), Some("stop"));
}

#[tokio::test]
async fn streaming_reports_usage_in_final_chunk() {
    let sse = vec![
        chunk_with_content("hi"),
        chunk_with_finish("stop"),
        "data: [DONE]".to_string(),
    ];
    let transport = Arc::new(MockTransport::new(vec![sse_response(&sse)]));
    let p = provider(transport);

    let mut stream = p
        .generate_streaming(GenerateRequest {
            user_prompt: "x".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item.unwrap());
    }

    let last = chunks.last().unwrap();
    let usage = last.usage.expect("usage should be in final chunk");
    assert_eq!(usage.input_tokens, 42);
    assert_eq!(usage.output_tokens, 17);
}

#[tokio::test]
async fn streaming_sends_stream_true_in_request() {
    let sse: Vec<String> = vec![
        chunk_with_content("x"),
        chunk_with_finish("stop"),
        "data: [DONE]".to_string(),
    ];
    let transport = Arc::new(MockTransport::new(vec![sse_response(&sse)]));
    let p = provider(transport.clone());

    // Drain the stream so the returned `Pin<Box<dyn Stream>>` is consumed.
    {
        let mut stream = p
            .generate_streaming(GenerateRequest {
                user_prompt: "x".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        while stream.next().await.is_some() {}
    }

    let body = &transport.recorded()[0].body;
    assert_eq!(
        body["stream"], true,
        "streaming must set stream:true in body: {body}"
    );
}

#[tokio::test]
async fn streaming_terminates_at_done_marker() {
    let sse = vec![
        chunk_with_content("a"),
        chunk_with_content("b"),
        "data: [DONE]".to_string(),
        // Anything after [DONE] should be ignored.
        chunk_with_content("ignored"),
    ];
    let transport = Arc::new(MockTransport::new(vec![sse_response(&sse)]));
    let p = provider(transport);

    let mut stream = p
        .generate_streaming(GenerateRequest {
            user_prompt: "x".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item.unwrap());
    }

    let deltas: Vec<&str> = chunks.iter().map(|c| c.delta.as_str()).collect();
    assert_eq!(
        deltas,
        vec!["a", "b"],
        "stream must stop at [DONE], not include trailing data"
    );
}

#[tokio::test]
async fn streaming_skips_empty_lines_and_non_data_lines() {
    let sse: Vec<String> = vec![
        "".to_string(),                                       // empty line, ignore
        ": comment line".to_string(),                         // SSE comment, ignore
        chunk_with_content("real"),
        "garbage without data prefix".to_string(),            // ignore
        chunk_with_content("stuff"),
        "data: [DONE]".to_string(),
    ];
    let transport = Arc::new(MockTransport::new(vec![sse_response(&sse)]));
    let p = provider(transport);

    let mut stream = p
        .generate_streaming(GenerateRequest {
            user_prompt: "x".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let mut deltas = Vec::new();
    while let Some(item) = stream.next().await {
        deltas.push(item.unwrap().delta);
    }
    assert_eq!(deltas, vec!["real", "stuff"]);
}

#[tokio::test]
async fn streaming_error_on_non_2xx() {
    let transport = Arc::new(MockTransport::new(vec![HttpResponse {
        status: 401,
        body: json!({"error": "unauthorized"}),
    }]));
    let p = provider(transport);

    let result = p
        .generate_streaming(GenerateRequest {
            user_prompt: "x".into(),
            ..Default::default()
        })
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn streaming_handles_empty_delta() {
    // Some senders emit a delta chunk with empty content (e.g. role marker).
    let sse: Vec<String> = vec![
        r#"data: {"choices":[{"delta":{"role":"assistant"},"finish_reason":null,"index":0}]}"#.to_string(),
        chunk_with_content("hi"),
        chunk_with_finish("stop"),
        "data: [DONE]".to_string(),
    ];
    let transport = Arc::new(MockTransport::new(vec![sse_response(&sse)]));
    let p = provider(transport);

    let mut stream = p
        .generate_streaming(GenerateRequest {
            user_prompt: "x".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let mut deltas = Vec::new();
    while let Some(item) = stream.next().await {
        deltas.push(item.unwrap().delta);
    }
    // 3 SSE events: role-only (empty delta), content "hi", finish-only
    // (empty delta with finish_reason + usage).
    assert_eq!(deltas, vec!["", "hi", ""]);
}

#[tokio::test]
async fn streaming_works_with_json_mode() {
    // json_mode + streaming: each chunk carries a partial JSON object that
    // becomes valid when concatenated.
    let sse: Vec<String> = vec![
        chunk_with_content("{\"xml\":\""),
        chunk_with_content("<mxfile/>"),
        chunk_with_content("\"}"),
        chunk_with_finish("stop"),
        "data: [DONE]".to_string(),
    ];
    let transport = Arc::new(MockTransport::new(vec![sse_response(&sse)]));
    let p = provider(transport);

    let mut stream = p
        .generate_streaming(GenerateRequest {
            user_prompt: "x".into(),
            json_mode: true,
            memory: vec![],
            ..Default::default()
        })
        .await
        .unwrap();

    let mut full = String::new();
    while let Some(item) = stream.next().await {
        full.push_str(&item.unwrap().delta);
    }
    assert_eq!(full, "{\"xml\":\"<mxfile/>\"}");
}
