//! End-to-end translation tests for the AnthropicBackend.
//!
//! These tests do NOT call the real Anthropic API — they spin up a local axum
//! mock server, point AnthropicBackend at it, and verify both the outbound
//! request shape (tools / system / tool_choice) and the inbound response
//! translation (tool_use blocks → OpenAI tool_calls).

use std::sync::Arc;

use axum::{routing::post, Json, Router};
use openai_server::{
    backend::{AnthropicBackend, BackendProvider},
    types::{ChatCompletionRequest, ChatMessage, FunctionCall, Tool, ToolCall, ToolFunction},
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// Shared state for the mock: holds the captured request body and the response to return.
struct MockState {
    captured: Mutex<Option<Value>>,
    response: Value,
}

async fn handle_messages(
    axum::extract::State(state): axum::extract::State<Arc<MockState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    *state.captured.lock().await = Some(body);
    Json(state.response.clone())
}

/// Boot a mock Anthropic server. Returns (base_url, captured_request_holder).
async fn start_mock(response: Value) -> (String, Arc<MockState>) {
    let state = Arc::new(MockState {
        captured: Mutex::new(None),
        response,
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

fn empty_request_template() -> ChatCompletionRequest {
    ChatCompletionRequest {
        fallbacks: Default::default(),
        model: "claude-haiku-4-5".to_string(),
        messages: vec![],
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

fn canned_text_response() -> Value {
    json!({
        "id": "msg_x",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-4-5",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1},
        "content": [{"type": "text", "text": "ok"}],
    })
}

// `outbound_request_includes_tools_in_anthropic_format`,
// `outbound_request_hoists_system_messages_into_system_field`, and
// `outbound_request_translates_tool_choice_named_function` used to live here.
// Each only added an HTTP round trip over a unit test already covering the
// same translation in isolation (src/backend/anthropic.rs::tests
// `convert_tools_maps_openai_function_to_anthropic_tool`,
// `convert_messages_hoists_system_into_top_level_system` +
// `convert_messages_concatenates_multiple_system_messages`, and
// `convert_tool_choice_maps_string_modes_and_named_function`), so they were
// deleted. The two tests below stay as the wire-level proof that
// `create_chat_completion` actually wires translation into a real HTTP
// request/response, covering both the outbound and inbound direction.

#[tokio::test]
async fn outbound_assistant_tool_call_round_trips_as_tool_use_block() {
    let (base_url, state) = start_mock(canned_text_response()).await;
    let backend =
        AnthropicBackend::with_base_url(reqwest::Client::new(), "test-key".to_string(), base_url);

    let mut req = empty_request_template();
    req.messages = vec![
        user_msg("ls"),
        ChatMessage {
            role: "assistant".to_string(),
            content: None,
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_xyz".to_string(),
                tool_type: "function".to_string(),
                function: FunctionCall {
                    name: "bash".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                },
            }]),
            tool_call_id: None,
        },
        ChatMessage {
            role: "tool".to_string(),
            content: Some(json!("file1\nfile2")),
            name: None,
            tool_calls: None,
            tool_call_id: Some("call_xyz".to_string()),
        },
    ];

    backend.create_chat_completion(req).await.expect("ok");

    let captured = state.captured.lock().await.clone().expect("captured");
    let msgs = captured["messages"].as_array().expect("messages array");
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0]["role"], "user");

    // Assistant message: an array containing one tool_use block.
    assert_eq!(msgs[1]["role"], "assistant");
    let asst_blocks = msgs[1]["content"]
        .as_array()
        .expect("assistant content must be a block array when tool_calls are present");
    assert_eq!(asst_blocks.len(), 1);
    assert_eq!(asst_blocks[0]["type"], "tool_use");
    assert_eq!(asst_blocks[0]["id"], "call_xyz");
    assert_eq!(asst_blocks[0]["name"], "bash");
    assert_eq!(asst_blocks[0]["input"], json!({"command": "ls"}));

    // Tool result: must come back as user role with a tool_result block.
    assert_eq!(msgs[2]["role"], "user");
    let result_blocks = msgs[2]["content"]
        .as_array()
        .expect("tool-result content must be a block array");
    assert_eq!(result_blocks.len(), 1);
    assert_eq!(result_blocks[0]["type"], "tool_result");
    assert_eq!(result_blocks[0]["tool_use_id"], "call_xyz");
    assert_eq!(result_blocks[0]["content"], "file1\nfile2");
}

#[tokio::test]
async fn outbound_prefers_max_completion_tokens_and_defaults_when_omitted() {
    for (max_tokens, max_completion_tokens, expected) in [
        (Some(64), Some(123), 123),
        (Some(64), None, 64),
        (None, None, 4_096),
    ] {
        let (base_url, state) = start_mock(canned_text_response()).await;
        let backend = AnthropicBackend::with_base_url(
            reqwest::Client::new(),
            "test-key".to_string(),
            base_url,
        );
        let mut request = empty_request_template();
        request.messages = vec![user_msg("Hello")];
        request.max_tokens = max_tokens;
        request.max_completion_tokens = max_completion_tokens;

        backend.create_chat_completion(request).await.expect("ok");

        let captured = state.captured.lock().await.clone().expect("captured");
        assert_eq!(captured["max_tokens"], expected);
    }
}

#[tokio::test]
async fn inbound_tool_use_response_becomes_openai_tool_calls() {
    let response = json!({
        "id": "msg_tool",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-4-5",
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 7, "output_tokens": 3},
        "content": [
            {"type": "text", "text": "I'll list the files."},
            {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": "ls"}}
        ]
    });
    let (base_url, _state) = start_mock(response).await;
    let backend =
        AnthropicBackend::with_base_url(reqwest::Client::new(), "test-key".to_string(), base_url);

    let mut req = empty_request_template();
    req.messages = vec![user_msg("ls")];
    req.tools = Some(vec![Tool {
        tool_type: "function".to_string(),
        function: ToolFunction {
            name: "bash".to_string(),
            description: None,
            parameters: Some(json!({"type": "object", "properties": {}}).into()),
        },
    }]);

    let resp = backend
        .create_chat_completion(req)
        .await
        .expect("mock chat completion");

    assert_eq!(resp.choices.len(), 1);
    let choice = &resp.choices[0];
    assert_eq!(
        choice.finish_reason.as_deref(),
        Some("tool_calls"),
        "stop_reason=tool_use must map to finish_reason=tool_calls",
    );
    let tool_calls = choice
        .message
        .tool_calls
        .as_ref()
        .expect("tool_use response must produce OpenAI-style tool_calls");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "toolu_1");
    assert_eq!(tool_calls[0].function.name, "bash");
    let parsed: Value = serde_json::from_str(&tool_calls[0].function.arguments)
        .expect("function.arguments must be a JSON string");
    assert_eq!(parsed, json!({"command": "ls"}));

    assert_eq!(
        choice.message.content,
        Some(json!("I'll list the files.")),
        "preceding text block must still surface in message.content",
    );
}

/// `reasoning_effort` becomes adaptive thinking plus `output_config.effort`,
/// `none` disables thinking, effort strings are validated upstream, and the
/// thinking block in the reply is not surfaced as content.
#[tokio::test]
async fn reasoning_effort_maps_to_adaptive_thinking_and_effort() {
    let response = json!({
        "id": "msg_think",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": [
            {"type": "thinking", "thinking": "", "signature": "sig"},
            {"type": "text", "text": "42"}
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 9}
    });
    let (base_url, state) = start_mock(response).await;
    let backend =
        AnthropicBackend::with_base_url(reqwest::Client::new(), "test-key".to_string(), base_url);

    let mut req = empty_request_template();
    req.model = "claude-opus-5-5".to_string();
    req.messages = vec![user_msg("what is 6 times 7?")];
    req.reasoning_effort = Some("medium".to_string());
    let reply = backend.create_chat_completion(req).await.expect("ok");
    assert_eq!(reply.choices[0].message.content, Some(json!("42")));
    let captured = state.captured.lock().await.clone().expect("captured");
    assert_eq!(captured["thinking"], json!({"type": "adaptive"}));
    assert_eq!(captured["output_config"], json!({"effort": "medium"}));

    // Effort shares `output_config` with structured output.
    let mut req = empty_request_template();
    req.model = "claude-sonnet-4-6".to_string();
    req.messages = vec![user_msg("answer as json")];
    req.reasoning_effort = Some("minimal".to_string());
    req.response_format = Some(
        json!({"type": "json_schema", "json_schema": {"name": "a", "schema": {"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"], "additionalProperties": false}}})
            .into(),
    );
    backend.create_chat_completion(req).await.expect("ok");
    let captured = state.captured.lock().await.clone().expect("captured");
    assert_eq!(captured["output_config"]["effort"], "low");
    assert_eq!(captured["output_config"]["format"]["type"], "json_schema");
    assert_eq!(captured["thinking"]["type"], "adaptive");

    // Explicit `none` disables thinking even on a model that defaults to it.
    let mut req = empty_request_template();
    req.model = "claude-sonnet-5".to_string();
    req.messages = vec![user_msg("hi")];
    req.reasoning_effort = Some("none".to_string());
    backend.create_chat_completion(req).await.expect("ok");
    let captured = state.captured.lock().await.clone().expect("captured");
    assert_eq!(captured["thinking"], json!({"type": "disabled"}));
    assert!(captured.get("output_config").is_none(), "{captured}");
}

#[tokio::test]
async fn reasoning_controls_preserve_defaults_and_forward_future_models_and_efforts() {
    let (base_url, state) = start_mock(canned_text_response()).await;
    let backend =
        AnthropicBackend::with_base_url(reqwest::Client::new(), "test-key".into(), base_url);
    for model in [
        "claude-3-haiku-20240307",
        "claude-future-6",
        "us.anthropic.claude-future-6-v1:0",
    ] {
        for effort in [
            None,
            Some("none"),
            Some("minimal"),
            Some("low"),
            Some("medium"),
            Some("high"),
            Some("xhigh"),
            Some("max"),
            Some("future-effort"),
        ] {
            let mut req = empty_request_template();
            req.model = model.into();
            req.messages = vec![user_msg("hi")];
            req.reasoning_effort = effort.map(str::to_owned);
            backend.create_chat_completion(req).await.unwrap();
            let captured = state.captured.lock().await.clone().unwrap();
            assert_eq!(captured["model"], model);
            match effort {
                None => {
                    assert!(captured.get("thinking").is_none());
                    assert!(captured.get("output_config").is_none());
                }
                Some("none") => {
                    assert_eq!(captured["thinking"], json!({"type": "disabled"}));
                    assert!(captured.get("output_config").is_none());
                }
                Some(effort) => {
                    assert_eq!(captured["thinking"], json!({"type": "adaptive"}));
                    assert_eq!(
                        captured["output_config"]["effort"],
                        if effort == "minimal" { "low" } else { effort }
                    );
                }
            }
        }
    }
}
