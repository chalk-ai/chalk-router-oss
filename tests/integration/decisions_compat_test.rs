//! Decisions across wire formats: OpenAI's `POST /v1/decisions` served by
//! System One providers, Router Decisions served by OpenAI and Perplexity,
//! OpenRouter's non-Jev decision models, images, and box questions.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{middleware, routing::post, Json, Router};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use openai_server::backend::{BackendProvider, RouterBackend};
use openai_server::handlers;
use openai_server::middleware::auth_middleware;
use openai_server::model_routes::InMemoryModelRouteStore;
use openai_server::provider_connections::{
    ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
};

use crate::support::app::{AppStateBuilder, TEST_API_KEY};

/// What a mock upstream saw, and what it answers with.
struct Upstream {
    bodies: Mutex<Vec<Value>>,
    image_opt_in: Mutex<Vec<Option<String>>>,
    reply: Value,
}

impl Upstream {
    fn replying(reply: Value) -> Arc<Self> {
        Arc::new(Self {
            bodies: Mutex::default(),
            image_opt_in: Mutex::default(),
            reply,
        })
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
}

async fn record(
    State(upstream): State<Arc<Upstream>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    upstream.bodies.lock().unwrap().push(body);
    upstream.image_opt_in.lock().unwrap().push(
        headers
            .get("TypeSafe-Danger-Zone-Images")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    );
    Json(upstream.reply.clone()).into_response()
}

async fn serve(path: &str, upstream: Arc<Upstream>) -> String {
    crate::support::mock::serve(Router::new().route(path, post(record)).with_state(upstream)).await
}

async fn state_with(connections: &[(&str, String)]) -> openai_server::AppState {
    let store = Arc::new(InMemoryProviderConnectionStore::new());
    for (kind, base_url) in connections {
        store
            .create(ConnectionInput {
                name: format!("{kind}-primary"),
                provider_kind: kind.to_string(),
                base_url: Some(base_url.clone()),
                prefix: Some(format!("{kind}/")),
                exposure: ExposurePolicy::Unlisted,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: format!("{kind}-secret"),
            })
            .await
            .unwrap();
    }
    AppStateBuilder::new(Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        store,
        Arc::new(InMemoryModelRouteStore::new()),
    )))
    .build()
    .await
}

fn app(state: openai_server::AppState) -> Router {
    Router::new()
        .route(
            "/v1/alpha/decisions",
            post(handlers::decisions::create_decision),
        )
        .route(
            "/v1/decisions",
            post(handlers::decisions::create_openai_decision),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth_middleware))
}

async fn call(state: openai_server::AppState, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())),
    )
}

#[tokio::test]
async fn openai_format_requests_are_served_by_typesafe_with_images() {
    let typesafe = Upstream::replying(json!({
        "model": "jev-1.14.0",
        "answers": {
            "person": {"type": "noul", "noul": 0.92},
            "mood": {"type": "score", "score": 1.6, "confidence": 0.7,
                     "legend": {"0": "calm", "1": "busy", "2": "chaotic"},
                     "probabilities": {"0": 0.1, "1": 0.2, "2": 0.7}}
        },
        "usage": {"input_tokens": 900, "output_tokens": 6}
    }));
    let base = serve("/v1/systemone", typesafe.clone()).await;
    let state = state_with(&[("typesafe", format!("{base}/v1"))]).await;

    let (status, body) = call(
        state,
        "/v1/decisions",
        json!({
            "model": "typesafe/jev-1.14.0",
            "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "Doorbell camera"},
                {"type": "input_image", "image_url": "data:image/jpeg;base64,/9j/AAAA"}
            ]}],
            "questions": [
                {"type": "predicate", "name": "person", "instructions": "Is a person at the door?"},
                {"type": "score", "name": "mood", "instructions": "How busy is the scene?",
                 "levels": [{"label": "calm"}, {"label": "busy"}, {"label": "chaotic"}]}
            ],
            "prompt_cache_key": "doorbell"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["answers"][0],
        json!({"type": "predicate", "name": "person", "probability": 0.92})
    );
    assert_eq!(body["answers"][1]["score"], 1.6);
    assert_eq!(
        body["answers"][1]["probabilities"][2],
        json!({"value": 2, "label": "chaotic", "probability": 0.7})
    );
    assert_eq!(body["usage"]["total_tokens"], 906);

    let sent = typesafe.bodies();
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0].get("prompt_cache_key").is_none(),
        "OpenAI extras stay off System One"
    );
    let state = sent[0]["state"].as_array().unwrap();
    assert_eq!(state[0], "Doorbell camera");
    let placeholder = state[1].as_str().unwrap();
    assert_eq!(
        sent[0]["assets"][placeholder],
        json!({"type": "image", "format": "jpeg", "data_base64": "/9j/AAAA"})
    );
    assert_eq!(sent[0]["questions"]["person"]["type"], "noul");
    assert_eq!(
        typesafe.image_opt_in.lock().unwrap().as_slice(),
        [Some("true".to_string())]
    );
}

#[tokio::test]
async fn router_decisions_are_served_by_openai_through_translation() {
    let openai = Upstream::replying(json!({
        "model": "gpt-6-luna-2026-10-01",
        "answers": [
            {"type": "choice", "name": "team", "choice": "payments", "confidence": 0.8,
             "probabilities": [{"value": "frontend", "probability": 0.2},
                               {"value": "payments", "probability": 0.8}]},
            {"type": "refusal", "name": "urgent"}
        ],
        "usage": {"input_tokens": 50, "output_tokens": 2, "total_tokens": 52,
                  "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
                  "output_tokens_details": {"reasoning_tokens": 0}}
    }));
    let base = serve("/v1/decisions", openai.clone()).await;
    let state = state_with(&[("openai", format!("{base}/v1"))]).await;

    let (status, body) = call(
        state,
        "/v1/alpha/decisions",
        json!({
            "model": "openai/gpt-6-luna",
            "state": {"ticket": "Checkout is blank"},
            "questions": {
                "team": {"type": "choice", "instructions": "Owner?",
                         "criteria": {"frontend": "Browser", "payments": "Billing"}},
                "urgent": {"type": "noul", "instructions": "Urgent?"}
            },
            "provider": {"order": ["OpenAI"]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["model"], "openai/gpt-6-luna-2026-10-01");
    assert_eq!(body["provider"], "OpenAI");
    assert_eq!(body["answers"]["team"]["choice"], "payments");
    assert_eq!(body["answers"]["team"]["probabilities"]["payments"], 0.8);
    assert_eq!(body["answers"]["urgent"], json!({"type": "refusal"}));
    assert_eq!(body["usage"]["input_tokens"], 50);

    let sent = openai.bodies();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["model"], "gpt-6-luna");
    assert_eq!(sent[0]["input"], r#"{"ticket":"Checkout is blank"}"#);
    assert!(
        sent[0].get("provider").is_none(),
        "OpenRouter extras stay off OpenAI"
    );
    assert_eq!(sent[0]["questions"][0]["type"], "choice");
    assert_eq!(sent[0]["questions"][0]["name"], "team");
    assert_eq!(sent[0]["questions"][1]["type"], "predicate");
}

#[tokio::test]
async fn openrouter_serves_non_jev_decision_models_and_keeps_its_own_fields() {
    let openrouter = Upstream::replying(json!({
        "id": "gen-1",
        "model": "perplexity/pplx-decider-v1.2",
        "provider": "Perplexity",
        "answers": {"urgent": {"type": "noul", "noul": 0.4}},
        "usage": {"input_tokens": 10, "output_tokens": 1, "cost": 0.0000002}
    }));
    let base = serve("/api/alpha/decisions", openrouter.clone()).await;
    let state = state_with(&[("openrouter", format!("{base}/api/v1"))]).await;

    let (status, body) = call(
        state,
        "/v1/alpha/decisions",
        json!({
            "model": "openrouter/perplexity/pplx-decider-v1.2",
            "state": "Server down",
            "questions": {"urgent": {"type": "noul", "instructions": "Urgent?"}},
            "provider": {"order": ["Perplexity"]},
            "user": "team-ops"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["provider"], "Perplexity");
    let sent = openrouter.bodies();
    assert_eq!(sent[0]["model"], "perplexity/pplx-decider-v1.2");
    assert_eq!(sent[0]["provider"]["order"][0], "Perplexity");
    assert_eq!(sent[0]["user"], "team-ops");
}

#[tokio::test]
async fn perplexity_receives_images_inline_in_state() {
    let perplexity = Upstream::replying(json!({
        "model": "pplx-decider-v1.2",
        "answers": {"damaged": {"type": "noul", "noul": 0.1}},
        "usage": {"input_tokens": 300, "output_tokens": 1}
    }));
    let base = serve("/v1/decisions", perplexity.clone()).await;
    let state = state_with(&[("perplexity", base)]).await;

    let (status, body) = call(
        state,
        "/v1/alpha/decisions",
        json!({
            "model": "perplexity/pplx-decider-v1.2",
            "state": {"photo": "parcel-1", "carrier": "UPS"},
            "assets": {"parcel-1": {"type": "image", "format": "png", "data_base64": "iVBOR"}},
            "questions": {"damaged": {"type": "noul", "instructions": "Is the parcel damaged?"}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["model"], "perplexity/pplx-decider-v1.2");
    assert_eq!(
        perplexity.bodies()[0],
        json!({
            "model": "pplx-decider-v1.2",
            "state": {
                "photo": {"type": "image_url",
                          "image_url": {"url": "data:image/png;base64,iVBOR"}},
                "carrier": "UPS"
            },
            "questions": {"damaged": {"type": "noul", "instructions": "Is the parcel damaged?"}}
        })
    );
}

#[tokio::test]
async fn box_questions_only_reach_models_that_answer_them() {
    let typesafe = Upstream::replying(json!({
        "model": "jev-1.14.0",
        "answers": {"mug": {"type": "box", "confidence": 0.8,
                            "box": {"top": 0.1, "bottom": 0.5, "left": 0.2, "right": 0.6}}},
        "usage": {"input_tokens": 1200, "output_tokens": 4}
    }));
    let base = serve("/v1/systemone", typesafe.clone()).await;
    let body = |model: &str| {
        json!({
            "model": model,
            "state": "kitchen",
            "assets": {"kitchen": {"type": "image", "format": "png", "data_base64": "iVBOR"}},
            "questions": {"mug": {"type": "box", "instructions": "Find the mug"}}
        })
    };

    let state = state_with(&[("typesafe", format!("{base}/v1"))]).await;
    let (status, rejected) = call(
        state.clone(),
        "/v1/alpha/decisions",
        body("typesafe/jev-1.13.0"),
    )
    .await;
    assert!(status.is_client_error(), "{status}: {rejected}");
    assert!(typesafe.bodies().is_empty(), "rejected before dispatch");

    let (status, answered) = call(state, "/v1/alpha/decisions", body("typesafe/jev-1.14.0")).await;
    assert_eq!(status, StatusCode::OK, "{answered}");
    assert_eq!(answered["answers"]["mug"]["box"]["right"], 0.6);
    assert_eq!(typesafe.bodies()[0]["questions"]["mug"]["type"], "box");
    assert_eq!(
        typesafe.image_opt_in.lock().unwrap().as_slice(),
        [Some("true".to_string())]
    );
}

#[tokio::test]
async fn openai_format_requests_without_a_system_one_form_are_rejected() {
    let state = state_with(&[]).await;
    let (status, body) = call(
        state,
        "/v1/decisions",
        json!({
            "model": "typesafe/jev-1.14.0",
            "input": [{"role": "assistant", "content": "earlier answer"}],
            "questions": [{"type": "predicate", "instructions": "?"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("user messages"), "{body}");
}

/// Shaped like OpenRouter's real catalog: no `object` on the envelope or on
/// the models.
async fn openrouter_catalog(
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Json<Value> {
    Json(match query.as_deref() {
        // OpenRouter lists decision models only when asked for them.
        Some("output_modalities=decisions") => json!({"data": [{
            "id": "perplexity/pplx-decider-v1.2",
            "context_length": 262144,
            "architecture": {"input_modalities": ["text", "image"],
                             "output_modalities": ["decisions"]}
        }]}),
        _ => json!({"data": [{
            "id": "openai/gpt-4o",
            "context_length": 128000,
            "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]}
        }]}),
    })
}

#[tokio::test]
async fn openrouter_listing_includes_decision_models_with_catalog_capabilities() {
    use openai_server::capabilities::{InputModality, ModelSurface, Support};
    let base = crate::support::mock::serve(
        Router::new().route("/api/v1/models", axum::routing::get(openrouter_catalog)),
    )
    .await;
    let store = Arc::new(InMemoryProviderConnectionStore::new());
    store
        .create(ConnectionInput {
            name: "openrouter-primary".to_string(),
            provider_kind: "openrouter".to_string(),
            base_url: Some(format!("{base}/api/v1")),
            prefix: Some("openrouter/".to_string()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "openrouter-secret".to_string(),
        })
        .await
        .unwrap();
    let router = RouterBackend::new(
        reqwest::Client::new(),
        store,
        Arc::new(InMemoryModelRouteStore::new()),
    );
    let listing = router.list_models().await.unwrap();
    let ids: Vec<_> = listing.data.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "openrouter/openai/gpt-4o",
            "openrouter/perplexity/pplx-decider-v1.2"
        ]
    );
    let decider = listing.data[1].capabilities.as_ref().unwrap();
    assert_eq!(
        decider.surfaces[&ModelSurface::Decisions],
        Support::Supported
    );
    assert_eq!(decider.surfaces[&ModelSurface::Chat], Support::Unsupported);
    assert_eq!(
        decider.modalities.input[&InputModality::Image],
        Support::Supported
    );
    assert_eq!(decider.limits.context_window, Some(262_144));
}
