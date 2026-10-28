//! Native Mantle fixtures retain raw wire bodies independently of typed views.
use super::{mantle_backend_with_urls, BackendProvider};
use crate::{
    backend::provider::{BackendError, UpstreamErrorFormat},
    capabilities::{DeliveryMode, Protocol, ProtocolOperation},
    native_chat::NativeChatRequest,
};
use axum::{
    body::Body,
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use futures::TryStreamExt;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

#[path = "../../../tests/integration/support/mock.rs"]
mod mock;

const FUTURE: &str = r#"{ "z":[1.0000000000000001,null], "a":{"kept":true} }"#;
const REPLY: &str = r#"{ "id":"native-mantle", "object":"chat.completion", "created":1, "model":"provider-model", "choices":[{"index":0,"message":{"role":"assistant","content":null,"refusal":"fixture refusal","annotations":[{"type":"fixture","future":null}]},"finish_reason":"stop","logprobs":{"content":null}}], "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10,"future_tokens":null}, "service_tier":"priority", "future_reply":{"exact":1.0000000000000001,"null":null} }"#;
const SSE: &str = concat!(
    r#"data: {"id":"native-mantle","object":"chat.completion.chunk","created":1,"model":"provider-model","choices":[{"index":0,"delta":{"role":"assistant","refusal":"fixture refusal","annotations":[{"future":null}]},"logprobs":{"content":null},"finish_reason":null}],"future_chunk":1.0000000000000001}"#,
    "\n\n",
    r#"data: {"id":"native-mantle","object":"chat.completion.chunk","created":1,"model":"provider-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"service_tier":"priority","future_terminal":null}"#,
    "\n\n",
    r#"data: {"id":"native-mantle","object":"chat.completion.chunk","created":1,"model":"provider-model","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10,"future_tokens":null}}"#,
    "\n\ndata: [DONE]\n\n"
);
const ERROR: &str = r#"{ "error":{"message":"provider rejection","type":"fixture_error","param":"future_control","code":"fixture_code","future":null}, "future_error":1.0000000000000001 }"#;

struct Captured {
    path: String,
    headers: HeaderMap,
    raw: String,
}
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<Captured>>,
    rejection: Option<StatusCode>,
}

fn reply(status: StatusCode, mime: &str, body: &'static str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", mime)
        .header("x-request-id", "mantle-native-request")
        .header("openai-processing-ms", "17.25")
        .header("x-ratelimit-remaining-tokens", "41")
        .header("retry-after", "Tue, 06 Oct 2026 12:00:00 GMT")
        .header("set-cookie", "provider-secret=hidden")
        .header("authorization", "Bearer hidden")
        .body(Body::from(body))
        .unwrap()
}

async fn completion(
    State(recorder): State<Arc<Recorder>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    raw: String,
) -> Response {
    let body: Value = serde_json::from_str(&raw).unwrap();
    let path = uri.path().to_owned();
    recorder.calls.lock().await.push(Captured {
        path: path.clone(),
        headers,
        raw,
    });
    // Literal endpoint facts: neither this recorder nor its expected paths
    // calls the production dialect classifier.
    let expected = match body["model"].as_str().unwrap() {
        "openai.gpt-6-luna" => "/openai/v1/chat/completions",
        "openai.gpt-oss-120b" | "google.gemma-3-4b-it" => "/v1/chat/completions",
        model => panic!("unexpected fixture model {model}"),
    };
    if path != expected {
        return reply(
            StatusCode::BAD_REQUEST,
            "application/json",
            r#"{"error":{"message":"wrong Mantle generation dialect"}}"#,
        );
    }
    if let Some(status) = recorder.rejection {
        return reply(status, "application/json; charset=utf-8", ERROR);
    }
    if body["stream"] == true {
        reply(StatusCode::OK, "text/event-stream", SSE)
    } else {
        reply(
            StatusCode::CREATED,
            "application/json; charset=utf-8",
            REPLY,
        )
    }
}

async fn backend(rejection: Option<StatusCode>) -> (super::BedrockMantleBackend, Arc<Recorder>) {
    let recorder = Arc::new(Recorder {
        rejection,
        ..Default::default()
    });
    let base = mock::serve(
        Router::new()
            .route("/v1/chat/completions", post(completion))
            .route("/openai/v1/chat/completions", post(completion))
            .with_state(recorder.clone()),
    )
    .await;
    (
        mantle_backend_with_urls(
            reqwest::Client::new(),
            "same-account-fixture-key".into(),
            format!("{base}/v1"),
            Some(format!("{base}/openai/v1")),
        ),
        recorder,
    )
}

fn request(model: &str, stream: bool) -> NativeChatRequest {
    let raw = format!(
        r#"{{"model":"placeholder","messages":[{{"role":"user","content":"Reply OK"}}],"max_completion_tokens":64,"future_control":{FUTURE}}}"#
    );
    let mut request: NativeChatRequest = serde_json::from_str(&raw).unwrap();
    request.set_model(model.into());
    if stream {
        request
            .set_field(
                "stream_options",
                &serde_json::json!({"include_usage":true,"future_stream_option":{"kept":null}}),
            )
            .unwrap();
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        "Bearer client-credential-must-not-leak".parse().unwrap(),
    );
    headers.insert("openai-beta", "fixture-beta".parse().unwrap());
    request.with_headers(&headers)
}

fn assert_headers(headers: &[(String, String)]) {
    for (name, expected) in [
        ("x-request-id", "mantle-native-request"),
        ("openai-processing-ms", "17.25"),
        ("x-ratelimit-remaining-tokens", "41"),
        ("retry-after", "Tue, 06 Oct 2026 12:00:00 GMT"),
    ] {
        assert!(
            headers
                .iter()
                .any(|(key, value)| key == name && value == expected),
            "{name}"
        );
    }
    assert!(!headers
        .iter()
        .any(|(name, _)| name == "set-cookie" || name == "authorization"));
}

async fn assert_calls(recorder: &Recorder, stream: bool) {
    let calls = recorder.calls.lock().await;
    assert_eq!(calls.len(), 3);
    for (index, (call, model)) in calls
        .iter()
        .zip([
            "openai.gpt-6-luna",
            "openai.gpt-oss-120b",
            "google.gemma-3-4b-it",
        ])
        .enumerate()
    {
        assert_eq!(
            call.path,
            if index == 0 {
                "/openai/v1/chat/completions"
            } else {
                "/v1/chat/completions"
            }
        );
        assert_eq!(
            call.headers["authorization"],
            "Bearer same-account-fixture-key"
        );
        assert_eq!(call.headers["openai-beta"], "fixture-beta");
        assert!(
            call.raw.contains(FUTURE),
            "unknown nested values retain order, nulls, and exact numbers"
        );
        let body: Value = serde_json::from_str(&call.raw).unwrap();
        assert_eq!(body["model"], model);
        assert_eq!(body["max_completion_tokens"], 64);
        assert_eq!(body["stream"], stream);
        if stream {
            assert_eq!(body["stream_options"]["include_usage"], true);
            assert!(body["stream_options"]["future_stream_option"]
                .get("kept")
                .unwrap()
                .is_null());
        }
    }
}

#[tokio::test]
async fn native_chat_dialects_preserve_authoritative_request_reply_and_headers() {
    let (backend, recorder) = backend(None).await;
    for model in [
        "openai.gpt-6-luna",
        "openai.gpt-oss-120b",
        "google.gemma-3-4b-it",
    ] {
        let response = backend
            .create_native_chat_completion(request(model, false))
            .await
            .unwrap();
        assert_eq!(response.body.get(), REPLY);
        assert_eq!(response.status, 201);
        assert_eq!(response.content_type, "application/json; charset=utf-8");
        assert_eq!(response.completion.usage.unwrap().total_tokens, 10);
        assert_headers(&response.headers);
    }
    assert_calls(&recorder, false).await;
}

#[tokio::test]
async fn native_stream_dialects_preserve_request_options_sse_bytes_and_headers() {
    let (backend, recorder) = backend(None).await;
    for model in [
        "openai.gpt-6-luna",
        "openai.gpt-oss-120b",
        "google.gemma-3-4b-it",
    ] {
        let response = backend
            .create_native_chat_completion_stream(request(model, true))
            .await
            .unwrap();
        assert_headers(&response.headers);
        assert_eq!(
            response
                .stream
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .concat(),
            SSE.as_bytes()
        );
    }
    assert_calls(&recorder, true).await;
}

#[tokio::test]
async fn native_closed_gpt_errors_keep_body_status_headers_and_single_account() {
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let (backend, recorder) = backend(Some(status)).await;
        for stream in [false, true] {
            let error = if stream {
                match backend
                    .create_native_chat_completion_stream(request("openai.gpt-6-luna", true))
                    .await
                {
                    Err(error) => error,
                    Ok(_) => panic!("expected provider error"),
                }
            } else {
                backend
                    .create_native_chat_completion(request("openai.gpt-6-luna", false))
                    .await
                    .unwrap_err()
            };
            let BackendError::ApiError {
                status: actual,
                original: Some(original),
                headers,
                ..
            } = error
            else {
                panic!("native error provenance must survive");
            };
            assert_eq!(actual, status.as_u16());
            assert_eq!(original.body.as_ref(), ERROR.as_bytes());
            assert_eq!(original.content_type, "application/json; charset=utf-8");
            assert_eq!(original.format, UpstreamErrorFormat::Openai);
            assert_headers(&headers);
        }
        let calls = recorder.calls.lock().await;
        assert_eq!(
            calls.len(),
            2,
            "no hidden probes, dialect retries, or credential swapping"
        );
        for call in calls.iter() {
            assert_eq!(call.path, "/openai/v1/chat/completions");
            assert_eq!(
                call.headers["authorization"],
                "Bearer same-account-fixture-key"
            );
        }
    }
}

#[test]
fn closed_gpt_responses_are_native_and_messages_use_chat_translation() {
    use crate::capabilities::{EndpointFeature, Support};
    for model in ["openai.gpt-5.6-luna", "openai.gpt-6-luna"] {
        let capabilities = crate::capabilities::resolve("bedrock", model, None, None).unwrap();
        let responses = capabilities
            .profile
            .operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
            .unwrap();
        assert_eq!(responses.mode, DeliveryMode::Native, "{model}");
        for feature in [
            EndpointFeature::Streaming,
            EndpointFeature::StoredContinuation,
            EndpointFeature::ToolCalling,
            EndpointFeature::ImageInput,
            EndpointFeature::ServerToolWebSearch,
        ] {
            assert_eq!(
                responses.features.get(&feature),
                Some(&Support::Supported),
                "{model} / {feature:?}"
            );
        }
        // Unverified server tools are refused before dispatch so a fallback
        // candidate can still serve them.
        for feature in [
            EndpointFeature::ServerToolFileSearch,
            EndpointFeature::ServerToolCodeInterpreter,
            EndpointFeature::ServerToolComputerUse,
            EndpointFeature::ServerToolMcp,
        ] {
            assert_eq!(
                responses.features.get(&feature),
                Some(&Support::Unsupported),
                "{model} / {feature:?}"
            );
        }
        // Response IDs are endpoint-scoped and the locator does not record
        // the endpoint, so the ID-only operations (and background, which
        // needs retrieve) are not offered.
        assert_ne!(
            responses.features.get(&EndpointFeature::Background),
            Some(&Support::Supported),
            "{model}"
        );
        assert!(capabilities
            .profile
            .operation(Protocol::OpenaiResponses, ProtocolOperation::Retrieve)
            .is_none_or(|operation| !matches!(
                operation.mode,
                DeliveryMode::Native | DeliveryMode::Adapted
            )));
        assert_eq!(
            capabilities
                .profile
                .operation(Protocol::AnthropicMessages, ProtocolOperation::Create)
                .unwrap()
                .mode,
            DeliveryMode::Translated,
            "{model}"
        );
    }
    let open_weight =
        crate::capabilities::resolve("bedrock", "openai.gpt-oss-120b", None, None).unwrap();
    assert_eq!(
        open_weight
            .profile
            .operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
            .unwrap()
            .mode,
        DeliveryMode::Translated
    );
}

#[tokio::test]
async fn native_responses_select_the_mantle_endpoint_by_model() {
    let paths = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let recorder = paths.clone();
    let base = mock::serve(Router::new().fallback(
        move |OriginalUri(uri): OriginalUri, raw: String| {
            let recorder = recorder.clone();
            async move {
                let body: Value = serde_json::from_str(&raw).unwrap();
                recorder.lock().await.push((
                    uri.path().to_owned(),
                    body["model"].as_str().unwrap().to_owned(),
                ));
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"id":"resp","object":"response","status":"completed","output":[]}"#,
                    ))
                    .unwrap()
            }
        },
    ))
    .await;
    let backend = mantle_backend_with_urls(
        reqwest::Client::new(),
        "same-account-fixture-key".into(),
        format!("{base}/v1"),
        Some(format!("{base}/openai/v1")),
    );
    for model in ["openai.gpt-6-luna", "openai.gpt-oss-120b"] {
        let request: crate::responses::ResponsesRequest =
            serde_json::from_value(serde_json::json!({"model": model, "input": "hi"})).unwrap();
        backend
            .create_response(request, &Default::default())
            .await
            .unwrap();
    }
    assert_eq!(
        *paths.lock().await,
        vec![
            ("/openai/v1/responses".into(), "openai.gpt-6-luna".into()),
            ("/v1/responses".into(), "openai.gpt-oss-120b".into()),
        ]
    );
}
