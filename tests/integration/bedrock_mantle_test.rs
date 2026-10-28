//! Bedrock Mantle routes Anthropic-family models through its Anthropic
//! Messages API even when callers use OpenAI Chat Completions.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use bytes::Bytes;
use openai_server::{
    adapters::anthropic_chat::translate_messages,
    adapters::responses_chat::create_response_via_chat,
    backend::{bedrock_backend, BackendProvider},
    responses::ResponsesRequest,
    types::{ChatCompletionRequest, ChatMessage},
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::support::mock::serve;

struct MockState {
    anthropic_request: Mutex<Option<Value>>,
    anthropic_headers: Mutex<Option<HeaderMap>>,
}

async fn messages(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Json<Value> {
    *state.anthropic_request.lock().await = Some(request);
    *state.anthropic_headers.lock().await = Some(headers);
    Json(json!({
        "id": "msg_bedrock",
        "type": "message",
        "role": "assistant",
        "model": "anthropic.claude-sonnet-5",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 1},
        "content": [{"type": "text", "text": "OK"}],
    }))
}

async fn chat_completions() -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Anthropic models must not be sent to Mantle chat completions",
    )
        .into_response()
}

async fn compatible_chat_completions() -> Json<Value> {
    Json(json!({
        "id": "chat_bedrock",
        "object": "chat.completion",
        "created": 1,
        "model": "google.gemma-3-4b-it",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "OK"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4},
    }))
}

fn request() -> ChatCompletionRequest {
    ChatCompletionRequest {
        fallbacks: Default::default(),
        model: "anthropic.claude-sonnet-5".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(json!("Reply with exactly: OK")),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: None,
        top_p: None,
        n: None,
        stream: Some(false),
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

#[tokio::test]
async fn anthropic_model_chat_completions_use_mantle_messages() {
    let state = Arc::new(MockState {
        anthropic_request: Mutex::new(None),
        anthropic_headers: Mutex::new(None),
    });
    let base = serve(
        Router::new()
            .route("/anthropic/v1/messages", post(messages))
            .route("/v1/chat/completions", post(chat_completions))
            .with_state(state.clone()),
    )
    .await;
    let backend = bedrock_backend(
        reqwest::Client::new(),
        "bedrock-key".to_string(),
        None,
        Some(&format!("{base}/v1")),
    );

    let response = backend
        .create_chat_completion(request())
        .await
        .expect("Anthropic model should be served through Messages");

    assert_eq!(response.model, "anthropic.claude-sonnet-5");
    assert_eq!(response.choices[0].message.content, Some(json!("OK")));
    assert_eq!(
        state
            .anthropic_request
            .lock()
            .await
            .as_ref()
            .expect("Messages request")["max_tokens"],
        64
    );
    let headers = state
        .anthropic_headers
        .lock()
        .await
        .clone()
        .expect("Messages headers");
    assert_eq!(headers["x-api-key"], "bedrock-key");
    assert_eq!(headers["anthropic-version"], "2023-06-01");
}

#[tokio::test]
async fn anthropic_model_responses_use_chat_then_mantle_messages() {
    let state = Arc::new(MockState {
        anthropic_request: Mutex::new(None),
        anthropic_headers: Mutex::new(None),
    });
    let base = serve(
        Router::new()
            .route("/anthropic/v1/messages", post(messages))
            // Deliberately omit `/v1/responses`: the compatibility adapter
            // must select Chat before the backend selects Mantle Messages.
            .with_state(state.clone()),
    )
    .await;
    let backend = bedrock_backend(
        reqwest::Client::new(),
        "bedrock-key".to_string(),
        None,
        Some(&format!("{base}/v1")),
    );
    let request: ResponsesRequest = serde_json::from_value(json!({
        "model": "anthropic.claude-sonnet-5",
        "input": "Reply with exactly: OK",
        "max_output_tokens": 64,
    }))
    .expect("valid compatibility Responses request");

    let response = create_response_via_chat(&backend, request)
        .await
        .expect("Responses should be served through Chat and Messages");

    assert_eq!(response.body["model"], "anthropic.claude-sonnet-5");
    assert_eq!(
        state
            .anthropic_request
            .lock()
            .await
            .as_ref()
            .expect("Messages request")["max_tokens"],
        64
    );
}

#[tokio::test]
async fn non_anthropic_messages_and_responses_use_mantle_chat_completions() {
    let base =
        serve(Router::new().route("/v1/chat/completions", post(compatible_chat_completions))).await;
    let backend = bedrock_backend(
        reqwest::Client::new(),
        "bedrock-key".to_string(),
        None,
        Some(&format!("{base}/v1")),
    );
    let request = Bytes::from(
        serde_json::to_vec(&json!({
            "model": "google.gemma-3-4b-it",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "Reply with exactly: OK"}],
        }))
        .expect("serializable Messages request"),
    );

    let response = translate_messages(&backend, &request)
        .await
        .expect("Messages should be served through Chat Completions");

    assert_eq!(response.value["model"], "google.gemma-3-4b-it");
    assert_eq!(response.value["content"][0]["text"], "OK");

    let request: ResponsesRequest = serde_json::from_value(json!({
        "model": "google.gemma-3-4b-it",
        "input": "Reply with exactly: OK",
        "max_output_tokens": 64,
    }))
    .expect("valid compatibility Responses request");
    let response = create_response_via_chat(&backend, request)
        .await
        .expect("Responses should be served through Chat Completions");
    assert_eq!(response.body["model"], "google.gemma-3-4b-it");
}
