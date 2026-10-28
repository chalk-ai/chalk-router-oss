use super::*;
use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use std::sync::{Arc, Mutex};

fn request() -> ChatCompletionRequest {
    serde_json::from_str(
        r#"{"model":"us.openai.gpt-5.6-luna","messages":[{"role":"user","content":"hello"}]}"#,
    )
    .unwrap()
}

#[test]
fn converse_rejects_modeled_controls_before_serializing_request() {
    for options in [
        json!({"n":2}),
        json!({"presence_penalty":0.5}),
        json!({"frequency_penalty":-0.5}),
        json!({"logit_bias":{"1":10}}),
        json!({"response_format":{"type":"json_object"}}),
        json!({"tool_choice":"required"}),
        json!({"tools":[{"type":"function","function":{"name":"lookup"}}],"parallel_tool_calls":false}),
    ] {
        let mut body = json!({"model":"test","messages":[{"role":"user","content":"hello"}]});
        body.as_object_mut()
            .unwrap()
            .extend(options.as_object().unwrap().clone());
        let request = serde_json::from_slice(&serde_json::to_vec(&body).unwrap()).unwrap();
        let error = BedrockRuntimeBackend::converse_body(&request)
            .err()
            .unwrap();
        assert!(matches!(error, BackendError::ApiError { status: 400, .. }));
    }
}

#[test]
fn converse_noop_defaults_omit_empty_tools_and_keep_inference_controls() {
    let request = serde_json::from_str(r#"{"model":"test","messages":[{"role":"user","content":"hello"}],"n":1,"presence_penalty":0,"frequency_penalty":0,"logit_bias":{},"user":"","tools":[],"tool_choice":"none","parallel_tool_calls":false,"response_format":{"type":"text"},"max_completion_tokens":17,"temperature":0.3,"top_p":0.8,"stop":["END"]}"#).unwrap();
    let body =
        serde_json::to_value(BedrockRuntimeBackend::converse_body(&request).unwrap()).unwrap();
    assert!(body.get("toolConfig").is_none());
    assert_eq!(body["inferenceConfig"]["maxTokens"], 17);
    assert_eq!(body["inferenceConfig"]["stopSequences"], json!(["END"]));
}

#[test]
fn runtime_classifies_wire_protocol_and_rejects_ambiguous_paths() {
    for (path, openai) in [
        ("", false),
        ("/", false),
        ("/openai/v1", true),
        ("/openai/v1/", true),
    ] {
        let url = format!("https://bedrock-runtime.us-east-1.amazonaws.com{path}");
        let backend =
            BedrockRuntimeBackend::with_api_key(reqwest::Client::new(), "stored-key", Some(&url))
                .unwrap();
        assert_eq!(backend.openai.is_some(), openai, "{url}");
        assert!(matches!(backend.auth, RuntimeAuth::Bearer(_)));
    }
    for path in ["/v1", "/converse", "/openai/v1?x=1", "/openai/v1#fragment"] {
        let url = format!("https://bedrock-runtime.us-east-1.amazonaws.com{path}");
        assert!(runtime_endpoint(Some(&url)).is_err(), "{url}");
    }
    assert!(runtime_endpoint(Some(
        "http://bedrock-runtime.us-east-1.amazonaws.com/openai/v1"
    ))
    .is_err());
    assert!(
        runtime_endpoint(Some("http://custom-proxy/tenant/openai/v1"))
            .unwrap()
            .is_none()
    );
    for key in ["", crate::provider_connections::KEYLESS_PLACEHOLDER] {
        let backend = BedrockRuntimeBackend::with_api_key(
            reqwest::Client::new(),
            key,
            Some("https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1"),
        )
        .unwrap();
        assert!(matches!(backend.auth, RuntimeAuth::Sigv4(_)));
    }
}

#[test]
fn malformed_converse_successes_never_become_empty_completions() {
    for value in [
        json!({}),
        json!({"message":"not found"}),
        json!({"error":{"message":"denied"}}),
        json!({"choices":[]}),
        json!({"output":{"message":{"role":"assistant","content":[]}},"stopReason":"end_turn"}),
        json!({"output":{"message":{"role":"assistant","content":[]}},"stopReason":"end_turn","usage":{"inputTokens":-1,"outputTokens":0}}),
    ] {
        let error = BedrockRuntimeBackend::convert_response("model".into(), value).unwrap_err();
        assert!(matches!(error, BackendError::ApiError { status: 502, .. }));
    }
}

#[test]
fn valid_converse_tools_guardrails_and_future_blocks_are_accepted() {
    let completion = BedrockRuntimeBackend::convert_response("model".into(), json!({
        "output":{"message":{"role":"assistant","content":[{"toolUse":{"toolUseId":"call-1","name":"lookup","input":{"q":"hello"}}}]}},
        "stopReason":"tool_use","usage":{"inputTokens":12,"outputTokens":4}})).unwrap();
    assert_eq!(
        completion.choices[0].finish_reason.as_deref(),
        Some("tool_calls")
    );
    assert_eq!(
        completion.choices[0].message.tool_calls.as_ref().unwrap()[0].id,
        "call-1"
    );
    assert_eq!(completion.usage.unwrap().total_tokens, 16);
    let filtered = BedrockRuntimeBackend::convert_response(
        "model".into(),
        json!({"stopReason":"guardrail_intervened","usage":{"inputTokens":1,"outputTokens":0}}),
    )
    .unwrap();
    assert_eq!(
        filtered.choices[0].finish_reason.as_deref(),
        Some("content_filter")
    );
    BedrockRuntimeBackend::convert_response("model".into(), json!({"output":{"message":{"role":"assistant","content":[{"futureBlock":{"data":"opaque"}}]}},"stopReason":"new_stop_reason","usage":{"inputTokens":1,"outputTokens":0}})).unwrap();
}

async fn fixture(
    status: StatusCode,
    mime: &'static str,
    body: &'static str,
) -> (
    String,
    Arc<Mutex<Vec<(String, Bytes)>>>,
    tokio::task::JoinHandle<()>,
) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    let app = Router::new().fallback(post(move |headers: HeaderMap, bytes: Bytes| {
        observed.lock().unwrap().push((
            headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string(),
            bytes,
        ));
        async move { (status, [("content-type", mime)], body).into_response() }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, calls, task)
}

#[tokio::test]
async fn converse_guards_controls_without_dispatching_in_both_modes() {
    let (url, calls, task) = fixture(
        StatusCode::FORBIDDEN,
        "application/json",
        r#"{"message":"must never be reached"}"#,
    )
    .await;
    let mut backend = BedrockRuntimeBackend::with_api_key(
        reqwest::Client::new(),
        "stored-key",
        Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
    )
    .unwrap();
    backend.base_url = url;
    let mut request = request();
    request.n = Some(2);
    let error = backend
        .create_native_chat_completion(
            crate::native_chat::NativeChatRequest::from_typed(request.clone()).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, BackendError::ApiError { status: 400, .. }));
    let error = match backend
        .create_native_chat_completion_stream(
            crate::native_chat::NativeChatRequest::from_typed(request).unwrap(),
        )
        .await
    {
        Ok(_) => panic!("unrepresentable stream was accepted"),
        Err(error) => error,
    };
    assert!(matches!(error, BackendError::ApiError { status: 400, .. }));
    assert!(calls.lock().unwrap().is_empty());
    task.abort();
}

#[tokio::test]
async fn converse_uses_connection_bearer_and_never_retries_rejected_key() {
    let (url, calls, task) = fixture(
        StatusCode::FORBIDDEN,
        "application/json",
        r#"{"message":"Marketplace subscription required"}"#,
    )
    .await;
    let mut backend = BedrockRuntimeBackend::with_api_key(
        reqwest::Client::new(),
        "account-specific-key",
        Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
    )
    .unwrap();
    backend.base_url = url;
    let error = backend.create_chat_completion(request()).await.unwrap_err();
    assert!(matches!(error, BackendError::ApiError { status: 403, .. }));
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "Bearer account-specific-key");
    let body: Value = serde_json::from_slice(&calls[0].1).unwrap();
    assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
    task.abort();
}

#[tokio::test]
async fn converse_stream_rejects_openai_sse_before_opening_stream() {
    let (url, _, task) = fixture(StatusCode::OK, "text/event-stream", "data: [DONE]\n\n").await;
    let mut backend = BedrockRuntimeBackend::with_api_key(
        reqwest::Client::new(),
        "stored-key",
        Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
    )
    .unwrap();
    backend.base_url = url;
    let error = match backend.create_chat_completion_stream(request()).await {
        Err(error) => error,
        Ok(_) => panic!("wrong wire format accepted"),
    };
    assert!(matches!(error, BackendError::ApiError { status: 502, .. }));
    task.abort();
}

#[tokio::test]
async fn malformed_converse_http_200_is_bad_gateway() {
    for body in [
        r#"{"message":"does not exist"}"#,
        "<html>proxy error</html>",
    ] {
        let (url, _, task) = fixture(StatusCode::OK, "application/json", body).await;
        let mut backend = BedrockRuntimeBackend::with_api_key(
            reqwest::Client::new(),
            "stored-key",
            Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
        )
        .unwrap();
        backend.base_url = url;
        assert!(matches!(
            backend.create_chat_completion(request()).await.unwrap_err(),
            BackendError::ApiError { status: 502, .. }
        ));
        task.abort();
    }
}

#[tokio::test]
async fn runtime_openai_forwards_native_options_and_returns_real_content() {
    let body = r#"{"id":"real-upstream","object":"chat.completion","created":1,"model":"us.openai.gpt-5.6-luna","service_tier":"default","choices":[{"index":0,"message":{"role":"assistant","content":"OK","refusal":null},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":4,"total_tokens":15}}"#;
    let (url, calls, task) = fixture(StatusCode::OK, "application/json", body).await;
    let client = reqwest::Client::new();
    let mut backend = BedrockRuntimeBackend::with_api_key(
        client.clone(),
        "account-specific-key",
        Some("https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1"),
    )
    .unwrap();
    assert!(backend.supports_native_chat());
    // Classification is tested against the actual AWS URL; the transport is
    // exercised against a credential-free local recorder, never a provider.
    backend.openai = Some(OpenAICompatibleBackend::for_provider(
        client,
        "bedrock",
        "account-specific-key".into(),
        format!("{url}/openai/v1"),
    ));
    let request = serde_json::from_str(r#"{"model":"us.openai.gpt-5.6-luna","messages":[{"role":"user","content":"hello"}],"reasoning_effort":"none","future_option":{"z":1,"a":2}}"#).unwrap();
    let result = backend
        .create_native_chat_completion(request)
        .await
        .unwrap();
    assert_eq!(result.body.get(), body);
    assert_eq!(
        result.completion.choices[0].message.content,
        Some(json!("OK"))
    );
    assert_eq!(result.completion.usage.unwrap().total_tokens, 15);
    let calls = calls.lock().unwrap();
    assert_eq!(calls[0].0, "Bearer account-specific-key");
    let wire = std::str::from_utf8(&calls[0].1).unwrap();
    assert!(wire.contains(r#""reasoning_effort":"none""#));
    assert!(wire.contains(r#""future_option":{"z":1,"a":2}"#));
    task.abort();
}
