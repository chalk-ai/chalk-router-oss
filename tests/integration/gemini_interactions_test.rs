//! Exercise the native Gemini surface through real router middleware and HTTP.
use super::support::{
    app::{AppStateBuilder, TEST_API_KEY},
    mock::serve,
};
use async_trait::async_trait;
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use futures::{StreamExt, TryStreamExt};
use openai_server::{
    api_keys::{ApiKey, ApiKeyPermission, ApiKeyStore, ApiKeyStoreError, CreatedApiKey},
    backend::RouterBackend,
    fallback::FallbackPolicy,
    model_routes::InMemoryModelRouteStore,
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
    service::pagination::StablePageCursor,
    AppState,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::Notify;

const MODEL: &str = "google/models/gemini-2.5-flash";
const BACKUP: &str = "backup/gemini-2.5-flash";

struct RecordingKeys {
    inner: Arc<dyn ApiKeyStore>,
    usage: Mutex<Vec<(u64, u64, u64)>>,
}
#[async_trait]
impl ApiKeyStore for RecordingKeys {
    async fn insert_raw(&self, key: ApiKey) -> Result<(), ApiKeyStoreError> {
        self.inner.insert_raw(key).await
    }
    async fn create(
        &self,
        description: String,
        permission: ApiKeyPermission,
        labels: HashMap<String, String>,
        pool: Option<String>,
    ) -> Result<CreatedApiKey, ApiKeyStoreError> {
        self.inner
            .create(description, permission, labels, pool)
            .await
    }
    async fn update(
        &self,
        hash: &str,
        description: Option<String>,
        pool: Option<Option<String>>,
        permission: Option<ApiKeyPermission>,
    ) -> Result<Option<Arc<ApiKey>>, ApiKeyStoreError> {
        self.inner.update(hash, description, pool, permission).await
    }
    async fn authenticate(&self, token: &str) -> Option<Arc<ApiKey>> {
        self.inner.authenticate(token).await
    }
    async fn get(&self, hash: &str) -> Option<Arc<ApiKey>> {
        self.inner.get(hash).await
    }
    async fn list(&self) -> Vec<Arc<ApiKey>> {
        self.inner.list().await
    }
    async fn list_page(
        &self,
        cursor: Option<&StablePageCursor>,
        fetch: i64,
    ) -> Result<Vec<Arc<ApiKey>>, ApiKeyStoreError> {
        self.inner.list_page(cursor, fetch).await
    }
    async fn select_by_labels(
        &self,
        labels: &HashMap<String, String>,
        required: Option<&str>,
        hashes: &[String],
        fetch: i64,
    ) -> Result<Vec<Arc<ApiKey>>, ApiKeyStoreError> {
        self.inner
            .select_by_labels(labels, required, hashes, fetch)
            .await
    }
    async fn revoke(&self, hash: &str) -> Result<bool, ApiKeyStoreError> {
        self.inner.revoke(hash).await
    }
    async fn record_usage(&self, _: &str, input: u64, output: u64, total: u64) {
        self.usage.lock().unwrap().push((input, output, total));
    }
}

async fn state(
    primary: &str,
    backup: Option<&str>,
    kind: &str,
    declared: Option<Value>,
) -> (AppState, Arc<RecordingKeys>) {
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    for (prefix, url) in
        std::iter::once(("google", primary)).chain(backup.map(|url| ("backup", url)))
    {
        connections
            .create(ConnectionInput {
                name: prefix.into(),
                provider_kind: kind.into(),
                base_url: Some(format!("{url}/v1beta/openai")),
                prefix: Some(format!("{prefix}/")),
                exposure: ExposurePolicy::Unlisted,
                routing_enabled: true,
                declared_capabilities: declared.clone().map(|v| serde_json::from_value(v).unwrap()),
                api_key: "saved-google-key".into(),
            })
            .await
            .unwrap();
    }
    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections,
        Arc::new(InMemoryModelRouteStore::new()),
    ));
    let mut state = AppStateBuilder::new(backend)
        .fallback_policy(FallbackPolicy::from_map(HashMap::from([(
            MODEL.into(),
            vec![BACKUP.into()],
        )])))
        .build()
        .await;
    let keys = Arc::new(RecordingKeys {
        inner: state.api_key_store.clone(),
        usage: Mutex::new(Vec::new()),
    });
    state.api_key_store = keys.clone();
    (state, keys)
}

fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/v1beta/interactions",
            post(openai_server::handlers::gemini::create_interaction),
        )
        .route(
            "/v1beta/interactions/:id",
            axum::routing::get(openai_server::handlers::gemini::unsupported_interaction_operation)
                .delete(openai_server::handlers::gemini::unsupported_interaction_operation),
        )
        .route(
            "/v1beta/interactions/:id/cancel",
            post(openai_server::handlers::gemini::unsupported_interaction_operation),
        )
        .route(
            "/v1/chat/completions",
            post(openai_server::handlers::chat::create_chat_completion),
        )
        .route(
            "/v1/responses",
            post(openai_server::handlers::responses::create_response),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            openai_server::middleware::auth_middleware,
        ))
        .with_state(state)
}
async fn app(state: AppState) -> String {
    serve(router(state)).await
}
fn request(base: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .post(format!("{base}/v1beta/interactions"))
        .header("x-goog-api-key", TEST_API_KEY)
}
fn usage() -> Value {
    json!({"total_input_tokens":10,"total_output_tokens":2,"total_thought_tokens":3,"total_tool_use_tokens":1,"total_cached_tokens":4,"total_tokens":16})
}
fn result() -> Value {
    json!({"id":"native-id","model":"gemini-2.5-flash","status":"completed",
        "steps":[{"type":"thought","signature":"opaque-signed-history"},
        {"type":"function_call","id":"call-1","name":"weather","arguments":{"city":"Paris"}}],
        "usage":usage(),"future_extension":{"nested":[3,2,1]}})
}
fn terminal(value: &Value) -> String {
    format!(
        "id: final-id\nevent: interaction.completed\ndata: {}\n\n",
        json!({"event_type":"interaction.completed","interaction":value})
    )
}
fn reply(streaming: bool) -> Response {
    if streaming {
        ([("content-type", "text/event-stream")], terminal(&result())).into_response()
    } else {
        Json(result()).into_response()
    }
}

#[tokio::test]
async fn gemini_native_signed_tool_round_trip_json_and_streaming() {
    for streaming in [false, true] {
        let finish = Arc::new(Notify::new());
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let saved = captured.clone();
        let release = finish.clone();
        let upstream = serve(Router::new().route("/v1beta/interactions",post(move |headers: HeaderMap, Json(body):Json<Value>| {
            let release = release.clone();
            saved.lock().unwrap().push(body);
            async move {
                assert_eq!(headers["x-goog-api-key"],"saved-google-key");
                assert_eq!(headers["x-goog-api-client"],"test-native-client");
                assert!(!headers.contains_key("authorization"));
                assert!(!headers.contains_key("x-goog-user-project"));
                if !streaming { return Json(result()).into_response(); }
                let first = "id: signed-event\n: keepalive\nevent: step.start\ndata: {\"event_type\":\"step.start\",\"index\":0,\"step\":{\"type\":\"thought\",\"signature\":\"opaque-signed-history\",\"text\":\"héllo\"}}\n\n";
                let bytes=Bytes::copy_from_slice(first.as_bytes());
                let split=first.find('é').unwrap()+1;
                let source=futures::stream::iter([Ok::<_,std::convert::Infallible>(bytes.slice(..split)),Ok(bytes.slice(split..))])
                    .chain(futures::stream::once(async move { release.notified().await; Ok(Bytes::from(terminal(&result()))) }));
                Response::builder().header("content-type","text/event-stream").body(Body::from_stream(source)).unwrap()
            }
        }))).await;
        let (state, keys) = state(&upstream, None, "gemini", None).await;
        let base = app(state).await;
        let first_input =
            json!({"type":"user_input","content":[{"type":"text","text":"Weather?"}]});
        let body = json!({"model":MODEL,"input":[first_input],"store":false,"stream":streaming,
            "generation_config":{"thinking_level":"high","thinking_summaries":"auto"},
            "tools":[{"type":"function","name":"weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}],
            "future_control":{"untouched":true}});
        let response = request(&base)
            .header("x-goog-api-client", "test-native-client")
            .header("x-goog-user-project", "do-not-forward")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let received = if streaming {
            let mut stream = response.bytes_stream();
            let mut received = Vec::new();
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while !String::from_utf8_lossy(&received).contains("opaque-signed-history") {
                    received.extend_from_slice(&stream.next().await.unwrap().unwrap());
                }
            })
            .await
            .expect("signature must arrive before the upstream completes");
            assert!(!String::from_utf8_lossy(&received).contains("interaction.completed"));
            finish.notify_one();
            for bytes in stream.try_collect::<Vec<_>>().await.unwrap() {
                received.extend_from_slice(&bytes);
            }
            let text = String::from_utf8(received).unwrap();
            assert!(text.contains("id: signed-event\n: keepalive\n"));
            assert!(text.contains("héllo"));
            assert!(text.ends_with(&terminal(&result())));
            result()
        } else {
            let text = response.text().await.unwrap();
            assert_eq!(text, result().to_string());
            serde_json::from_str(&text).unwrap()
        };
        assert_eq!(*keys.usage.lock().unwrap(), vec![(11, 5, 16)]);
        // The client executes the tool and replays every original signed step.
        let mut replay = body.clone();
        let mut history = vec![first_input];
        history.extend(received["steps"].as_array().unwrap().clone());
        history.push(json!({"type":"function_result","call_id":"call-1","name":"weather","result":{"temperature":20}}));
        replay["input"] = Value::Array(history);
        let response = request(&base)
            .header("x-goog-api-client", "test-native-client")
            .json(&replay)
            .send()
            .await
            .unwrap();
        if streaming {
            finish.notify_one();
        }
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
        let captured = captured.lock().unwrap();
        let mut expected = body;
        expected["model"] = json!("gemini-2.5-flash");
        assert_eq!(captured[0], expected);
        replay["model"] = json!("gemini-2.5-flash");
        assert_eq!(captured[1], replay);
        assert_eq!(keys.usage.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn gemini_native_fallback_is_limited_to_fresh_stateless_requests() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let backup = serve(Router::new().route(
        "/v1beta/interactions",
        post(move |Json(body): Json<Value>| {
            counted.fetch_add(1, Ordering::SeqCst);
            async move { reply(body["stream"] == true) }
        }),
    ))
    .await;
    let primary = serve(Router::new().route(
        "/v1beta/interactions",
        post(|| async {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [("content-type", "application/json"), ("retry-after", "3")],
                "{\"error\": {\"code\":\"unavailable\",\"extra\":123}}",
            )
        }),
    ))
    .await;
    let (state, keys) = state(&primary, Some(&backup), "gemini", None).await;
    let base = app(state).await;
    for streaming in [false, true] {
        let response = request(&base)
            .json(&json!({"model":MODEL,"store":false,"input":"hi","stream":streaming}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-fallback-model"], BACKUP);
        response.bytes().await.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(keys.usage.lock().unwrap().len(), 2);
    for extra in [
        json!({}),
        json!({"store":true}),
        json!({"store":false,"previous_interaction_id":"opaque-session"}),
        json!({"store":false,"input":[{"type":"thought","signature":"opaque"}]}),
        json!({"store":false,"input":[{"type":"function_result","call_id":"call-1","result":"ok"}]}),
    ] {
        let mut body = json!({"model":MODEL,"input":"hi"});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let response = request(&base).json(&body).send().await.unwrap();
        assert_eq!(response.status(), 503);
        assert_eq!(response.headers()["retry-after"], "3");
        assert_eq!(
            response.text().await.unwrap(),
            "{\"error\": {\"code\":\"unavailable\",\"extra\":123}}"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn gemini_native_terminal_http_errors_keep_original_body_and_headers() {
    for status in [400, 401, 429, 503] {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let backup = serve(Router::new().route(
            "/v1beta/interactions",
            post(move || {
                count.fetch_add(1, Ordering::SeqCst);
                async { Json(result()) }
            }),
        ))
        .await;
        let upstream = serve(Router::new().route(
            "/v1beta/interactions",
            post(move || async move {
                (
                    StatusCode::from_u16(status).unwrap(),
                    [
                        ("content-type", "application/problem+json"),
                        ("retry-after", "4"),
                    ],
                    "{ \"error\": {\"code\":\"test\",\"detail\":[1,2]} }",
                )
            }),
        ))
        .await;
        let (state, keys) = state(
            &upstream,
            (status < 429).then_some(backup.as_str()),
            "gemini",
            None,
        )
        .await;
        let base = app(state).await;
        let response = request(&base)
            .json(&json!({"model":MODEL,"input":"hi","store":false}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(
            response.headers()["content-type"],
            "application/problem+json"
        );
        assert_eq!(response.headers()["retry-after"], "4");
        assert_eq!(
            response.text().await.unwrap(),
            "{ \"error\": {\"code\":\"test\",\"detail\":[1,2]} }"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(keys.usage.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn gemini_native_auth_permissions_capabilities_and_unsupported_operations_fail_locally() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let upstream = serve(Router::new().route(
        "/v1beta/interactions",
        post(move || {
            count.fetch_add(1, Ordering::SeqCst);
            async { Json(result()) }
        }),
    ))
    .await;
    let (state, _) = state(&upstream, None, "gemini", None).await;
    let keys = state.api_key_store.clone();
    let base = app(state).await;
    let body = json!({"model":MODEL,"store":false,"input":"hi"});
    assert_eq!(
        reqwest::Client::new()
            .post(format!("{base}/v1beta/interactions"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    for value in [
        json!({"model":MODEL,"input":"hi","background":true}),
        json!({"model":MODEL,"input":"hi","agent":"deep-research"}),
        json!({"agent":"deep-research","input":"hi","stream":false}),
        json!({"agent":"deep-research","input":"hi","stream":true}),
    ] {
        // Agent-only requests must still authenticate, including when they
        // ask for a streaming response from an unsupported operation.
        assert_eq!(
            reqwest::Client::new()
                .post(format!("{base}/v1beta/interactions"))
                .json(&value)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let response = request(&base).json(&value).send().await.unwrap();
        assert_eq!(response.status(), 501);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["type"],
            "not_implemented"
        );
    }
    for value in [
        json!({"input":"hi"}),
        json!({"model":"","input":"hi"}),
        json!({"agent":"","input":"hi"}),
        json!({"agent":null,"input":"hi"}),
    ] {
        assert_eq!(
            request(&base).json(&value).send().await.unwrap().status(),
            400
        );
    }
    for path in [
        "/v1beta/interactions/native-id",
        "/v1beta/interactions/native-id/cancel",
    ] {
        let client = reqwest::Client::new();
        let request = if path.ends_with("/cancel") {
            client.post(format!("{base}{path}"))
        } else {
            client.get(format!("{base}{path}"))
        };
        assert_eq!(
            request
                .header("x-goog-api-key", TEST_API_KEY)
                .send()
                .await
                .unwrap()
                .status(),
            501
        );
    }
    for model in [
        "google/imagen-4.0-generate-001",
        "google/models/imagen-4.0-generate-001",
    ] {
        assert_eq!(
            request(&base)
                .json(&json!({"model":model,"store":false,"input":"Draw a panda"}))
                .send()
                .await
                .unwrap()
                .status(),
            400,
            "{model}",
        );
    }
    let key = keys.authenticate(TEST_API_KEY).await.unwrap();
    keys.update(
        &key.key_hash,
        None,
        None,
        Some(ApiKeyPermission {
            provider: Some("anthropic".into()),
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        request(&base).json(&body).send().await.unwrap().status(),
        403
    );
    for (kind, declared) in [
        ("openai", None),
        (
            "gemini",
            Some(
                json!({"schema_version":1,"endpoint":{"interfaces":{"gemini_interactions":{"operations":{"create":{"support":"unsupported"}}}}}}),
            ),
        ),
    ] {
        let (state, _) = self::state(&upstream, None, kind, declared).await;
        let base = app(state).await;
        assert_eq!(
            request(&base).json(&body).send().await.unwrap().status(),
            400
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn gemini_native_stream_errors_and_unknown_events_are_preserved() {
    for ending in ["event: error\ndata: {\"event_type\":\"error\",\"error\":{\"code\":\"native_error\",\"message\":\"unchanged\"}}\n\n", ""] {
        let payload=format!("id: extension\nevent: future.event\ndata: {{\"signature\":\"opaque\"}}\n\n{ending}");
        let sent=payload.clone();
        let upstream=serve(Router::new().route("/v1beta/interactions",post(move ||{let bytes=sent.clone();async { ([("content-type","text/event-stream")],bytes) }}))).await;
        let (state,keys)=state(&upstream,None,"gemini",None).await;
        let base=app(state).await;
        let response=request(&base).json(&json!({"model":MODEL,"store":false,"input":"hi","stream":true})).send().await.unwrap();
        let text=response.text().await.unwrap();
        if ending.is_empty() { assert!(text.starts_with(&payload));assert!(text.contains("incomplete_stream")); }
        else { assert_eq!(text,payload); }
        assert!(!text.contains("interaction.completed"));
        assert!(keys.usage.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn gemini_native_missing_usage_fails_before_success_terminal() {
    for streaming in [false, true] {
        let upstream = serve(Router::new().route(
            "/v1beta/interactions",
            post(move || async move {
                let value = json!({"status":"completed","steps":[]});
                if streaming {
                    ([("content-type", "text/event-stream")], terminal(&value)).into_response()
                } else {
                    Json(value).into_response()
                }
            }),
        ))
        .await;
        let (state, keys) = state(&upstream, None, "gemini", None).await;
        let base = app(state).await;
        let response = request(&base)
            .json(&json!({"model":MODEL,"store":false,"input":"hi","stream":streaming}))
            .send()
            .await
            .unwrap();
        if !streaming {
            assert_eq!(response.status(), 503);
        }
        let text = response.text().await.unwrap();
        assert!(text.contains("usage_accounting_unavailable"));
        assert!(!text.contains("interaction.completed"));
        assert!(keys.usage.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn gemini_openai_clients_still_use_google_compatibility() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let upstream=serve(Router::new().route("/v1beta/openai/chat/completions",post(move |Json(body):Json<Value>| {count.fetch_add(1,Ordering::SeqCst);async move {
        assert!(body.get("messages").is_some());
        Json(json!({"id":"chat-1","object":"chat.completion","created":1,"model":"gemini-2.5-flash","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}))
    }}))).await;
    let (state, _) = state(&upstream, None, "gemini", None).await;
    let base = app(state).await;
    for (path, body) in [
        (
            "chat/completions",
            json!({"model":MODEL,"messages":[{"role":"user","content":"hi"}]}),
        ),
        (
            "responses",
            json!({"model":MODEL,"input":"hi","store":false}),
        ),
    ] {
        let response = reqwest::Client::new()
            .post(format!("{base}/v1/{path}"))
            .bearer_auth(TEST_API_KEY)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn gemini_native_referenced_history_requires_absence_of_matching_token_limits() {
    use openai_server::{
        budget::ApiKeyBudgetInput,
        quota::MemoryQuota,
        quota_policy::QuotaPolicyResolver,
        ratelimit::{MemoryRateLimitPolicyStore, RateLimitPolicy, RateLimitPolicyStore},
    };

    for policy in [
        "budget",
        "global_tpm",
        "route_tpm",
        "other_route_tpm",
        "rpm",
        "concurrency",
        "none",
    ] {
        let requires_estimate = matches!(policy, "budget" | "global_tpm" | "route_tpm");
        for streaming in [false, true] {
            let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
            let saved = captured.clone();
            let upstream = serve(Router::new().route(
                "/v1beta/interactions",
                post(move |Json(body): Json<Value>| {
                    saved.lock().unwrap().push(body);
                    async move { reply(streaming) }
                }),
            ))
            .await;
            let (mut state, keys) = state(&upstream, None, "gemini", None).await;
            let key = state
                .api_key_store
                .authenticate(TEST_API_KEY)
                .await
                .unwrap();
            let rates = Arc::new(MemoryRateLimitPolicyStore::new("test".into()));
            if policy == "budget" {
                state
                    .budget_store
                    .replace_key_budgets(
                        &key.key_hash,
                        vec![ApiKeyBudgetInput {
                            token_limit: 100_000,
                            period: "daily".into(),
                            enabled: true,
                        }],
                    )
                    .await
                    .unwrap();
            } else if policy != "none" {
                let connection_id = match policy {
                    "route_tpm" => {
                        state
                            .backend
                            .resolve(MODEL)
                            .await
                            .unwrap()
                            .resolution
                            .connection_id
                    }
                    "other_route_tpm" => Some("other-connection".into()),
                    _ => None,
                };
                rates
                    .create(RateLimitPolicy {
                        quota_policy_id: "gemini-tpm".into(),
                        id: "gemini-tpm-source".into(),
                        limit_type: match policy {
                            "rpm" => "requests_per_minute",
                            "concurrency" => "concurrent_requests",
                            _ => "tokens_per_minute",
                        }
                        .into(),
                        limit_value: if policy == "concurrency" { 1 } else { 100_000 },
                        burst_value: None,
                        models: if connection_id.is_some() {
                            vec![MODEL.into()]
                        } else {
                            vec![]
                        },
                        connection_id,
                        scope: "api_key".into(),
                        scope_id: key.key_hash.clone(),
                        enabled: true,
                        created_at: chrono::Utc::now(),
                    })
                    .await
                    .unwrap();
            }
            state.rate_limit_store = rates.clone();
            state.quota_backend = Arc::new(MemoryQuota::new());
            state.quota_resolver = Arc::new(QuotaPolicyResolver::new(
                "test".into(),
                state.budget_store.clone(),
                rates,
            ));
            let base = app(state).await;
            let tool_result = json!({"type":"function_result","call_id":"call-1","name":"weather","result":{"temperature":20}});
            let mut history = vec![
                json!({"type":"user_input","content":[{"type":"text","text":"Weather in Paris?"}]}),
            ];
            history.extend(result()["steps"].as_array().unwrap().clone());
            history.push(tool_result.clone());
            let body = json!({"model":MODEL,"input":history,"store":false,"stream":streaming,
                "generation_config":{"max_output_tokens":100}});
            // Inline signed/tool history is still measurable. Null references
            // do not add provider-stored input.
            for patch in [
                json!({}),
                json!({"input":[tool_result]}),
                json!({"previous_interaction_id":null,"cached_content":null}),
                // Image output settings do not make text input unmeasurable.
                json!({"model":"google/models/gemini-3.1-flash-image","input":"Draw a panda","response_format":{"type":"image","delivery":"inline","aspect_ratio":"16:9","image_size":"2K"}}),
            ] {
                let mut value = body.clone();
                value
                    .as_object_mut()
                    .unwrap()
                    .extend(patch.as_object().unwrap().clone());
                let response = request(&base).json(&value).send().await.unwrap();
                assert_eq!(
                    response.status(),
                    200,
                    "policy={policy}, stream={streaming}"
                );
                response.bytes().await.unwrap();
                value["model"] = json!(value["model"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("google/models/")
                    .unwrap());
                assert_eq!(captured.lock().unwrap().last(), Some(&value));
            }
            for patch in [
                json!({"input":"Continue","previous_interaction_id":"stored-history"}),
                json!({"input":"Continue","cached_content":"cached-history"}),
                json!({"input":"Continue","cachedContent":"cached-history"}),
            ] {
                let mut value = body.clone();
                value
                    .as_object_mut()
                    .unwrap()
                    .extend(patch.as_object().unwrap().clone());
                let before = captured.lock().unwrap().len();
                let response = request(&base).json(&value).send().await.unwrap();
                assert_eq!(
                    response.status(),
                    if requires_estimate { 400 } else { 200 },
                    "policy={policy}, stream={streaming}, request={value}"
                );
                let bytes = response.bytes().await.unwrap();
                if requires_estimate {
                    assert!(
                        String::from_utf8_lossy(&bytes).contains("supply the full history inline")
                    );
                    assert_eq!(captured.lock().unwrap().len(), before);
                } else {
                    value["model"] = json!("gemini-2.5-flash");
                    assert_eq!(captured.lock().unwrap().last(), Some(&value));
                }
            }
            let allowed = if requires_estimate { 4 } else { 7 };
            assert_eq!(captured.lock().unwrap().len(), allowed);
            assert_eq!(*keys.usage.lock().unwrap(), vec![(11, 5, 16); allowed]);
            if !requires_estimate {
                continue;
            }
            // Unsupported media/opaque content still need an unknown
            // reservation, and known estimates still enforce the token cap.
            for (patch, status) in [
                (
                    json!({"input":[{"type":"image","uri":"https://example.com/image.png"}]}),
                    400,
                ),
                (
                    json!({"input":[{"type":"image","uri":"https://example.com/image.png"}],"response_format":{"type":"image"}}),
                    400,
                ),
                (
                    json!({"input":[{"type":"thought","encrypted_content":"opaque"}]}),
                    400,
                ),
                // Inline history without a stored reference is a known
                // estimate, so it fails the cap rather than the estimate.
                (
                    json!({"previous_interaction_id":null,"generation_config":{"max_output_tokens":200_000}}),
                    429,
                ),
            ] {
                let mut value = body.clone();
                value
                    .as_object_mut()
                    .unwrap()
                    .extend(patch.as_object().unwrap().clone());
                let response = request(&base).json(&value).send().await.unwrap();
                assert_eq!(
                    response.status(),
                    status,
                    "policy={policy}, stream={streaming}, request={value}"
                );
            }
            assert_eq!(captured.lock().unwrap().len(), allowed);
            assert_eq!(*keys.usage.lock().unwrap(), vec![(11, 5, 16); allowed]);
        }
    }
}

#[tokio::test]
async fn gemini_native_concurrency_is_enforced_and_released_on_completion_and_disconnect() {
    use openai_server::{
        quota::MemoryQuota,
        quota_policy::QuotaPolicyResolver,
        ratelimit::{MemoryRateLimitPolicyStore, RateLimitPolicy, RateLimitPolicyStore},
    };
    for disconnect in [false, true] {
        let finish = Arc::new(Notify::new());
        let release = finish.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let upstream=serve(Router::new().route("/v1beta/interactions",post(move |Json(body):Json<Value>| {
            let release=release.clone();count.fetch_add(1,Ordering::SeqCst);
            async move {
                if body["stream"]!=true {return Json(result()).into_response();}
                let first=Bytes::from_static(b"event: step.start\ndata: {\"event_type\":\"step.start\",\"step\":{\"type\":\"thought\",\"signature\":\"signed\"}}\n\n");
                let source=futures::stream::once(async {Ok::<_,std::convert::Infallible>(first)}).chain(futures::stream::once(async move {
                    release.notified().await;Ok(Bytes::from(terminal(&result())))
                }));
                Response::builder().header("content-type","text/event-stream").body(Body::from_stream(source)).unwrap()
            }
        }))).await;
        let (mut state, keys) = state(&upstream, None, "gemini", None).await;
        let rates = Arc::new(MemoryRateLimitPolicyStore::new("develop".into()));
        let key = state
            .api_key_store
            .authenticate(TEST_API_KEY)
            .await
            .unwrap();
        rates
            .create(RateLimitPolicy {
                quota_policy_id: "gemini-concurrency".into(),
                id: "gemini-concurrency-source".into(),
                limit_type: "concurrent_requests".into(),
                limit_value: 1,
                burst_value: None,
                connection_id: None,
                models: vec![],
                scope: "api_key".into(),
                scope_id: key.key_hash.clone(),
                enabled: true,
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        state.rate_limit_store = rates.clone();
        state.quota_backend = Arc::new(MemoryQuota::new());
        state.quota_resolver = Arc::new(QuotaPolicyResolver::new(
            "develop".into(),
            state.budget_store.clone(),
            rates,
        ));
        let base = app(state).await;
        let mut stream = request(&base)
            .json(&json!({"model":MODEL,"input":"hi","store":false,"stream":true}))
            .send()
            .await
            .unwrap()
            .bytes_stream();
        stream.next().await.unwrap().unwrap();
        let body = json!({"model":MODEL,"input":"hi","store":false});
        assert_eq!(
            request(&base).json(&body).send().await.unwrap().status(),
            429
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        if disconnect {
            drop(stream);
        } else {
            finish.notify_one();
            stream.try_collect::<Vec<_>>().await.unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let response = request(&base).json(&body).send().await.unwrap();
                if response.status() == 200 {
                    response.bytes().await.unwrap();
                    break;
                }
                assert_eq!(response.status(), 429);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("stream completion or disconnect must release concurrency");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            keys.usage.lock().unwrap().len(),
            if disconnect { 1 } else { 2 }
        );
    }
}

#[tokio::test]
async fn gemini_native_fragmented_terminal_cannot_leak_success_before_usage_accounting() {
    let upstream = serve(Router::new().route(
        "/v1beta/interactions",
        post(|| async {
            let bytes = terminal(&json!({"status":"completed","steps":[]}));
            let chunks = bytes
                .as_bytes()
                .chunks(3)
                .map(Bytes::copy_from_slice)
                .collect::<Vec<_>>();
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(futures::stream::iter(
                    chunks.into_iter().map(Ok::<_, std::convert::Infallible>),
                )))
                .unwrap()
        }),
    ))
    .await;
    let (state, _) = state(&upstream, None, "gemini", None).await;
    let base = app(state).await;
    let text = request(&base)
        .json(&json!({"model":MODEL,"input":"hi","stream":true,"store":false}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.starts_with("event: error\ndata: "));
    assert!(!text.contains("interaction.completed"));
    let value: Value = serde_json::from_str(
        text.lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["error"]["code"], "usage_accounting_unavailable");
}

#[tokio::test]
async fn gemini_native_spans_record_usage_and_content_without_signatures() {
    use super::support::otel::{setup_otel, span_attrs};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let upstream = serve(Router::new().route(
        "/v1beta/interactions",
        post(|Json(body): Json<Value>| async move { reply(body["stream"] == true) }),
    ))
    .await;
    let (state, _) = state(&upstream, None, "gemini", None).await;
    let (provider, exporter, subscriber) = setup_otel();
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        for streaming in [false, true] {
            let app = router(state.clone());
            let request=axum::http::Request::builder().method("POST").uri("/v1beta/interactions")
                .header("content-type","application/json").header("x-goog-api-key",TEST_API_KEY)
                .body(Body::from(json!({"model":MODEL,"store":false,"stream":streaming,"input":[{"type":"thought","signature":"input-private"},{"type":"user_input","content":[{"type":"text","text":"Weather?"}]}]}).to_string())).unwrap();
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), 200);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            assert!(String::from_utf8_lossy(&bytes).contains("opaque-signed-history"));
        }
    }
    for result in provider.force_flush() {
        result.unwrap();
    }
    let spans = exporter.get_finished_spans().unwrap();
    let native = spans
        .iter()
        .filter(|span| span.name == "gemini.interactions")
        .collect::<Vec<_>>();
    assert_eq!(native.len(), 2);
    for span in native {
        let attrs = span_attrs(span);
        assert_eq!(attrs["openinference.span.kind"], "LLM");
        assert_eq!(attrs["llm.token_count.prompt"], "11");
        assert_eq!(attrs["llm.token_count.completion"], "5");
        assert_eq!(attrs["llm.token_count.total"], "16");
        assert!(attrs["input.value"].contains("Weather?"));
        assert!(attrs["output.value"].contains("weather"));
        assert!(!attrs.values().any(
            |value| value.contains("input-private") || value.contains("opaque-signed-history")
        ));
    }
    assert_eq!(
        openai_server::telemetry::frontend_for_path("/v1beta/interactions")
            .unwrap()
            .api,
        "gemini"
    );
}
