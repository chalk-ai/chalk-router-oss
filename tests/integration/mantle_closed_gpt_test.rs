//! Closed GPT models on Mantle: Chat and Messages use the `/openai/v1` Chat
//! dialect, and Responses is served natively by `/openai/v1/responses`.
use super::support::{
    app::{AppStateBuilder, TEST_API_KEY},
    mock::serve,
};
use axum::{
    http::{HeaderMap, StatusCode},
    middleware,
    routing::post,
    Json, Router,
};
use openai_server::{
    backend::RouterBackend,
    budget::{BudgetPolicy, MemoryBudgetPolicyStore},
    handlers,
    hashing::KeyHasher,
    middleware::auth_middleware,
    model_routes::InMemoryModelRouteStore,
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
    quota::MemoryQuota,
    quota_policy::QuotaPolicyResolver,
    ratelimit::{MemoryRateLimitPolicyStore, RateLimitPolicy, RateLimitPolicyStore},
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::test]
async fn closed_gpt_responses_are_native_while_chat_and_messages_use_chat_dialect() {
    let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
    let recorder = calls.clone();
    let responses_calls = Arc::new(Mutex::new(Vec::<Value>::new()));
    let responses_recorder = responses_calls.clone();
    let upstream = serve(Router::new()
        .route("/openai/v1/chat/completions", post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let recorder = recorder.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer mantle-surface-fixture-key");
                assert!(matches!(body["model"].as_str(), Some("openai.gpt-5.6-luna" | "openai.gpt-6-luna")));
                recorder.lock().await.push(body.clone());
                if body["stream"] == true {
                    let first = json!({"id":"mantle","object":"chat.completion.chunk","created":1,
                        "model":body["model"],"choices":[{"index":0,"delta":{"role":"assistant","content":"OK"},"finish_reason":null}]});
                    let last = json!({"id":"mantle","object":"chat.completion.chunk","created":1,
                        "model":body["model"],"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}});
                    axum::response::IntoResponse::into_response(([("content-type","text/event-stream")],
                        format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n")))
                } else {
                    axum::response::IntoResponse::into_response(Json(json!({"id":"mantle","object":"chat.completion","created":1,
                        "model":body["model"],"choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}})))
                }
            }
        }))
        .route("/openai/v1/responses", post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let recorder = responses_recorder.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer mantle-surface-fixture-key");
                assert!(matches!(body["model"].as_str(), Some("openai.gpt-5.6-luna" | "openai.gpt-6-luna")));
                recorder.lock().await.push(body.clone());
                let response = mantle_response(&body["model"]);
                if body["stream"] == true {
                    let created = json!({"type":"response.created","sequence_number":0,"response":{"id":"resp_mantle","object":"response","status":"in_progress","model":body["model"],"output":[]}});
                    let delta = json!({"type":"response.output_text.delta","sequence_number":1,"item_id":"msg_mantle","output_index":0,"content_index":0,"delta":"OK"});
                    let completed = json!({"type":"response.completed","sequence_number":2,"response":response});
                    axum::response::IntoResponse::into_response(([("content-type","text/event-stream")],
                        format!("event: response.created\ndata: {created}\n\nevent: response.output_text.delta\ndata: {delta}\n\nevent: response.completed\ndata: {completed}\n\n")))
                } else {
                    axum::response::IntoResponse::into_response(Json(response))
                }
            }
        }))
        .fallback(|| async { (StatusCode::BAD_REQUEST, "unexpected native surface or legacy dialect") }))
        .await;
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    connections
        .create(ConnectionInput {
            name: "fixture-mantle".into(),
            provider_kind: "bedrock".into(),
            // Explicit local override exercises the real router and handlers;
            // the unit fixtures independently verify AWS URL inference/selection.
            base_url: Some(format!("{upstream}/openai/v1")),
            prefix: Some("mantle/".into()),
            exposure: ExposurePolicy::Unlisted,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "mantle-surface-fixture-key".into(),
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
    let app = serve(
        Router::new()
            .route(
                "/v1/chat/completions",
                post(handlers::chat::create_chat_completion),
            )
            .route("/v1/responses", post(handlers::responses::create_response))
            .route("/v1/messages", post(handlers::anthropic::proxy_messages))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state),
    )
    .await;
    let client = reqwest::Client::new();
    for model in ["mantle/openai.gpt-5.6-luna", "mantle/openai.gpt-6-luna"] {
        for stream in [false, true] {
            for (path, body) in [
                (
                    "/v1/chat/completions",
                    json!({"model":model,"messages":[{"role":"user","content":"Reply OK"}],"max_completion_tokens":64,"stream":stream}),
                ),
                (
                    "/v1/responses",
                    json!({"model":model,"input":"Reply OK","max_output_tokens":64,"stream":stream}),
                ),
                (
                    "/v1/messages",
                    json!({"model":model,"messages":[{"role":"user","content":"Reply OK"}],"max_tokens":64,"stream":stream}),
                ),
            ] {
                let response = client
                    .post(format!("{app}{path}"))
                    .bearer_auth(TEST_API_KEY)
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                let content_type = response.headers()["content-type"]
                    .to_str()
                    .unwrap()
                    .to_owned();
                let body = response.text().await.unwrap();
                assert_eq!(status, 200, "{model} {path} stream={stream}: {body}");
                if stream {
                    assert!(content_type.starts_with("text/event-stream"));
                    assert!(body.contains("OK"), "{body}");
                    match path {
                        "/v1/chat/completions" => assert!(body.contains("data: [DONE]")),
                        "/v1/responses" => assert!(body.contains("response.completed")),
                        "/v1/messages" => assert!(body.contains("message_stop")),
                        _ => unreachable!(),
                    }
                } else {
                    let body: Value = serde_json::from_str(&body).unwrap();
                    let text = match path {
                        "/v1/chat/completions" => &body["choices"][0]["message"]["content"],
                        "/v1/responses" => &body["output"][0]["content"][0]["text"],
                        "/v1/messages" => &body["content"][0]["text"],
                        _ => unreachable!(),
                    };
                    assert_eq!(text, "OK");
                }
            }
        }
    }
    let calls = calls.lock().await;
    assert_eq!(
        calls.len(),
        8,
        "each Chat and Messages request makes one Chat invocation"
    );
    for body in calls.iter() {
        assert_eq!(body["messages"][0]["content"], "Reply OK");
        assert_eq!(body["max_completion_tokens"], 64);
    }
    let responses_calls = responses_calls.lock().await;
    assert_eq!(
        responses_calls.len(),
        4,
        "each Responses request makes one native Responses invocation"
    );
    for body in responses_calls.iter() {
        assert_eq!(body["input"], "Reply OK");
        assert_eq!(body["max_output_tokens"], 64);
    }
}

fn mantle_response(model: &Value) -> Value {
    json!({
        "id": "resp_mantle",
        "object": "response",
        "created_at": 1,
        "status": "completed",
        "model": model,
        "output": [{
            "id": "msg_mantle",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "OK", "annotations": []}]
        }],
        "usage": {
            "input_tokens": 7,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 3,
            "output_tokens_details": {"reasoning_tokens": 1},
            "total_tokens": 10
        }
    })
}

/// The controls Chat translation had to refuse reach Mantle verbatim,
/// continuations return to the same endpoint, and background (which needs
/// ID-only retrieval the locator cannot route) is rejected before dispatch.
#[tokio::test]
async fn closed_gpt_native_responses_forward_reasoning_store_and_tools() {
    let calls = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    let recorder = calls.clone();
    let upstream = serve(
        Router::new()
            .route(
                "/openai/v1/responses",
                post(move |Json(body): Json<Value>| {
                    let recorder = recorder.clone();
                    async move {
                        recorder
                            .lock()
                            .await
                            .push(("/openai/v1/responses".into(), body.clone()));
                        Json(mantle_response(&body["model"]))
                    }
                }),
            )
            .fallback(|| async { (StatusCode::BAD_REQUEST, "unexpected path") }),
    )
    .await;
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    connections
        .create(ConnectionInput {
            name: "fixture-mantle".into(),
            provider_kind: "bedrock".into(),
            base_url: Some(format!("{upstream}/openai/v1")),
            prefix: Some("mantle/".into()),
            exposure: ExposurePolicy::Unlisted,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "mantle-surface-fixture-key".into(),
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
    let app = serve(
        Router::new()
            .route("/v1/responses", post(handlers::responses::create_response))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state),
    )
    .await;
    let client = reqwest::Client::new();
    let tool = json!({"type":"function","name":"lookup","parameters":{"type":"object","properties":{},"additionalProperties":false}});
    let forwarded = json!({
        "model": "mantle/openai.gpt-6-luna",
        "input": "Reply OK",
        "reasoning": {"effort": "high"},
        "store": true,
        "tools": [tool],
        "text": {"verbosity": "low"}
    });
    let response = client
        .post(format!("{app}/v1/responses"))
        .bearer_auth(TEST_API_KEY)
        .json(&forwarded)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    let router_id = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(
        router_id, "resp_mantle",
        "the router issues its own locator"
    );

    // The continuation names its model, so it returns to `/openai/v1` with
    // the upstream ID decoded from the router's locator.
    let response = client
        .post(format!("{app}/v1/responses"))
        .bearer_auth(TEST_API_KEY)
        .json(&json!({"model":"mantle/openai.gpt-6-luna","input":"again","previous_response_id":router_id}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 200, "{body}");

    // Background needs ID-only retrieval, which cannot be routed to the
    // endpoint that created the response.
    let response = client
        .post(format!("{app}/v1/responses"))
        .bearer_auth(TEST_API_KEY)
        .json(&json!({"model":"mantle/openai.gpt-6-luna","input":"later","background":true}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_client_error(), "background: {status} {body}");

    let calls = calls.lock().await;
    assert_eq!(calls.len(), 2, "background never reaches Mantle");
    assert_eq!(calls[1].0, "/openai/v1/responses");
    assert_eq!(calls[1].1["previous_response_id"], "resp_mantle");
    let (path, body) = &calls[0];
    assert_eq!(path, "/openai/v1/responses");
    assert_eq!(body["model"], "openai.gpt-6-luna");
    for field in ["reasoning", "store", "tools", "text", "input"] {
        assert_eq!(body[field], forwarded[field], "{field} forwarded verbatim");
    }
}

#[tokio::test]
async fn closed_gpt_soft_caps_settle_as_debt_and_unclassified_tools_require_no_token_policy() {
    for policy in ["none", "budget", "tpm", "rpm", "concurrency"] {
        let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
        let recorder = calls.clone();
        let upstream = serve(Router::new().route(
            "/openai/v1/responses",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let recorder = recorder.clone();
                async move {
                    assert_eq!(headers["authorization"], "Bearer mantle-quota-fixture-key");
                    assert_eq!(body["model"], "openai.gpt-6-luna");
                    recorder.lock().await.push(body.clone());
                    let mut response = mantle_response(&body["model"]);
                    if body["tools"][0]["type"] == "web_search" {
                        // Hosted search adds context absent from the wire prompt.
                        response["usage"]["input_tokens"] = json!(8000);
                        response["usage"]["total_tokens"] = json!(8003);
                    }
                    if body["metadata"]["omit_usage"] == "true" {
                        response.as_object_mut().unwrap().remove("usage");
                    }
                    if body["stream"] == true {
                        let completed = json!({"type":"response.completed","sequence_number":0,"response":response});
                        axum::response::IntoResponse::into_response((
                            [("content-type", "text/event-stream")],
                            format!("event: response.completed\ndata: {completed}\n\n"),
                        ))
                    } else {
                        axum::response::IntoResponse::into_response(Json(response))
                    }
                }
            }),
        ))
        .await;
        let connections = Arc::new(InMemoryProviderConnectionStore::new());
        let connection = connections
            .create(ConnectionInput {
                name: "fixture-mantle".into(),
                provider_kind: "bedrock".into(),
                base_url: Some(format!("{upstream}/openai/v1")),
                prefix: Some("mantle/".into()),
                exposure: ExposurePolicy::Unlisted,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "mantle-quota-fixture-key".into(),
            })
            .await
            .unwrap();
        let routes = Arc::new(InMemoryModelRouteStore::new());
        let backend = Arc::new(RouterBackend::new(
            reqwest::Client::new(),
            connections.clone(),
            routes.clone(),
        ));
        let budgets = Arc::new(MemoryBudgetPolicyStore::new("test".into()));
        let rates = Arc::new(MemoryRateLimitPolicyStore::new("test".into()));
        let key_hash = KeyHasher::new(b"test-pepper".to_vec()).hash(TEST_API_KEY);
        if policy == "budget" {
            budgets
                .insert(BudgetPolicy {
                    quota_policy_id: "mantle-token-budget".into(),
                    id: "mantle-token-budget-source".into(),
                    environment_id: "test".into(),
                    scope: "api_key".into(),
                    scope_id: key_hash.clone(),
                    period: "daily".into(),
                    window_type: "calendar".into(),
                    window_size: sqlx::postgres::types::PgInterval {
                        months: 0,
                        days: 1,
                        microseconds: 0,
                    },
                    anchor: None,
                    timezone: "UTC".into(),
                    limit_kind: "tokens".into(),
                    limit_amount: 512,
                    enabled: true,
                    created_at: chrono::Utc::now(),
                })
                .unwrap();
        }
        if let Some(limit_type) = match policy {
            "tpm" => Some("tokens_per_minute"),
            "rpm" => Some("requests_per_minute"),
            "concurrency" => Some("concurrent_requests"),
            _ => None,
        } {
            rates
                .create(RateLimitPolicy {
                    quota_policy_id: "mantle-rate-limit".into(),
                    id: "mantle-rate-limit-source".into(),
                    limit_type: limit_type.into(),
                    limit_value: if policy == "concurrency" { 1 } else { 100_000 },
                    burst_value: None,
                    connection_id: None,
                    models: vec![],
                    scope: "api_key".into(),
                    scope_id: key_hash,
                    enabled: true,
                    created_at: chrono::Utc::now(),
                })
                .await
                .unwrap();
        }
        let state = AppStateBuilder::new(backend)
            .provider_connection_store(connections)
            .model_route_store(routes)
            .quota_resolver(Arc::new(QuotaPolicyResolver::new(
                "test".into(),
                budgets.clone(),
                rates.clone(),
            )))
            .budget_store(budgets)
            .rate_limit_store(rates)
            .quota_backend(Arc::new(MemoryQuota::new()))
            .build()
            .await;
        let previous_response_id =
            state
                .response_locator
                .encode(&connection.id, None, "resp_mantle");
        let app = serve(
            Router::new()
                .route("/v1/responses", post(handlers::responses::create_response))
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    auth_middleware,
                ))
                .with_state(state),
        )
        .await;
        let client = reqwest::Client::new();
        // The 512-token budget admits the first hosted request on a clamped
        // reserve, charges all 8,003 tokens as debt, and then rejects every
        // bounded request until the window resets.
        let mut budget_in_debt = false;
        for streaming in [false, true] {
            for (patch, unknown_tokens) in [
                (json!({"tools":[{"type":"web_search"}]}), false),
                // A stored-history continuation is a soft cap: admitted until
                // the hosted request's debt exhausts the budget.
                (json!({"previous_response_id":previous_response_id}), false),
                (json!({"tools":[{"type":"future_hosted_tool"}]}), true),
                (
                    json!({"tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{}}}]}),
                    false,
                ),
                (
                    json!({"tools":[{"type":"custom","name":"lookup","format":{"type":"text"}}]}),
                    false,
                ),
            ] {
                let mut request = json!({"model":"mantle/openai.gpt-6-luna","input":"Reply OK",
                    "max_output_tokens":32,"stream":streaming});
                request
                    .as_object_mut()
                    .unwrap()
                    .extend(patch.as_object().unwrap().clone());
                let before = calls.lock().await.len();
                let response = client
                    .post(format!("{app}/v1/responses"))
                    .bearer_auth(TEST_API_KEY)
                    .json(&request)
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                let body = response.text().await.unwrap();
                let unknown_rejected = unknown_tokens && matches!(policy, "budget" | "tpm");
                let expected = if unknown_rejected {
                    400
                } else if budget_in_debt {
                    429
                } else {
                    200
                };
                let rejected = expected != 200;
                assert_eq!(
                    status, expected,
                    "policy={policy}, stream={streaming}, request={request}: {body}"
                );
                assert_eq!(
                    calls.lock().await.len(),
                    before + usize::from(!rejected),
                    "token rejection must precede native dispatch"
                );
                if unknown_rejected {
                    assert!(body.contains("conservative token estimate"));
                } else if !rejected && request["tools"][0]["type"] == "web_search" {
                    assert!(
                        body.contains("8003"),
                        "authoritative hosted-tool usage is preserved"
                    );
                    budget_in_debt |= policy == "budget";
                }
            }
        }
        if !matches!(policy, "budget" | "tpm") {
            for streaming in [false, true] {
                // Hosted input cannot be reconstructed when authoritative
                // usage is missing; caller-run functions retain estimation.
                // The second call also proves a concurrency lease is released
                // after the preceding hosted-tool accounting error.
                for hosted in [true, false] {
                    let tool = if hosted {
                        json!({"type":"web_search"})
                    } else {
                        json!({"type":"function","name":"lookup","parameters":{"type":"object","properties":{}}})
                    };
                    let response = client
                        .post(format!("{app}/v1/responses"))
                        .bearer_auth(TEST_API_KEY)
                        .json(
                            &json!({"model":"mantle/openai.gpt-6-luna","input":"Reply OK",
                            "stream":streaming,"max_output_tokens":32,"tools":[tool],
                            "metadata":{"omit_usage":"true"}}),
                        )
                        .send()
                        .await
                        .unwrap();
                    let status = response.status();
                    let body = response.text().await.unwrap();
                    assert_eq!(
                        status,
                        if hosted && !streaming { 503 } else { 200 },
                        "policy={policy}, stream={streaming}, hosted={hosted}: {body}"
                    );
                    assert_eq!(body.contains("usage_accounting_unavailable"), hosted, "provider-hosted context must never be replaced by a wire-prompt estimate: {body}");
                    if hosted && streaming {
                        assert!(body.contains("event: error"));
                        assert!(!body.contains("event: response.completed"));
                    }
                }
            }
        }
    }
}
