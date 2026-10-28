//! Integration tests for AnthropicBackend::create_chat_completion_stream.
//!
//! These tests do NOT call the real Anthropic API. They spin up a local axum
//! mock server that serves Anthropic's SSE event format on `/messages`, point
//! AnthropicBackend at it, and verify that the backend translates the Anthropic
//! event stream into OpenAI chat.completion.chunk SSE bytes.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body, extract::State, http::StatusCode, response::Response, routing::post, Json, Router,
};
use futures::StreamExt;
use openai_server::{
    backend::{AnthropicBackend, BackendProvider},
    types::{ChatCompletionRequest, ChatMessage},
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// One Anthropic SSE event: `(event_name, data_value, delay_before_emit)`.
type Event = (&'static str, Value, Duration);

/// Mock state: holds the captured outbound request body and the scripted
/// stream of Anthropic events to emit (in order, with per-event delays).
struct MockState {
    captured: Mutex<Option<Value>>,
    events: Mutex<Vec<Event>>,
}

async fn handle_messages(State(state): State<Arc<MockState>>, Json(body): Json<Value>) -> Response {
    *state.captured.lock().await = Some(body);
    let events = std::mem::take(&mut *state.events.lock().await);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::convert::Infallible>>(32);
    tokio::spawn(async move {
        for (name, data, delay) in events {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let payload = format!(
                "event: {}\ndata: {}\n\n",
                name,
                serde_json::to_string(&data).unwrap()
            );
            if tx.send(Ok(bytes::Bytes::from(payload))).await.is_err() {
                return;
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn start_mock(events: Vec<Event>) -> (String, Arc<MockState>) {
    let state = Arc::new(MockState {
        captured: Mutex::new(None),
        events: Mutex::new(events),
    });
    let app = Router::new()
        .route("/messages", post(handle_messages))
        .with_state(state.clone());

    let base = crate::support::mock::serve(app).await;
    (base, state)
}

fn user_msg(text: &str) -> ChatMessage {
    ChatMessage {
        role: "user".to_string(),
        content: Some(json!(text)),
        name: None,
        tool_calls: None,
        tool_call_id: None,
    }
}

fn streaming_request() -> ChatCompletionRequest {
    ChatCompletionRequest {
        fallbacks: Default::default(),
        model: "claude-haiku-4-5".to_string(),
        messages: vec![user_msg("hi")],
        temperature: None,
        top_p: None,
        n: None,
        stream: Some(true),
        stop: None,
        max_tokens: Some(64),
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        logit_bias: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        stream_options: None,
        parallel_tool_calls: None,
        reasoning_effort: None,
    }
}

/// Drain the backend stream and return (raw_concatenated_text, parsed_json_chunks).
async fn drain_stream(
    backend: &AnthropicBackend,
    request: ChatCompletionRequest,
) -> (String, Vec<Value>) {
    let mut stream = backend
        .create_chat_completion_stream(request)
        .await
        .expect("stream creation should succeed");

    let mut raw = String::new();
    while let Some(chunk) = stream.next().await {
        raw.push_str(&String::from_utf8_lossy(&chunk.expect("chunk ok")));
    }

    let chunks: Vec<Value> = raw
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|p| *p != "[DONE]")
        .map(|p| {
            serde_json::from_str(p)
                .unwrap_or_else(|e| panic!("each data line must be valid JSON: {e}\nline: {p}"))
        })
        .collect();

    (raw, chunks)
}

#[tokio::test]
async fn streaming_translates_text_response_to_openai_delta_chunks() {
    let events: Vec<Event> = vec![
        (
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_int_text",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-haiku-4-5",
                    "content": [],
                    "usage": {
                        "input_tokens": 9,
                        "output_tokens": 0,
                        "cache_read_input_tokens": 2,
                        "cache_creation_input_tokens": 3
                    }
                }
            }),
            Duration::ZERO,
        ),
        (
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
            Duration::ZERO,
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "Hello"}
            }),
            Duration::ZERO,
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": " world"}
            }),
            Duration::ZERO,
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
            Duration::ZERO,
        ),
        (
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"output_tokens": 4}
            }),
            Duration::ZERO,
        ),
        (
            "message_stop",
            json!({"type": "message_stop"}),
            Duration::ZERO,
        ),
    ];
    let (base_url, state) = start_mock(events).await;
    let backend =
        AnthropicBackend::with_base_url(reqwest::Client::new(), "test-key".to_string(), base_url);

    let (raw, chunks) = drain_stream(&backend, streaming_request()).await;

    // Verify outbound request asked for streaming.
    let captured = state
        .captured
        .lock()
        .await
        .clone()
        .expect("request captured");
    assert_eq!(
        captured.get("stream").and_then(|v| v.as_bool()),
        Some(true),
        "outbound Anthropic request must include stream: true"
    );

    // Stream must terminate with [DONE].
    assert!(
        raw.contains("data: [DONE]"),
        "stream must end with [DONE]:\n{raw}"
    );

    // Every chunk must be the OpenAI streaming object type.
    for c in &chunks {
        assert_eq!(c["object"], "chat.completion.chunk", "chunk = {c}");
        assert!(c.get("message").is_none(), "no top-level message field");
        assert_eq!(c["id"], "msg_int_text");
    }

    // The role must be advertised once at the start.
    let role_chunks: Vec<&Value> = chunks
        .iter()
        .filter(|c| c["choices"][0]["delta"]["role"].is_string())
        .collect();
    assert_eq!(role_chunks.len(), 1, "exactly one role delta");
    assert_eq!(role_chunks[0]["choices"][0]["delta"]["role"], "assistant");

    // Text content must arrive in two separate delta.content chunks (not glued).
    let content_pieces: Vec<String> = chunks
        .iter()
        .filter_map(|c| {
            c["choices"][0]["delta"]["content"]
                .as_str()
                .map(|s| s.to_string())
        })
        .collect();
    assert_eq!(
        content_pieces,
        vec!["Hello".to_string(), " world".to_string()],
        "each text_delta must produce its own chunk"
    );

    // The finish chunk must carry stop and the usage totals from message_delta.
    let finish = chunks
        .iter()
        .find(|c| c["choices"][0]["finish_reason"].is_string())
        .expect("must have a chunk with finish_reason");
    assert_eq!(finish["choices"][0]["finish_reason"], "stop");
    // prompt_tokens must fold in cache-read and cache-write tokens, matching
    // the non-streaming translator's accounting.
    assert_eq!(finish["usage"]["prompt_tokens"], 14);
    assert_eq!(finish["usage"]["completion_tokens"], 4);
    assert_eq!(finish["usage"]["total_tokens"], 18);
    assert_eq!(finish["usage"]["prompt_tokens_details"]["cached_tokens"], 2);
    assert_eq!(
        finish["usage"]["prompt_tokens_details"]["cache_write_tokens"],
        3
    );
}

#[tokio::test]
async fn streaming_translates_tool_use_with_streamed_input_json() {
    let events: Vec<Event> = vec![
        (
            "message_start",
            json!({
                "message": {
                    "id": "msg_int_tool",
                    "role": "assistant",
                    "model": "claude-haiku-4-5",
                    "content": [],
                    "usage": {"input_tokens": 12, "output_tokens": 0}
                }
            }),
            Duration::ZERO,
        ),
        (
            "content_block_start",
            json!({
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_42",
                    "name": "bash",
                    "input": {}
                }
            }),
            Duration::ZERO,
        ),
        (
            "content_block_delta",
            json!({
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{\"command\":"}
            }),
            Duration::ZERO,
        ),
        (
            "content_block_delta",
            json!({
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "\"ls -la\"}"}
            }),
            Duration::ZERO,
        ),
        ("content_block_stop", json!({"index": 0}), Duration::ZERO),
        (
            "message_delta",
            json!({
                "delta": {"stop_reason": "tool_use"},
                "usage": {"output_tokens": 8}
            }),
            Duration::ZERO,
        ),
        ("message_stop", json!({}), Duration::ZERO),
    ];

    let (base_url, _state) = start_mock(events).await;
    let backend =
        AnthropicBackend::with_base_url(reqwest::Client::new(), "test-key".to_string(), base_url);

    let (_raw, chunks) = drain_stream(&backend, streaming_request()).await;

    // First tool_calls chunk must declare the call (id, name, type, index, empty args).
    let mut tool_chunks = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["tool_calls"].as_array().cloned());

    let first = tool_chunks.next().expect("missing tool_use start chunk");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["index"], 0);
    assert_eq!(first[0]["id"], "toolu_42");
    assert_eq!(first[0]["type"], "function");
    assert_eq!(first[0]["function"]["name"], "bash");
    assert_eq!(first[0]["function"]["arguments"], "");

    // Subsequent argument chunks must carry partial_json verbatim, no id/name re-emit.
    let second = tool_chunks
        .next()
        .expect("missing first input_json_delta chunk");
    assert_eq!(second[0]["index"], 0);
    assert_eq!(second[0]["function"]["arguments"], "{\"command\":");
    assert!(second[0].get("id").is_none() || second[0]["id"].is_null());

    let third = tool_chunks
        .next()
        .expect("missing second input_json_delta chunk");
    assert_eq!(third[0]["function"]["arguments"], "\"ls -la\"}");

    // Concatenating all argument fragments must produce valid JSON that parses to
    // the original tool input.
    let all_args: String = chunks
        .iter()
        .filter_map(|c| {
            c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .map(|s| s.to_string())
        })
        .collect();
    let parsed: Value = serde_json::from_str(&all_args)
        .unwrap_or_else(|e| panic!("concatenated args must parse: {e}\nargs={all_args}"));
    assert_eq!(parsed, json!({"command": "ls -la"}));

    // Finish must map tool_use -> tool_calls.
    let finish = chunks
        .iter()
        .find(|c| c["choices"][0]["finish_reason"].is_string())
        .expect("missing finish chunk");
    assert_eq!(finish["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(finish["usage"]["completion_tokens"], 8);
}

#[tokio::test]
async fn streaming_chunks_arrive_incrementally_not_in_one_blob() {
    let events: Vec<Event> = vec![
        (
            "message_start",
            json!({
                "message": {
                    "id": "msg_int_inc",
                    "role": "assistant",
                    "model": "claude-haiku-4-5",
                    "content": [],
                    "usage": {"input_tokens": 1, "output_tokens": 0}
                }
            }),
            Duration::ZERO,
        ),
        (
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
            Duration::ZERO,
        ),
        (
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": "tick"}}),
            Duration::from_millis(30),
        ),
        (
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": " tock"}}),
            Duration::from_millis(30),
        ),
        ("content_block_stop", json!({"index": 0}), Duration::ZERO),
        (
            "message_delta",
            json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
            Duration::ZERO,
        ),
        ("message_stop", json!({}), Duration::ZERO),
    ];

    let (base_url, _state) = start_mock(events).await;
    let backend =
        AnthropicBackend::with_base_url(reqwest::Client::new(), "test-key".to_string(), base_url);

    let mut stream = backend
        .create_chat_completion_stream(streaming_request())
        .await
        .expect("stream creation");

    let mut timestamps: Vec<std::time::Instant> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let _ = chunk.expect("chunk ok");
        timestamps.push(std::time::Instant::now());
    }

    assert!(
        timestamps.len() >= 3,
        "expected multiple incremental chunks, got {}",
        timestamps.len()
    );

    // With ~30ms between the two text deltas, the spread between first and last
    // chunk must exceed a small floor — proving we're not buffering the entire
    // upstream response before emitting.
    let elapsed = *timestamps.last().unwrap() - *timestamps.first().unwrap();
    assert!(
        elapsed >= Duration::from_millis(40),
        "chunks should arrive incrementally; total spread was only {:?}",
        elapsed
    );
}

#[tokio::test]
async fn streaming_returns_error_for_non_2xx_response() {
    // Mock that always returns a 4xx with an Anthropic-style error body.
    async fn bad_handler() -> Response {
        let body = json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": "boom"}
        });
        Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    let app = Router::new().route("/messages", post(bad_handler));
    let base_url = crate::support::mock::serve(app).await;

    let backend =
        AnthropicBackend::with_base_url(reqwest::Client::new(), "test-key".to_string(), base_url);
    let result = backend
        .create_chat_completion_stream(streaming_request())
        .await;
    assert!(result.is_err(), "non-2xx must surface as a backend error");
    let err_msg = format!("{}", result.err().unwrap());
    assert!(
        err_msg.contains("boom"),
        "error message should include upstream message; got: {err_msg}"
    );
}
