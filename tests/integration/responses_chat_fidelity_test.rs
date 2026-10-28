//! Literal native Chat replies translated through the real Responses handler.

use std::sync::Arc;

use axum::{extract::State, middleware, response::IntoResponse, routing::post, Json, Router};
use openai_server::{
    backend::RouterBackend,
    handlers,
    middleware::auth_middleware,
    model_routes::InMemoryModelRouteStore,
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::support::{
    app::{AppStateBuilder, TEST_API_KEY},
    mock::serve,
};

const REFUSAL: &str = r#"{"id":"chatcmpl-refusal","object":"chat.completion","created":1700000000,"model":"command-r","choices":[{"index":0,"message":{"role":"assistant","content":null,"refusal":"I cannot help with that.","chat_only":"omit"},"finish_reason":"stop","logprobs":{"content":null,"refusal":[{"token":"I","logprob":-0.2}]}}],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14,"prompt_tokens_details":{"cached_tokens":3,"audio_tokens":2,"chat_only":99},"completion_tokens_details":{"reasoning_tokens":1,"audio_tokens":2,"accepted_prediction_tokens":1,"rejected_prediction_tokens":2,"chat_only":99}},"service_tier":"flex","system_fingerprint":"chat-only","chat_only":"omit"}"#;
const TEXT: &str = r#"{"id":"chatcmpl-text","object":"chat.completion","created":1700000000,"model":"command-r","choices":[{"index":0,"message":{"role":"assistant","content":"Read this.","refusal":null,"annotations":[{"type":"url_citation","url_citation":{"start_index":0,"end_index":4,"title":"Reference","url":"https://example.test/reference","chat_only":"omit"}}]},"finish_reason":"stop","logprobs":{"content":[{"token":"Read","logprob":-0.25,"bytes":[82,101,97,100],"top_logprobs":[{"token":"Read","logprob":-0.25,"bytes":[82,101,97,100]}]}],"refusal":null}}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}"#;

#[derive(Default)]
struct Recorder(Mutex<Vec<Value>>);

async fn upstream(
    State(recorder): State<Arc<Recorder>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    assert_eq!(body["model"], "command-r");
    assert_eq!(body["stream"], false);
    let input = body["messages"][0]["content"].as_str().unwrap();
    let reply = match input {
        "refusal-null" => REFUSAL.to_owned(),
        "refusal-empty" => REFUSAL.replace("\"content\":null", "\"content\":\"\""),
        "text" => TEXT.to_owned(),
        "plain" => r#"{"id":"chatcmpl-plain","object":"chat.completion","created":1700000000,"model":"command-r","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}"#.to_owned(),
        "mixed" => REFUSAL.replace("\"content\":null", "\"content\":\"Safe context.\""),
        other => panic!("unexpected input: {other}"),
    };
    recorder.0.lock().await.push(body);
    let mut response = reply.into_response();
    for (name, value) in [
        ("content-type", "application/json"),
        ("x-request-id", "translated-request"),
        ("openai-processing-ms", "12.5"),
        ("x-ratelimit-remaining-requests", "42"),
        ("retry-after", "2"),
        ("set-cookie", "provider-secret=hidden"),
        ("x-provider-secret", "hidden"),
    ] {
        response.headers_mut().insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    response
}

async fn app() -> (String, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    let base = serve(
        Router::new()
            .route("/chat/completions", post(upstream))
            .with_state(recorder.clone()),
    )
    .await;
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    connections
        .create(ConnectionInput {
            name: "translated-chat".into(),
            provider_kind: "cohere".into(),
            base_url: Some(base),
            prefix: Some("cohere/".into()),
            exposure: ExposurePolicy::Unlisted,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "mock-key".into(),
        })
        .await
        .unwrap();
    let routes = Arc::new(InMemoryModelRouteStore::new());
    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections.clone(),
        routes.clone(),
    ));
    let state = AppStateBuilder::new(backend)
        .provider_connection_store(connections)
        .model_route_store(routes)
        .build()
        .await;
    let router = Router::new()
        .route("/v1/responses", post(handlers::responses::create_response))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);
    (serve(router).await, recorder)
}

async fn request(base: &str, input: &str, stream: bool) -> (Value, Vec<Value>) {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .bearer_auth(TEST_API_KEY)
        .json(&json!({"model":"cohere/command-r","input":input,"stream":stream}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    for (name, value) in [
        ("x-request-id", "translated-request"),
        ("openai-processing-ms", "12.5"),
        ("x-ratelimit-remaining-requests", "42"),
        ("retry-after", "2"),
    ] {
        assert_eq!(
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok()),
            Some(value),
            "{name}"
        );
    }
    for name in ["set-cookie", "x-provider-secret"] {
        assert!(!response.headers().contains_key(name));
    }
    let bytes = response.bytes().await.unwrap();
    if !stream {
        return (serde_json::from_slice(&bytes).unwrap(), Vec::new());
    }
    let events: Vec<Value> = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap()["response"]
        .clone();
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], index as u64);
    }
    (completed, events)
}

#[tokio::test]
async fn responses_translation_retains_refusal_headers_and_usage_in_both_modes() {
    let (base, recorder) = app().await;
    for input in ["refusal-null", "refusal-empty"] {
        for stream in [false, true] {
            let (reply, events) = request(&base, input, stream).await;
            assert_eq!(
                reply["output"][0]["content"],
                json!([{"type":"refusal","refusal":"I cannot help with that."}])
            );
            assert_eq!(reply["usage"]["input_tokens"], 10);
            assert_eq!(reply["usage"]["output_tokens"], 4);
            assert_eq!(reply["usage"]["total_tokens"], 14);
            assert_eq!(
                reply["usage"]["input_tokens_details"],
                json!({"cached_tokens":3,"cache_write_tokens":0,"audio_tokens":2})
            );
            assert_eq!(
                reply["usage"]["output_tokens_details"],
                json!({"reasoning_tokens":1,"audio_tokens":2,"accepted_prediction_tokens":1,"rejected_prediction_tokens":2})
            );
            assert_eq!(reply["service_tier"], "flex");
            for field in ["chat_only", "system_fingerprint", "choices"] {
                assert!(reply.get(field).is_none());
            }
            if stream {
                for event_type in ["response.refusal.delta", "response.refusal.done"] {
                    assert_eq!(
                        events
                            .iter()
                            .filter(|event| event["type"] == event_type)
                            .count(),
                        1
                    );
                }
                assert!(!events
                    .iter()
                    .any(|event| event["type"] == "response.output_text.delta"));
                assert_eq!(
                    events
                        .iter()
                        .find(|event| event["type"] == "response.refusal.delta")
                        .unwrap()["delta"],
                    "I cannot help with that."
                );
                assert_eq!(
                    events
                        .iter()
                        .find(|event| event["type"] == "response.refusal.done")
                        .unwrap()["refusal"],
                    "I cannot help with that."
                );
                assert_eq!(
                    events
                        .iter()
                        .find(|event| event["type"] == "response.content_part.added")
                        .unwrap()["part"],
                    json!({"type":"refusal","refusal":""})
                );
            }
        }
    }
    assert_eq!(recorder.0.lock().await.len(), 4);
}

#[tokio::test]
async fn responses_translation_retains_text_annotations_and_logprobs_in_both_modes() {
    let (base, recorder) = app().await;
    let annotation = json!({"type":"url_citation","start_index":0,"end_index":4,"title":"Reference","url":"https://example.test/reference"});
    let logprobs =
        json!([{"token":"Read","logprob":-0.25,"top_logprobs":[{"token":"Read","logprob":-0.25}]}]);
    for stream in [false, true] {
        let (reply, events) = request(&base, "text", stream).await;
        let part = &reply["output"][0]["content"][0];
        assert_eq!(part["text"], "Read this.");
        assert_eq!(part["annotations"], json!([annotation.clone()]));
        assert_eq!(part["logprobs"], logprobs);
        assert_eq!(reply["usage"]["total_tokens"], 3);
        assert!(!events
            .iter()
            .any(|event| event["type"] == "response.refusal.delta"));
        if stream {
            assert_eq!(
                events
                    .iter()
                    .find(|event| event["type"] == "response.output_text.delta")
                    .unwrap()["delta"],
                "Read this."
            );
            assert_eq!(
                events
                    .iter()
                    .find(|event| event["type"] == "response.output_text.done")
                    .unwrap()["logprobs"],
                logprobs
            );
            assert_eq!(
                events
                    .iter()
                    .find(|event| event["type"] == "response.output_text.annotation.added")
                    .unwrap()["annotation"],
                annotation
            );
        }
    }
    assert_eq!(recorder.0.lock().await.len(), 2);
}

#[tokio::test]
async fn responses_translation_plain_text_control_in_both_modes() {
    let (base, _) = app().await;
    for stream in [false, true] {
        let (reply, events) = request(&base, "plain", stream).await;
        assert_eq!(
            reply["output"][0]["content"],
            json!([{"type":"output_text","text":"hello"}])
        );
        assert_eq!(reply["usage"]["total_tokens"], 3);
        assert!(!events
            .iter()
            .any(|event| event["type"] == "response.refusal.delta"));
    }
}

#[tokio::test]
async fn responses_translation_streams_text_and_refusal_as_distinct_parts() {
    let (base, _) = app().await;
    let (reply, events) = request(&base, "mixed", true).await;
    assert_eq!(
        reply["output"][0]["content"],
        json!([
            {"type":"output_text","text":"Safe context."},
            {"type":"refusal","refusal":"I cannot help with that."},
        ])
    );
    for (event_type, content_index) in [
        ("response.output_text.delta", 0),
        ("response.refusal.delta", 1),
    ] {
        let event = events
            .iter()
            .find(|event| event["type"] == event_type)
            .unwrap();
        assert_eq!(event["content_index"], content_index);
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "response.content_part.done")
            .count(),
        2
    );
}
