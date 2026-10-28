use super::{
    bedrock_backend, is_mantle_closed_gpt_model, mantle_backend_with_urls,
    mantle_closed_gpt_base_url, uses_mantle_openai_dialect, BackendProvider,
};
use crate::{
    adapters::{
        anthropic_chat::translate_messages,
        responses_chat::{create_response_stream_via_chat, create_response_via_chat},
    },
    responses::ResponsesRequest,
    types::ChatCompletionRequest,
};
use axum::{
    body::Bytes,
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::TryStreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

#[path = "../../../tests/integration/support/mock.rs"]
mod mock;

#[test]
fn closed_gpt_dialect_is_limited_to_documented_models() {
    for model in [
        "openai.gpt-5.4",
        "openai.gpt-5.5",
        "openai.gpt-5.6-sol",
        "openai.gpt-5.6-terra",
        "openai.gpt-5.6-luna",
        "openai.gpt-6-sol",
        "openai.gpt-6-luna",
        "openai.gpt-6-astra",
        "openai.gpt-6.1-sol",
        "openai.gpt-5.4-2026-03-05",
        "openai.gpt-5.5-2026-04-23",
    ] {
        assert!(is_mantle_closed_gpt_model(model), "{model}");
        assert!(uses_mantle_openai_dialect(model), "{model}");
    }
    for model in ["google.gemma-4-31b", "google.gemma-4-e2b", "xai.grok-4.3"] {
        assert!(!is_mantle_closed_gpt_model(model), "{model}");
        assert!(uses_mantle_openai_dialect(model), "{model}");
    }
    for model in [
        "openai.gpt-oss-120b",
        "openai.gpt-oss-20b-1:0",
        "openai.gpt-oss-safeguard-120b",
        "openai.gpt-6-future",
        "us.openai.gpt-5.6-luna",
        "google.gemma-3-4b-it",
        "anthropic.claude-sonnet-5",
        "openai.gpt-5.5-preview",
        "openai.gpt-5.5-2026-04",
        // Non-ASCII ids must not panic on the snapshot-suffix split.
        "ééééé-2026-04-2",
        "openai.gpt-5.5-2026-04-2é",
        "é",
        "-2026-04-23",
    ] {
        assert!(!is_mantle_closed_gpt_model(model), "{model}");
        assert!(!uses_mantle_openai_dialect(model), "{model}");
    }
}

#[test]
fn aws_mantle_infers_closed_gpt_path_without_changing_explicit_proxy_paths() {
    for region in ["us-east-1", "us-east-2", "us-west-2", "eu-west-1"] {
        assert_eq!(
            mantle_closed_gpt_base_url(&format!("https://bedrock-mantle.{region}.api.aws/v1")),
            Some(format!("https://bedrock-mantle.{region}.api.aws/openai/v1"))
        );
        let backend = bedrock_backend(reqwest::Client::new(), "key".into(), Some(region), None);
        assert!(backend.closed_gpt.is_some());
    }
    for base in [
        "https://bedrock-mantle.us-east-1.api.aws/openai/v1",
        "https://bedrock-mantle.us-east-1.api.aws/custom/v1",
        "https://bedrock-mantle.us-east-1.api.aws/v1?project=custom",
        "https://bedrock-mantle.us-east-1.api.aws/v1#custom",
        "https://bedrock-mantle.us-east-1.api.aws:8443/v1",
        "https://user@bedrock-mantle.us-east-1.api.aws/v1",
        "http://bedrock-mantle.us-east-1.api.aws/v1",
        "https://bedrock-mantle.us-east-1.api.aws.example.com/v1",
        "https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1",
        "https://proxy.example.com/v1",
        "http://127.0.0.1:1234/v1",
    ] {
        assert_eq!(mantle_closed_gpt_base_url(base), None, "{base}");
        let backend = bedrock_backend(reqwest::Client::new(), "key".into(), None, Some(base));
        assert!(backend.closed_gpt.is_none(), "{base}");
    }
}

#[derive(Debug)]
struct CapturedRequest {
    path: String,
    headers: HeaderMap,
    body: Value,
}

#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<CapturedRequest>>,
    rejection: Option<StatusCode>,
}

async fn catalog(State(state): State<Arc<Recorder>>, headers: HeaderMap) -> Json<Value> {
    state.requests.lock().await.push(CapturedRequest {
        path: "/v1/models".into(),
        headers,
        body: Value::Null,
    });
    Json(json!({"object":"list","data":[
        {"id":"openai.gpt-5.6-luna","object":"model","created":0,"owned_by":"bedrock"},
        {"id":"openai.gpt-6-luna","object":"model","created":0,"owned_by":"bedrock"},
        {"id":"openai.gpt-oss-120b","object":"model","created":0,"owned_by":"bedrock"},
        {"id":"google.gemma-3-4b-it","object":"model","created":0,"owned_by":"bedrock"},
        {"id":"openai.gpt-5.5-2026-04-23","object":"model","created":0,"owned_by":"system","status":"available"},
        {"id":"google.gemma-4-31b","object":"model","created":0,"owned_by":"system","status":"available"},
        {"id":"xai.grok-4.3","object":"model","created":0,"owned_by":"system","status":"available"},
        {"id":"anthropic.claude-fable-5","object":"model","created":0,"owned_by":"system","status":"unavailable",
         "status_reason":"This model is not available under data retention mode 'default'."}
    ]}))
}

async fn completion(
    State(state): State<Arc<Recorder>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let path = uri.path().to_string();
    state.requests.lock().await.push(CapturedRequest {
        path: path.clone(),
        headers,
        body: body.clone(),
    });
    if let Some(status) = state.rejection {
        return (
            status,
            Json(json!({"error":{"message":"provider rejected invocation",
            "type":"provider_error"}})),
        )
            .into_response();
    }
    // Independent literal-wire fixture: closed GPT works only on the new
    // dialect; other models work only on the original endpoint.
    let expected_path = match body["model"].as_str().unwrap() {
        "openai.gpt-5.6-sol"
        | "openai.gpt-5.6-terra"
        | "openai.gpt-5.6-luna"
        | "openai.gpt-6-sol"
        | "openai.gpt-6-luna"
        | "openai.gpt-5.5-2026-04-23"
        | "google.gemma-4-31b"
        | "xai.grok-4.3" => "/openai/v1/chat/completions",
        _ => "/v1/chat/completions",
    };
    if path != expected_path {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"model isn't supported on this route","type":"invalid_request_error"}})),
        )
            .into_response();
    }
    let model = &body["model"];
    if body["stream"] == true {
        let first = json!({"id":"chat_mantle","object":"chat.completion.chunk","created":1,
            "model":model,"choices":[{"index":0,"delta":{"role":"assistant","content":"OK"},"finish_reason":null}]});
        let last = json!({"id":"chat_mantle","object":"chat.completion.chunk","created":1,
            "model":model,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}});
        return (
            [("content-type", "text/event-stream")],
            format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"),
        )
            .into_response();
    }
    Json(json!({"id":"chat_mantle","object":"chat.completion","created":1,"model":model,
        "choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}))
    .into_response()
}

async fn recorder() -> (String, Arc<Recorder>) {
    let state = Arc::new(Recorder::default());
    let base = mock::serve(
        Router::new()
            .route("/v1/models", get(catalog))
            .route("/v1/chat/completions", post(completion))
            .route("/openai/v1/chat/completions", post(completion))
            .with_state(state.clone()),
    )
    .await;
    (base, state)
}

fn request(model: &str) -> ChatCompletionRequest {
    serde_json::from_value(
        json!({"model":model,"messages":[{"role":"user","content":"Reply OK"}],
        "max_completion_tokens":64}),
    )
    .unwrap()
}

#[tokio::test]
async fn listed_mantle_models_use_their_generation_dialect_and_same_credential() {
    let (base, state) = recorder().await;
    let backend = mantle_backend_with_urls(
        reqwest::Client::new(),
        "account-two-key".into(),
        format!("{base}/v1"),
        Some(format!("{base}/openai/v1")),
    );
    let models = backend.list_models().await.unwrap();
    assert_eq!(
        models
            .data
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        [
            "openai.gpt-5.6-luna",
            "openai.gpt-6-luna",
            "openai.gpt-oss-120b",
            "google.gemma-3-4b-it",
            "openai.gpt-5.5-2026-04-23",
            "google.gemma-4-31b",
            "xai.grok-4.3",
        ],
        "a model Mantle reports unavailable is not listed"
    );
    assert_eq!(
        state.requests.lock().await.len(),
        1,
        "listing must not invoke models"
    );
    for model in models.data {
        let response = backend
            .create_chat_completion(request(&model.id))
            .await
            .unwrap();
        assert_eq!(response.choices[0].message.content, Some(json!("OK")));
        assert_eq!(response.usage.unwrap().total_tokens, 10);
    }
    let requests = state.requests.lock().await;
    assert_eq!(
        requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        [
            "/v1/models",
            "/openai/v1/chat/completions",
            "/openai/v1/chat/completions",
            "/v1/chat/completions",
            "/v1/chat/completions",
            "/openai/v1/chat/completions",
            "/openai/v1/chat/completions",
            "/openai/v1/chat/completions",
        ]
    );
    for captured in requests.iter() {
        assert_eq!(captured.headers["authorization"], "Bearer account-two-key");
    }
}

#[tokio::test]
async fn closed_gpt_stream_and_translated_surfaces_inherit_the_generation_dialect() {
    let (base, state) = recorder().await;
    let backend = mantle_backend_with_urls(
        reqwest::Client::new(),
        "saved-key".into(),
        format!("{base}/v1"),
        Some(format!("{base}/openai/v1")),
    );
    for model in [
        "openai.gpt-5.6-sol",
        "openai.gpt-5.6-terra",
        "openai.gpt-6-sol",
    ] {
        let stream = backend
            .create_chat_completion_stream(request(model))
            .await
            .unwrap();
        let bytes = stream.try_collect::<Vec<_>>().await.unwrap().concat();
        let sse = String::from_utf8(bytes).unwrap();
        assert!(sse.contains("\"content\":\"OK\""));
        assert!(sse.contains("\"total_tokens\":10"));
        assert!(sse.ends_with("data: [DONE]\n\n"));

        let response_request: ResponsesRequest = serde_json::from_value(json!({
            "model":model,"input":"Reply OK","max_output_tokens":64}))
        .unwrap();
        let response = create_response_via_chat(&backend, response_request.clone())
            .await
            .unwrap();
        assert_eq!(response.body["output"][0]["content"][0]["text"], "OK");
        let stream = create_response_stream_via_chat(&backend, response_request)
            .await
            .unwrap();
        let bytes = stream
            .stream
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .concat();
        assert!(String::from_utf8(bytes)
            .unwrap()
            .contains("response.completed"));

        let messages = Bytes::from(
            serde_json::to_vec(&json!({"model":model,
            "messages":[{"role":"user","content":"Reply OK"}],"max_tokens":64}))
            .unwrap(),
        );
        let response = translate_messages(&backend, &messages).await.unwrap();
        assert_eq!(response.value["content"][0]["text"], "OK");
    }
    let requests = state.requests.lock().await;
    assert_eq!(requests.len(), 12);
    for (index, captured) in requests.iter().enumerate() {
        assert_eq!(captured.path, "/openai/v1/chat/completions");
        assert_eq!(captured.headers["authorization"], "Bearer saved-key");
        assert_eq!(captured.body["messages"][0]["content"], "Reply OK");
        assert_eq!(captured.body["max_completion_tokens"], 64);
        assert_eq!(captured.body["stream"], index % 4 == 0);
    }
}

#[tokio::test]
async fn custom_proxy_and_explicit_openai_base_urls_remain_authoritative_on_wire() {
    for path in ["/v1", "/openai/v1", "/proxy/v1"] {
        let state = Arc::new(Recorder::default());
        async fn capture(
            State(state): State<Arc<Recorder>>,
            OriginalUri(uri): OriginalUri,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            state.requests.lock().await.push(CapturedRequest {
                path: uri.path().into(),
                headers,
                body: body.clone(),
            });
            Json(
                json!({"id":"override","object":"chat.completion","created":1,"model":body["model"],
                "choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}]}),
            )
        }
        let base = mock::serve(
            Router::new()
                .route(&format!("{path}/chat/completions"), post(capture))
                .with_state(state.clone()),
        )
        .await;
        let backend = bedrock_backend(
            reqwest::Client::new(),
            "custom-key".into(),
            None,
            Some(&format!("{base}{path}///")),
        );
        backend
            .create_chat_completion(request("openai.gpt-6-luna"))
            .await
            .unwrap();
        let requests = state.requests.lock().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, format!("{path}/chat/completions"));
        assert_eq!(requests[0].headers["authorization"], "Bearer custom-key");
    }
}

#[tokio::test]
async fn rejected_closed_gpt_invocations_preserve_status_and_catalog_entries() {
    for status in [
        StatusCode::FORBIDDEN,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let state = Arc::new(Recorder {
            requests: Mutex::new(Vec::new()),
            rejection: Some(status),
        });
        let base = mock::serve(
            Router::new()
                .route("/v1/models", get(catalog))
                .route("/openai/v1/chat/completions", post(completion))
                .with_state(state.clone()),
        )
        .await;
        let backend = mantle_backend_with_urls(
            reqwest::Client::new(),
            "account-key".into(),
            format!("{base}/v1"),
            Some(format!("{base}/openai/v1")),
        );
        let error = backend
            .create_chat_completion(request("openai.gpt-6-luna"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, super::BackendError::ApiError { status: actual, .. } if actual == status.as_u16())
        );
        let models = backend.list_models().await.unwrap();
        assert!(models
            .data
            .iter()
            .any(|model| model.id == "openai.gpt-6-luna"));
        let requests = state.requests.lock().await;
        assert_eq!(requests.len(), 2, "no endpoint retry or hidden invocation");
        assert_eq!(requests[0].path, "/openai/v1/chat/completions");
        assert_eq!(requests[1].path, "/v1/models");
        assert_eq!(requests[0].headers["authorization"], "Bearer account-key");
    }
}
