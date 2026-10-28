//! Jev prompting: OpenAI chat completions served by TypeSafe's System One
//! endpoint through the shared chat pipeline (routing, capability gating,
//! synthesized streaming, usage).

use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::{routing::get, routing::post, Json, Router};
use http_body_util::BodyExt;
use tower::ServiceExt;

use openai_server::backend::RouterBackend;
use openai_server::model_routes::InMemoryModelRouteStore;
use openai_server::provider_connections::{
    ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
};

use crate::support::app::{build_app, TEST_API_KEY};

#[derive(Default)]
struct Upstream {
    requests: Mutex<Vec<serde_json::Value>>,
    wires: Mutex<Vec<String>>,
    authorization: Mutex<Vec<Option<String>>>,
}

async fn system_one(
    State(upstream): State<Arc<Upstream>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> impl IntoResponse {
    let wire = String::from_utf8(bytes.to_vec()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&wire).unwrap();
    upstream.wires.lock().unwrap().push(wire);
    upstream.authorization.lock().unwrap().push(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    );
    upstream.requests.lock().unwrap().push(body);
    Json(serde_json::json!({
        "model": "jev-1.13.0",
        "answers": {
            "billing": {"type": "noul", "noul": 0.98},
            "tone": {
                "type": "choice",
                "choice": "angry",
                "confidence": 1.0,
                "probabilities": {"angry": 1.0, "calm": 0.0}
            },
            "urgency": {
                "type": "score",
                "score": 1.99,
                "confidence": 0.99,
                "legend": {"0": "Can wait", "1": "This week", "2": "Today"},
                "probabilities": {"0": 0.0, "1": 0.01, "2": 0.99}
            }
        },
        "usage": {"input_tokens": 392, "output_tokens": 73}
    }))
}

async fn models() -> impl IntoResponse {
    Json(serde_json::json!({
        "models": [{
            "name": "jev-latest",
            "description": "Latest stable Jev",
            "release_date": "2026-09-10T18:38:01.391457+00:00"
        }]
    }))
}

async fn typesafe_app() -> (Router, Arc<Upstream>) {
    let upstream = Arc::new(Upstream::default());
    let mock = Router::new()
        .route("/v1/systemone", post(system_one))
        .route("/v1/models", get(models))
        .with_state(upstream.clone());
    let base = crate::support::mock::serve(mock).await;

    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    connections
        .create(ConnectionInput {
            name: "typesafe-primary".to_string(),
            provider_kind: "typesafe".to_string(),
            base_url: Some(format!("{base}/v1")),
            prefix: Some("typesafe/".to_string()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "typesafe-secret".to_string(),
        })
        .await
        .unwrap();
    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections,
        Arc::new(InMemoryModelRouteStore::new()),
    ));
    (build_app(backend).await, upstream)
}

fn triage_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "triage",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": {
                    "billing": {"type": "boolean", "description": "Is this message about billing?"},
                    "tone": {
                        "type": "string",
                        "description": "What is the tone?",
                        "oneOf": [
                            {"const": "angry", "description": "upset or hostile"},
                            {"const": "calm", "description": "neutral or polite"}
                        ]
                    },
                    "urgency": {
                        "type": "integer",
                        "description": "How urgent is this?",
                        "oneOf": [
                            {"const": 0, "description": "Can wait"},
                            {"const": 1, "description": "This week"},
                            {"const": 2, "description": "Today"}
                        ]
                    }
                },
                "required": ["billing", "tone", "urgency"],
                "additionalProperties": false
            }
        }
    })
}

fn prompt(stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": "typesafe/jev-latest",
        "messages": [
            {"role": "system", "content": "You triage support tickets."},
            {"role": "user", "content": "I was charged twice and nobody answers my emails!"}
        ],
        "response_format": triage_schema(),
        "stream": stream
    })
}

fn request(body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&bytes)))
}

#[tokio::test]
async fn chat_prompt_is_answered_as_typed_decisions() {
    let (app, upstream) = typesafe_app().await;

    let response = app.oneshot(request(prompt(false))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;

    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["model"], "jev-1.13.0");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["choices"][0]["message"]["role"], "assistant");
    let content: serde_json::Value =
        serde_json::from_str(body["choices"][0]["message"]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        content,
        serde_json::json!({"billing": true, "tone": "angry", "urgency": 2})
    );
    assert_eq!(body["usage"]["prompt_tokens"], 392);
    assert_eq!(body["usage"]["completion_tokens"], 73);
    assert_eq!(body["usage"]["total_tokens"], 465);

    let requests = upstream.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let native = &requests[0];
    assert_eq!(native["model"], "jev-latest");
    assert_eq!(
        native["state"],
        serde_json::json!([
            {"role": "system", "content": "You triage support tickets."},
            {"role": "user", "content": "I was charged twice and nobody answers my emails!"}
        ])
    );
    assert_eq!(
        native["questions"],
        serde_json::json!({
            "billing": {"type": "noul", "instructions": "Is this message about billing?"},
            "tone": {
                "type": "choice",
                "instructions": "What is the tone?",
                "criteria": {"angry": "upset or hostile", "calm": "neutral or polite"}
            },
            "urgency": {
                "type": "score",
                "instructions": "How urgent is this?",
                "criteria": ["Can wait", "This week", "Today"]
            }
        })
    );
    assert!(
        native.get("stream").is_none() && native.get("response_format").is_none(),
        "only the System One contract reaches TypeSafe: {native}"
    );
    assert_eq!(
        upstream.authorization.lock().unwrap().as_slice(),
        [Some("Bearer typesafe-secret".to_string())]
    );
}

#[tokio::test]
async fn streamed_prompts_are_synthesized_from_the_whole_answer() {
    let (app, upstream) = typesafe_app().await;

    let response = app.oneshot(request(prompt(true))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream")));
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let stream = String::from_utf8(bytes.to_vec()).unwrap();

    let mut content = String::new();
    for frame in stream.split("\n\n") {
        let Some(data) = frame.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let chunk: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(chunk["object"], "chat.completion.chunk");
        if let Some(delta) = chunk["choices"][0]["delta"]["content"].as_str() {
            content.push_str(delta);
        }
    }
    assert!(stream.trim_end().ends_with("data: [DONE]"), "{stream}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&content).unwrap(),
        serde_json::json!({"billing": true, "tone": "angry", "urgency": 2})
    );
    assert_eq!(upstream.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn prompts_without_a_schema_are_rejected_with_the_prompting_guide() {
    let (app, upstream) = typesafe_app().await;

    let mut body = prompt(false);
    body.as_object_mut().unwrap().remove("response_format");
    let response = app.oneshot(request(body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("`response_format` must be a `json_schema`."),
        "{message}"
    );
    assert!(message.contains("/v1/alpha/decisions"), "{message}");
    assert!(upstream.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn tool_prompts_never_reach_typesafe() {
    let (app, upstream) = typesafe_app().await;

    let mut body = prompt(false);
    body["tools"] = serde_json::json!([{
        "type": "function",
        "function": {"name": "refund", "parameters": {"type": "object", "properties": {}}}
    }]);
    let response = app.oneshot(request(body)).await.unwrap();
    assert!(
        response.status().is_client_error(),
        "tools must be refused at routing, got {}",
        response.status()
    );
    assert!(upstream.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn schema_order_survives_jev_questions_answers_and_synthesized_streaming() {
    for streaming in [false, true] {
        let (app, upstream) = typesafe_app().await;
        // The mock deliberately returns billing, tone, urgency in a different
        // order; the client declared urgency, billing, tone.
        let wire = format!(
            r#"{{"model":"typesafe/jev-latest","messages":[{{"role":"user","content":"triage"}}],"stream":{streaming},"response_format":{{"type":"json_schema","json_schema":{{"schema":{{"type":"object","properties":{{"urgency":{{"type":"integer","minimum":0,"maximum":2}},"billing":{{"type":"boolean"}},"tone":{{"type":"string","enum":["angry","calm"]}}}}}}}}}}}}"#
        );
        let response = app
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(wire))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let content = if streaming {
            let stream = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(stream.trim_end().ends_with("data: [DONE]"));
            let mut content = String::new();
            for frame in stream.split("\n\n") {
                let Some(data) = frame.strip_prefix("data: ") else {
                    continue;
                };
                if data == "[DONE]" {
                    continue;
                }
                let chunk: serde_json::Value = serde_json::from_str(data).unwrap();
                if let Some(delta) = chunk["choices"][0]["delta"]["content"].as_str() {
                    content.push_str(delta);
                }
            }
            content
        } else {
            let response: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            response["choices"][0]["message"]["content"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(content, r#"{"urgency":2,"billing":true,"tone":"angry"}"#);
        let wires = upstream.wires.lock().unwrap();
        assert_eq!(wires.len(), 1);
        let wire = &wires[0];
        let urgency = wire.find(r#""urgency":{"#).unwrap();
        let billing = wire.find(r#""billing":{"#).unwrap();
        let tone = wire.find(r#""tone":{"#).unwrap();
        assert!(urgency < billing && billing < tone, "{wire}");
    }
}
