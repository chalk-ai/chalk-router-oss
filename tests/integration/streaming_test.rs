use async_trait::async_trait;
use axum::{http::StatusCode, routing::post, Json, Router};
use futures::StreamExt;
use openai_server::backend::{BackendError, BackendProvider, OpenAICompatibleBackend};
use openai_server::types::{
    ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, CreateEmbeddingRequest,
    CreateEmbeddingResponse, CreateImageRequest, CreateModerationRequest, CreateModerationResponse,
    CreateSpeechRequest, FunctionCall, ImageResponse, ListResponse, Model, ToolCall,
    TranscriptionResponse, Usage,
};
use std::time::Duration;

/// Minimal mock backend that returns a fixed non-streaming response and relies on
/// the default `create_chat_completion_stream` implementation in BackendProvider.
struct MockFallbackBackend {
    response: ChatCompletionResponse,
}

#[async_trait]
impl BackendProvider for MockFallbackBackend {
    async fn create_chat_completion(
        &self,
        _request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        Ok(self.response.clone())
    }

    async fn create_embedding(
        &self,
        _r: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_image(&self, _r: CreateImageRequest) -> Result<ImageResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_speech(&self, _r: CreateSpeechRequest) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_transcription(
        &self,
        _model: String,
        _file_bytes: bytes::Bytes,
        _file_name: String,
        _language: Option<String>,
        _prompt: Option<String>,
        _response_format: Option<String>,
        _temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_translation(
        &self,
        _model: String,
        _file_bytes: bytes::Bytes,
        _file_name: String,
        _prompt: Option<String>,
        _response_format: Option<String>,
        _temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_moderation(
        &self,
        _r: CreateModerationRequest,
    ) -> Result<CreateModerationResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn get_model(&self, _model_id: &str) -> Result<Model, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    fn name(&self) -> &str {
        "mock-fallback"
    }
}

/// Spin up a mock OpenAI-compatible server that returns real SSE chunks with delays
/// between them, so we can verify streaming passthrough works (i.e. we receive chunks
/// incrementally rather than all at once after the response completes).
async fn start_mock_openai_server() -> String {
    let app = Router::new().route("/v1/chat/completions", post(mock_streaming_handler));

    let base = crate::support::mock::serve(app).await;
    format!("{base}/v1")
}

/// Mock handler that streams SSE chunks with artificial delays.
async fn mock_streaming_handler(Json(req): Json<serde_json::Value>) -> axum::response::Response {
    let stream_requested = req.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    if !stream_requested {
        // Non-streaming: return a regular JSON response
        let body = serde_json::json!({
            "id": "chatcmpl-test-123",
            "object": "chat.completion",
            "created": 1234567890_i64,
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello from mock!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        });
        return (StatusCode::OK, Json(body)).into_response();
    }

    // Streaming: send multiple SSE chunks with delays
    let chunks = vec![
        // Chunk 1: role
        r#"data: {"id":"chatcmpl-test-123","object":"chat.completion.chunk","created":1234567890,"model":"mock-model","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
        // Chunk 2: first content token
        r#"data: {"id":"chatcmpl-test-123","object":"chat.completion.chunk","created":1234567890,"model":"mock-model","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
        // Chunk 3: second content token
        r#"data: {"id":"chatcmpl-test-123","object":"chat.completion.chunk","created":1234567890,"model":"mock-model","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#,
        // Chunk 4: third content token
        r#"data: {"id":"chatcmpl-test-123","object":"chat.completion.chunk","created":1234567890,"model":"mock-model","choices":[{"index":0,"delta":{"content":"!"},"finish_reason":null}]}"#,
        // Chunk 5: finish with usage
        r#"data: {"id":"chatcmpl-test-123","object":"chat.completion.chunk","created":1234567890,"model":"mock-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}"#,
        // Done marker
        "data: [DONE]",
    ];

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::convert::Infallible>>(16);

    tokio::spawn(async move {
        for chunk in chunks {
            // Small delay between chunks to simulate real streaming. Kept modest
            // (was 50ms) so the test proves incremental delivery without a long
            // real-time wait; see the elapsed-floor assertion below.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = tx.send(Ok(format!("{chunk}\n\n"))).await;
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = axum::body::Body::from_stream(stream);

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .unwrap()
}

use axum::response::IntoResponse;

#[tokio::test]
async fn test_streaming_passthrough_receives_incremental_chunks() {
    let base_url = start_mock_openai_server().await;

    let backend = OpenAICompatibleBackend::with_base_url(
        reqwest::Client::new(),
        "test-key".to_string(),
        base_url,
    );

    let request = ChatCompletionRequest {
        fallbacks: Default::default(),
        model: "mock-model".to_string(),
        messages: vec![openai_server::types::ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!("Hello")),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: None,
        top_p: None,
        n: None,
        stream: Some(true),
        stop: None,
        max_tokens: None,
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        logit_bias: None,
        stream_options: None,
        parallel_tool_calls: None,
        reasoning_effort: None,
    };

    let mut stream = backend
        .create_chat_completion_stream(request)
        .await
        .expect("stream creation should succeed");

    // Collect all chunks with timestamps to verify incremental delivery
    let mut received_chunks: Vec<(std::time::Instant, String)> = Vec::new();

    while let Some(result) = stream.next().await {
        let bytes = result.expect("chunk should be Ok");
        let text = String::from_utf8_lossy(&bytes).to_string();
        received_chunks.push((std::time::Instant::now(), text));
    }

    // We should have received multiple chunks (not one giant blob)
    assert!(
        received_chunks.len() >= 2,
        "expected multiple incremental chunks, got {}",
        received_chunks.len()
    );

    // Concatenate all chunks and verify content
    let full_response: String = received_chunks.iter().map(|(_, s)| s.as_str()).collect();
    assert!(
        full_response.contains("Hello"),
        "response should contain 'Hello'"
    );
    assert!(
        full_response.contains(" world"),
        "response should contain ' world'"
    );
    assert!(
        full_response.contains("[DONE]"),
        "response should contain [DONE] marker"
    );

    // Verify the chunks were received over time (not all at once). With 20ms
    // between 6 chunks the first→last spread is ~100ms; a 50ms floor proves
    // incremental delivery with comfortable margin above scheduler jitter.
    let first_time = received_chunks.first().unwrap().0;
    let last_time = received_chunks.last().unwrap().0;
    let elapsed = last_time - first_time;
    assert!(
        elapsed >= Duration::from_millis(50),
        "chunks should arrive incrementally over time, but total elapsed was only {:?}",
        elapsed
    );
}

#[tokio::test]
async fn test_non_streaming_still_works() {
    let base_url = start_mock_openai_server().await;

    let backend = OpenAICompatibleBackend::with_base_url(
        reqwest::Client::new(),
        "test-key".to_string(),
        base_url,
    );

    let request = ChatCompletionRequest {
        fallbacks: Default::default(),
        model: "mock-model".to_string(),
        messages: vec![openai_server::types::ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!("Hello")),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: None,
        top_p: None,
        n: None,
        stream: Some(false),
        stop: None,
        max_tokens: None,
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        logit_bias: None,
        stream_options: None,
        parallel_tool_calls: None,
        reasoning_effort: None,
    };

    let response = backend
        .create_chat_completion(request)
        .await
        .expect("non-streaming request should succeed");

    assert_eq!(response.id, "chatcmpl-test-123");
    assert_eq!(response.choices.len(), 1);
    assert_eq!(response.choices[0].finish_reason.as_deref(), Some("stop"));
    assert!(response.usage.is_some());
    assert_eq!(response.usage.unwrap().total_tokens, 15);
}

#[tokio::test]
async fn test_streaming_error_returns_error() {
    // Point to a server that doesn't exist
    let backend = OpenAICompatibleBackend::with_base_url(
        reqwest::Client::new(),
        "test-key".to_string(),
        "http://127.0.0.1:1".to_string(), // port 1 should fail to connect
    );

    let request = ChatCompletionRequest {
        fallbacks: Default::default(),
        model: "mock-model".to_string(),
        messages: vec![openai_server::types::ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!("Hello")),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: None,
        top_p: None,
        n: None,
        stream: Some(true),
        stop: None,
        max_tokens: None,
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        logit_bias: None,
        stream_options: None,
        parallel_tool_calls: None,
        reasoning_effort: None,
    };

    let result = backend.create_chat_completion_stream(request).await;
    match result {
        Err(BackendError::RequestError(_)) => {}
        Err(other) => panic!("expected RequestError for an unreachable server, got {other:?}"),
        Ok(_) => panic!("expected an error for an unreachable server, got Ok"),
    }
}

#[tokio::test]
async fn test_streaming_passthrough_preserves_sse_format() {
    let base_url = start_mock_openai_server().await;

    let backend = OpenAICompatibleBackend::with_base_url(
        reqwest::Client::new(),
        "test-key".to_string(),
        base_url,
    );

    let request = ChatCompletionRequest {
        fallbacks: Default::default(),
        model: "mock-model".to_string(),
        messages: vec![openai_server::types::ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!("Hello")),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: None,
        top_p: None,
        n: None,
        stream: Some(true),
        stop: None,
        max_tokens: None,
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        logit_bias: None,
        stream_options: None,
        parallel_tool_calls: None,
        reasoning_effort: None,
    };

    let mut stream = backend
        .create_chat_completion_stream(request)
        .await
        .expect("stream creation should succeed");

    let mut full_response = String::new();
    while let Some(result) = stream.next().await {
        let bytes = result.expect("chunk should be Ok");
        full_response.push_str(&String::from_utf8_lossy(&bytes));
    }

    // Every data line should be properly formatted SSE
    let lines: Vec<&str> = full_response.lines().filter(|l| !l.is_empty()).collect();
    for line in &lines {
        assert!(
            line.starts_with("data: "),
            "SSE line should start with 'data: ', got: {}",
            line
        );
    }

    // Should contain parseable JSON chunks
    let data_payloads: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|p| *p != "[DONE]")
        .collect();

    assert!(
        !data_payloads.is_empty(),
        "should have at least one JSON chunk"
    );

    for payload in &data_payloads {
        let parsed: serde_json::Value = serde_json::from_str(payload)
            .unwrap_or_else(|e| panic!("failed to parse SSE chunk as JSON: {e}\nchunk: {payload}"));
        assert_eq!(parsed["object"], "chat.completion.chunk");
    }
}

// Helper that builds a fixed ChatCompletionResponse for fallback tests.
fn make_fixed_response(content: &str) -> ChatCompletionResponse {
    ChatCompletionResponse {
        id: "chatcmpl-fallback-test".to_string(),
        object: "chat.completion".to_string(),
        created: 1_700_000_000,
        model: "test-model".to_string(),
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::String(content.to_string())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: Some(Usage {
            prompt_tokens: 8,
            completion_tokens: Some(5),
            total_tokens: 13,
            ..Default::default()
        }),
    }
}

/// RED → GREEN: the old default serialised the full `message` object into one SSE event,
/// so clients saw `choices[0].message.content` instead of `choices[0].delta.content`.
/// After the fix the stream must use `object: "chat.completion.chunk"` and carry the
/// text in `delta.content`, not `message.content`.
#[tokio::test]
async fn test_fallback_stream_emits_delta_chunks_not_full_message() {
    let backend = MockFallbackBackend {
        response: make_fixed_response("Hello from fallback!"),
    };

    let request = ChatCompletionRequest {
        fallbacks: Default::default(),
        model: "test-model".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!("hi")),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: None,
        top_p: None,
        n: None,
        stream: Some(true),
        stop: None,
        max_tokens: None,
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        logit_bias: None,
        stream_options: None,
        parallel_tool_calls: None,
        reasoning_effort: None,
    };

    let mut stream = backend
        .create_chat_completion_stream(request)
        .await
        .expect("stream should succeed");

    let mut raw = String::new();
    while let Some(chunk) = stream.next().await {
        raw.push_str(&String::from_utf8_lossy(&chunk.expect("chunk ok")));
    }

    // Must end with [DONE]
    assert!(
        raw.contains("data: [DONE]"),
        "stream must end with [DONE]\n{raw}"
    );

    let json_chunks: Vec<serde_json::Value> = raw
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|p| *p != "[DONE]")
        .map(|p| serde_json::from_str(p).expect("each data line must be valid JSON"))
        .collect();

    assert!(!json_chunks.is_empty(), "must have at least one JSON chunk");

    // Every chunk must use the streaming object type, not the completion type.
    for chunk in &json_chunks {
        assert_eq!(
            chunk["object"], "chat.completion.chunk",
            "expected 'chat.completion.chunk', got: {}",
            chunk["object"]
        );
        // Must NOT have a top-level `message` field (that was the bug).
        assert!(
            chunk.get("message").is_none(),
            "chunk must not carry a top-level 'message' field (old buggy format)\n{chunk}"
        );
    }

    // The response text must appear in a delta.content field somewhere.
    let has_delta_content = json_chunks.iter().any(|c| {
        c["choices"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|ch| ch["delta"]["content"].as_str())
            .is_some_and(|s| s.contains("Hello from fallback!"))
    });
    assert!(
        has_delta_content,
        "response text must appear in delta.content, not message.content\n{raw}"
    );

    // Must NOT carry the text inside message.content (the old buggy path).
    let has_message_content = json_chunks.iter().any(|c| {
        c["choices"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|ch| ch["message"]["content"].as_str())
            .is_some()
    });
    assert!(
        !has_message_content,
        "response text must NOT appear in message.content (that is the old bug)\n{raw}"
    );
}

/// Companion test: a backend that has NO native streaming support still returns
/// usage information in the final finish chunk.
#[tokio::test]
async fn test_fallback_stream_includes_usage_in_finish_chunk() {
    let backend = MockFallbackBackend {
        response: make_fixed_response("usage test"),
    };

    let request = ChatCompletionRequest {
        fallbacks: Default::default(),
        model: "test-model".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!("hi")),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: None,
        top_p: None,
        n: None,
        stream: Some(true),
        stop: None,
        max_tokens: None,
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        logit_bias: None,
        stream_options: None,
        parallel_tool_calls: None,
        reasoning_effort: None,
    };

    let mut stream = backend
        .create_chat_completion_stream(request)
        .await
        .expect("stream should succeed");

    let mut raw = String::new();
    while let Some(chunk) = stream.next().await {
        raw.push_str(&String::from_utf8_lossy(&chunk.expect("chunk ok")));
    }

    let json_chunks: Vec<serde_json::Value> = raw
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|p| *p != "[DONE]")
        .map(|p| serde_json::from_str(p).expect("valid JSON"))
        .collect();

    // The chunk that carries finish_reason should also carry usage.
    let finish_chunk = json_chunks.iter().find(|c| {
        c["choices"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|ch| ch["finish_reason"].as_str())
            .is_some()
    });

    let finish_chunk = finish_chunk.expect("must have a chunk with finish_reason");
    assert_eq!(
        finish_chunk["usage"]["total_tokens"], 13,
        "usage.total_tokens must match"
    );
    assert_eq!(finish_chunk["usage"]["prompt_tokens"], 8);
}

/// RED → GREEN: Anthropic-style backends fall back to the default
/// `create_chat_completion_stream`, which used to drop `tool_calls` entirely
/// (only emitting `delta.content`). After the fix, the stream must also emit
/// a `delta.tool_calls` chunk so OpenAI-compatible clients (opencode, the
/// AI-SDK openai-compatible provider, etc.) actually see the tool call.
#[tokio::test]
async fn test_fallback_stream_emits_tool_calls_delta() {
    let response = ChatCompletionResponse {
        id: "chatcmpl-tool-fallback".to_string(),
        object: "chat.completion".to_string(),
        created: 1_700_000_000,
        model: "test-model".to_string(),
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_string(),
                content: None,
                name: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_abc".to_string(),
                    tool_type: "function".to_string(),
                    function: FunctionCall {
                        name: "bash".to_string(),
                        arguments: r#"{"command":"ls"}"#.to_string(),
                    },
                }]),
                tool_call_id: None,
            },
            finish_reason: Some("tool_calls".to_string()),
        }],
        usage: Some(Usage {
            prompt_tokens: 4,
            completion_tokens: Some(6),
            total_tokens: 10,
            ..Default::default()
        }),
    };
    let backend = MockFallbackBackend { response };

    let request = ChatCompletionRequest {
        fallbacks: Default::default(),
        model: "test-model".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!("ls")),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: None,
        top_p: None,
        n: None,
        stream: Some(true),
        stop: None,
        max_tokens: None,
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        logit_bias: None,
        stream_options: None,
        parallel_tool_calls: None,
        reasoning_effort: None,
    };

    let mut stream = backend
        .create_chat_completion_stream(request)
        .await
        .expect("stream should succeed");

    let mut raw = String::new();
    while let Some(chunk) = stream.next().await {
        raw.push_str(&String::from_utf8_lossy(&chunk.expect("chunk ok")));
    }

    let json_chunks: Vec<serde_json::Value> = raw
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|p| *p != "[DONE]")
        .map(|p| serde_json::from_str(p).expect("valid JSON"))
        .collect();

    // Must have a chunk that carries tool_calls in delta.
    let tc_chunk = json_chunks.iter().find_map(|c| {
        c["choices"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|ch| ch["delta"]["tool_calls"].as_array())
            .cloned()
    });
    let tool_calls = tc_chunk.expect(
        "fallback stream must emit a chunk with delta.tool_calls when message has tool_calls",
    );
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["id"], "call_abc");
    assert_eq!(tool_calls[0]["type"], "function");
    assert_eq!(tool_calls[0]["function"]["name"], "bash");
    assert_eq!(
        tool_calls[0]["function"]["arguments"],
        r#"{"command":"ls"}"#
    );
    // Per the OpenAI streaming spec, each tool_call delta must carry an `index`.
    assert_eq!(tool_calls[0]["index"], 0);

    // The finish chunk must still carry tool_calls as the finish_reason.
    let finish_reasons: Vec<&str> = json_chunks
        .iter()
        .filter_map(|c| {
            c["choices"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(|ch| ch["finish_reason"].as_str())
        })
        .collect();
    assert!(
        finish_reasons.contains(&"tool_calls"),
        "expected a chunk with finish_reason=tool_calls, got {finish_reasons:?}"
    );
}
