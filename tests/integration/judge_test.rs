//! Route-owned selection and fallback through all three conversation handlers.
use crate::support::app::AppStateBuilder;
use async_trait::async_trait;
use axum::{middleware, routing::post, Router};
use openai_server::{
    api_keys::{ApiKey, ApiKeyConnectionScope, ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore},
    backend::{BackendError, BackendProvider, ResolvedRoute, RouteResolution},
    hashing::KeyHasher,
    middleware::auth_middleware,
    route_grid::{
        Classification, CompletionJudgeBackend, DecisionsJudgeBackend, JudgeMode,
        JudgeOutcomeConfig, RouteGrid, RouteJudge, RouteJudgeBackend, RouteTarget,
    },
    types::{ChatCompletionRequest, ChatCompletionResponse, ListResponse, Model},
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
const KEY: &str = "sk-route-grid-test";
/// Route id `chalk/assistant` resolves through.
const ASSISTANT_ROUTE: &str = "route-assistant";

/// A key granted `chalk/assistant` and nothing else.
fn assistant_grant() -> ApiKeyPermission {
    ApiKeyPermission {
        route_ids: Some(vec![ASSISTANT_ROUTE.into()]),
        ..Default::default()
    }
}

#[derive(Default)]
struct Leaf {
    calls: Mutex<Vec<String>>,
    failures: Mutex<BTreeMap<String, u16>>,
    judge_reply: Mutex<String>,
    judge_calls: AtomicUsize,
    judge_delay_ms: AtomicUsize,
    judge_inputs: Mutex<Vec<Value>>,
    native_provider: Mutex<Option<&'static str>>,
    native_calls: Mutex<Vec<Value>>,
    stream_error: AtomicUsize,
}
#[async_trait]
impl BackendProvider for Leaf {
    async fn create_response(
        &self,
        req: openai_server::responses::ResponsesRequest,
        _: &openai_server::backend::provider::ResponsesRequestHeaders,
    ) -> Result<openai_server::backend::provider::BackendJsonResponse, BackendError> {
        self.record_native(req.raw())?;
        Ok(
            openai_server::backend::provider::BackendJsonResponse::from_value(
                response_body(req.model()),
                vec![],
            )?,
        )
    }

    async fn create_response_stream(
        &self,
        req: openai_server::responses::ResponsesRequest,
        _: &openai_server::backend::provider::ResponsesRequestHeaders,
    ) -> Result<openai_server::backend::provider::BackendSseResponse, BackendError> {
        self.record_native(req.raw())?;
        let event = if self.stream_error.load(Ordering::SeqCst) > 0 {
            json!({"type":"response.failed","response":{"status":"failed","error":{"message":"stream failed"}}})
        } else {
            json!({"type":"response.completed","response":response_body(req.model())})
        };
        let bytes = bytes::Bytes::from(format!("event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}}\n\ndata: {event}\n\n"));
        Ok(openai_server::backend::provider::BackendSseResponse {
            stream: Box::pin(futures::stream::once(async { Ok(bytes) })),
            headers: vec![],
            status: 200,
            content_type: "text/event-stream".into(),
        })
    }

    async fn send_anthropic_request(
        &self,
        _: openai_server::backend::AnthropicRequestEndpoint,
        _: &openai_server::backend::AnthropicRequestHeaders,
        body: bytes::Bytes,
    ) -> Result<reqwest::Response, BackendError> {
        let request: Value = serde_json::from_slice(&body).unwrap();
        self.record_native(&request)?;
        let model = request["model"].as_str().unwrap();
        let message = json!({"id":"msg-test","type":"message","role":"assistant","model":model,
            "content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn",
            "usage":{"input_tokens":2,"output_tokens":1}});
        let stream = request["stream"] == true;
        let (content_type, body) = if stream {
            let finish = if self.stream_error.load(Ordering::SeqCst) > 0 {
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"stream failed\"}}\n\n"
            } else {
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
            };
            ("text/event-stream", format!("event: message_start\ndata: {}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"ok\"}}}}\n\n{finish}", json!({"type":"message_start","message":message})))
        } else {
            ("application/json", message.to_string())
        };
        let url = crate::support::mock::serve(Router::new().route(
            "/",
            post(move || async move { ([("content-type", content_type)], body) }),
        ))
        .await;
        Ok(reqwest::Client::new().post(url).send().await?)
    }

    async fn send_decisions_request(
        &self,
        req: openai_server::decisions::DecisionsRequest,
    ) -> Result<reqwest::Response, BackendError> {
        self.judge_calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(req.model, "decisions-judge");
        assert!(req.questions.contains_key("classification"));
        tokio::time::sleep(Duration::from_millis(
            self.judge_delay_ms.load(Ordering::SeqCst) as u64,
        ))
        .await;
        let body = json!({"model":req.model,"answers":{"classification":{"type":"choice","choice":"conversation","confidence":0.99}},"usage":{"input_tokens":2,"output_tokens":1}});
        let url = crate::support::mock::serve(
            Router::new().route("/", post(move || async { axum::Json(body) })),
        )
        .await;
        Ok(reqwest::Client::new().post(url).send().await?)
    }

    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        Ok(ListResponse {
            object: "list".into(),
            data: vec![],
            first_id: None,
            last_id: None,
            has_more: false,
        })
    }
    async fn get_model(&self, _: &str) -> Result<Model, BackendError> {
        Err(BackendError::api_status(404, "model not found"))
    }

    fn name(&self) -> &str {
        "openai-compatible"
    }
    async fn create_chat_completion(
        &self,
        req: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        assert!(
            serde_json::to_value(&req)
                .unwrap()
                .get("fallbacks")
                .is_none(),
            "router options must not reach a provider"
        );
        let text = if req.model == "judge" {
            self.judge_calls.fetch_add(1, Ordering::SeqCst);
            self.judge_inputs.lock().unwrap().push(
                serde_json::from_str(req.messages[1].content.as_ref().unwrap().as_str().unwrap())
                    .unwrap(),
            );
            tokio::time::sleep(Duration::from_millis(
                self.judge_delay_ms.load(Ordering::SeqCst) as u64,
            ))
            .await;
            self.judge_reply.lock().unwrap().clone()
        } else {
            self.calls.lock().unwrap().push(req.model.clone());
            if let Some(status) = self.failures.lock().unwrap().get(&req.model).copied() {
                if status == 0 {
                    return Err(BackendError::InvalidResponse(
                        "unreadable provider response".into(),
                    ));
                }
                return Err(BackendError::api_status(status, "upstream failure"));
            }
            "ok".into()
        };
        Ok(serde_json::from_value(json!({"id":"test", "object":"chat.completion", "created":0, "model":req.model,
            "choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}})).unwrap())
    }
}
impl Leaf {
    fn record_native(&self, request: &Value) -> Result<(), BackendError> {
        let model = request["model"].as_str().unwrap();
        self.calls.lock().unwrap().push(model.into());
        self.native_calls.lock().unwrap().push(request.clone());
        if let Some(status) = self.failures.lock().unwrap().get(model) {
            return Err(if *status == 0 {
                BackendError::InvalidResponse("unreadable".into())
            } else {
                BackendError::api_status(*status, "upstream failure")
            });
        }
        Ok(())
    }
}
fn response_body(model: &str) -> Value {
    json!({"id":"resp-test","object":"response","status":"completed","model":model,
        "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],
        "usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}})
}
struct Backend {
    leaf: Arc<Leaf>,
    grid: Arc<Mutex<RouteGrid>>,
}
impl Backend {
    fn route(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        let (connection, model) =
            openai_server::route_grid::split_target(model).map_err(BackendError::ConfigError)?;
        if model == "missing" {
            return Err(BackendError::ConfigError("missing target".into()));
        }
        Ok(ResolvedRoute {
            routing: Default::default(),
            backend: self.leaf.clone(),
            stripped_model: model.into(),
            resolution: RouteResolution {
                provider_kind: "openai-compatible".into(),
                connection_id: Some(connection.into()),
                connection_name: connection.into(),
                upstream_model_id: model.into(),
                connection_prefix: None,
                model_route_id: None,
            },
            capabilities: Arc::new(
                openai_server::capabilities::resolve(
                    if model == "decisions-judge" {
                        "typesafe"
                    } else if model == "incompatible" {
                        "voyage"
                    } else if model == "judge" {
                        "openai-compatible"
                    } else {
                        self.leaf
                            .native_provider
                            .lock()
                            .unwrap()
                            .unwrap_or("openai-compatible")
                    },
                    if model == "decisions-judge" {
                        "jev-1.0"
                    } else {
                        model
                    },
                    None,
                    None,
                )
                .unwrap(),
            ),
        })
    }
}
#[async_trait]
impl BackendProvider for Backend {
    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        Ok(ListResponse {
            object: "list".into(),
            data: vec![],
            first_id: None,
            last_id: None,
            has_more: false,
        })
    }
    async fn get_model(&self, _: &str) -> Result<Model, BackendError> {
        Err(BackendError::api_status(404, "model not found"))
    }

    fn name(&self) -> &str {
        "test-router"
    }
    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        if model == "chalk/simple" {
            let mut route = self.route("conn/astra")?;
            route.resolution.model_route_id = Some("route-simple".into());
            return Ok(route);
        }
        if model != "chalk/assistant" {
            return self.route(model);
        }
        let grid = self.grid.lock().unwrap().clone();
        let target = grid.default_target.concrete_model();
        let mut route = self.route(&target)?;
        route.routing = Some(Arc::new(grid));
        route.resolution.model_route_id = Some(ASSISTANT_ROUTE.into());
        Ok(route)
    }
    async fn resolve_anthropic(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        self.resolve(model).await
    }
    async fn resolve_concrete(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        self.route(model)
    }
}
struct Setup {
    url: String,
    locator: openai_server::response_locator::ResponseLocatorCodec,
    leaf: Arc<Leaf>,
    grid: Arc<Mutex<RouteGrid>>,
}
impl Setup {
    async fn request(&self, overrides: Value, stream: bool) -> reqwest::Response {
        let mut body = json!({"model":"chalk/assistant","messages":[{"role":"user","content":"hello"}],"stream":stream,"max_completion_tokens":32});
        if !overrides.is_null() {
            body["fallbacks"] = overrides;
        }
        reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", self.url))
            .bearer_auth(KEY)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    async fn conversation(&self, surface: &str, body: Value) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/v1/{surface}", self.url))
            .bearer_auth(KEY)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    fn native(&self, surface: &str) {
        *self.leaf.native_provider.lock().unwrap() = Some(if surface == "messages" {
            "anthropic"
        } else {
            "openai"
        });
    }
    fn calls(&self) -> Vec<String> {
        self.leaf.calls.lock().unwrap().clone()
    }
}
async fn setup(grid: RouteGrid, permission: ApiKeyPermission) -> Setup {
    setup_with(grid, permission, |builder| builder).await
}
async fn setup_with(
    grid: RouteGrid,
    permission: ApiKeyPermission,
    configure: impl FnOnce(AppStateBuilder) -> AppStateBuilder,
) -> Setup {
    let grid = Arc::new(Mutex::new(grid));
    let leaf = Arc::new(Leaf::default());
    *leaf.judge_reply.lock().unwrap() = r#"{"classification":"0"}"#.into();
    let backend = Arc::new(Backend {
        leaf: leaf.clone(),
        grid: grid.clone(),
    });
    let keys = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    keys.insert_raw(ApiKey {
        key_hash: KeyHasher::new(b"test-pepper".to_vec()).hash(KEY),
        key_prefix: "sk-route".into(),
        description: "test".into(),
        created_at: chrono::Utc::now(),
        permission,
        labels: HashMap::new(),
        usage_pool_id: None,
        daily_token_budget: None,
    })
    .await
    .unwrap();
    let state = configure(AppStateBuilder::new(backend).api_key_store(keys))
        .build()
        .await;
    let locator = state.response_locator.clone();
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(openai_server::handlers::chat::create_chat_completion),
        )
        .route(
            "/v1/messages",
            post(openai_server::handlers::anthropic::proxy_messages),
        )
        .route(
            "/v1/responses",
            post(openai_server::handlers::responses::create_response),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth_middleware));
    Setup {
        locator,
        url: crate::support::mock::serve(app).await,
        leaf,
        grid,
    }
}
fn judged() -> RouteGrid {
    RouteGrid {
        default_target: RouteTarget {
            connection_id: "conn".into(),
            upstream_model_id: "astra".into(),
        },
        judge: Some(RouteJudge {
            mode: JudgeMode::Live,
            classification: Classification::Difficulty,
            backend: RouteJudgeBackend::Heuristic(Default::default()),
            outcomes: BTreeMap::from([
                (
                    "0".into(),
                    JudgeOutcomeConfig {
                        model: "conn/gpt-4o".into(),
                        description: "easy".into(),
                        fallbacks: None,
                    },
                ),
                (
                    "1".into(),
                    JudgeOutcomeConfig {
                        model: "conn/astra".into(),
                        description: "hard".into(),
                        fallbacks: None,
                    },
                ),
            ]),
            ..Default::default()
        }),
        fallbacks: BTreeMap::from([
            (
                "conn/gpt-4o".into(),
                vec!["conn/gpt-5.5".into(), "conn/opus".into()],
            ),
            ("conn/gpt-5.5".into(), vec!["conn/never".into()]),
            ("conn/astra".into(), vec!["conn/gpt-5.6".into()]),
        ]),
    }
}
#[tokio::test]
async fn simple_route_without_grid_still_accepts_request_fallback_overrides() {
    let s = setup(judged(), Default::default()).await;
    s.leaf.failures.lock().unwrap().insert("astra".into(), 503);
    let response = s
        .conversation(
            "chat/completions",
            json!({
                "model": "chalk/simple",
                "messages": [{"role": "user", "content": "hello"}],
                "fallbacks": {"conn/astra": ["conn/gpt-5.5"]}
            }),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(s.calls(), ["astra", "gpt-5.5"]);
    assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn selection_uses_full_configured_chain_without_a_cheaper_only_ceiling_or_recursion() {
    for stream in [false, true] {
        let s = setup(judged(), Default::default()).await;
        s.leaf
            .failures
            .lock()
            .unwrap()
            .extend([("gpt-4o".into(), 503), ("gpt-5.5".into(), 429)]);
        let response = s.request(Value::Null, stream).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-fallback-model"], "conn/opus");
        let _ = response.bytes().await.unwrap();
        assert_eq!(s.calls(), ["gpt-4o", "gpt-5.5", "opus"]);
    }
}
#[tokio::test]
async fn matching_request_override_replaces_the_chain_without_mutating_the_route() {
    let s = setup(judged(), Default::default()).await;
    s.leaf.failures.lock().unwrap().insert("gpt-4o".into(), 503);
    assert_eq!(
        s.request(
            json!({"conn/gpt-4o":["conn/astra"],"conn/astra":["conn/never"]}),
            false
        )
        .await
        .status(),
        200
    );
    assert_eq!(s.calls(), ["gpt-4o", "astra"]);
    s.leaf.calls.lock().unwrap().clear();
    assert_eq!(s.request(Value::Null, false).await.status(), 200);
    assert_eq!(s.calls(), ["gpt-4o", "gpt-5.5"]);
}
#[tokio::test]
async fn unmatched_override_uses_route_configuration_and_empty_entry_disables_fallback() {
    let s = setup(judged(), Default::default()).await;
    s.leaf.failures.lock().unwrap().insert("gpt-4o".into(), 503);
    assert_eq!(
        s.request(json!({"conn/gpt-5.6":["conn/astra"]}), false)
            .await
            .status(),
        200
    );
    assert_eq!(s.calls(), ["gpt-4o", "gpt-5.5"]);
    s.leaf.calls.lock().unwrap().clear();
    assert_eq!(
        s.request(json!({"conn/gpt-4o":[]}), false).await.status(),
        503
    );
    assert_eq!(s.calls(), ["gpt-4o"]);
}
#[tokio::test]
async fn outcome_specific_chain_is_used_before_route_defaults() {
    let mut grid = judged();
    grid.judge
        .as_mut()
        .unwrap()
        .outcomes
        .get_mut("0")
        .unwrap()
        .fallbacks = Some(vec!["conn/opus".into()]);
    let s = setup(grid, Default::default()).await;
    s.leaf.failures.lock().unwrap().insert("gpt-4o".into(), 503);
    assert_eq!(s.request(Value::Null, false).await.status(), 200);
    assert_eq!(s.calls(), ["gpt-4o", "opus"]);
}
#[tokio::test]
async fn off_and_shadow_serve_default_and_use_its_fallbacks() {
    for mode in [JudgeMode::Off, JudgeMode::Shadow] {
        let mut grid = judged();
        grid.judge.as_mut().unwrap().mode = mode;
        let s = setup(grid, Default::default()).await;
        s.leaf.failures.lock().unwrap().insert("astra".into(), 503);
        assert_eq!(s.request(Value::Null, false).await.status(), 200);
        assert_eq!(s.calls(), ["astra", "gpt-5.6"]);
    }
}
#[tokio::test]
async fn remote_judge_caches_classification_and_policy_edits_invalidate_it() {
    let mut grid = judged();
    let judge = grid.judge.as_mut().unwrap();
    judge.backend = RouteJudgeBackend::Completion(CompletionJudgeBackend {
        target: RouteTarget {
            connection_id: "conn".into(),
            upstream_model_id: "judge".into(),
        },
        ..Default::default()
    });
    let s = setup(grid, Default::default()).await;
    for _ in 0..2 {
        assert_eq!(s.request(Value::Null, false).await.status(), 200);
    }
    assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 1);
    s.grid
        .lock()
        .unwrap()
        .judge
        .as_mut()
        .unwrap()
        .outcomes
        .get_mut("0")
        .unwrap()
        .model = "conn/gpt-5.6".into();
    assert_eq!(s.request(Value::Null, false).await.status(), 200);
    assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 2);
    assert_eq!(s.calls(), ["gpt-4o", "gpt-4o", "gpt-5.6"]);
}
#[tokio::test]
async fn malformed_or_out_of_range_judgments_choose_default_and_are_not_cached() {
    for reply in [
        "not json",
        r#"{"classification":"unknown"}"#,
        r#"{"classification":"0","classification":"1"}"#,
    ] {
        let mut grid = judged();
        let j = grid.judge.as_mut().unwrap();
        j.backend = RouteJudgeBackend::Completion(CompletionJudgeBackend {
            target: RouteTarget {
                connection_id: "conn".into(),
                upstream_model_id: "judge".into(),
            },
            ..Default::default()
        });
        let s = setup(grid, Default::default()).await;
        *s.leaf.judge_reply.lock().unwrap() = reply.into();
        for _ in 0..2 {
            assert_eq!(s.request(Value::Null, false).await.status(), 200);
        }
        assert_eq!(s.calls(), ["astra", "astra"]);
        assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 2);
    }
}
#[tokio::test]
async fn judge_timeout_chooses_default_and_denied_egress_never_calls_judge() {
    let mut grid = judged();
    let j = grid.judge.as_mut().unwrap();
    j.backend = RouteJudgeBackend::Completion(CompletionJudgeBackend {
        target: RouteTarget {
            connection_id: "other".into(),
            upstream_model_id: "judge".into(),
        },
        ..Default::default()
    });
    if let RouteJudgeBackend::Completion(config) = &mut j.backend {
        config.timeout_ms = 5;
    }
    let s = setup(grid.clone(), Default::default()).await;
    s.leaf.judge_delay_ms.store(50, Ordering::SeqCst);
    assert_eq!(s.request(Value::Null, false).await.status(), 200);
    assert_eq!(s.calls(), ["astra"]);
    // A route grant covers the route's classifier, wherever it runs.
    let s = setup(grid.clone(), assistant_grant()).await;
    assert_eq!(s.request(Value::Null, false).await.status(), 200);
    assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 1);
    // Without the grant, request content never reaches the classifier's
    // connection, even though the request is then refused.
    let s = setup(
        grid,
        ApiKeyPermission {
            connection_scopes: Some(vec![ApiKeyConnectionScope {
                connection_id: "conn".into(),
                models: vec![],
            }]),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(s.request(Value::Null, false).await.status(), 403);
    assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn forbidden_candidate_is_skipped_and_original_denial_is_terminal() {
    // Route-owned candidates are covered by the route grant, so the forbidden
    // candidate here is a caller-chosen fallback on a connection the key's
    // scopes do not include.
    let permission = ApiKeyPermission {
        connection_scopes: Some(vec![ApiKeyConnectionScope {
            connection_id: "conn".into(),
            models: vec![],
        }]),
        ..assistant_grant()
    };
    let s = setup(judged(), permission).await;
    s.leaf.failures.lock().unwrap().insert("gpt-4o".into(), 503);
    let response = s
        .request(
            json!({"conn/gpt-4o": ["other/gpt-5.5", "conn/opus"]}),
            false,
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(s.calls(), ["gpt-4o", "opus"]);
    let s = setup(
        judged(),
        ApiKeyPermission {
            models: Some(vec!["conn/gpt-4o".into()]),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(s.request(Value::Null, false).await.status(), 403);
    assert!(s.calls().is_empty());
}
#[tokio::test]
async fn route_model_access_covers_judge_selection_and_route_fallbacks() {
    // The key is granted only the route; the judge's choice and the grid's
    // fallbacks are part of that route and are served under its grant, on
    // connections the key's scopes do not include.
    let mut grid = judged();
    grid.fallbacks
        .insert("conn/gpt-4o".into(), vec!["other/gpt-5.5".into()]);
    let permission = ApiKeyPermission {
        connection_scopes: Some(vec![ApiKeyConnectionScope {
            connection_id: "unrelated".into(),
            models: vec![],
        }]),
        ..assistant_grant()
    };
    let s = setup(grid, permission).await;
    s.leaf.failures.lock().unwrap().insert("gpt-4o".into(), 503);
    assert_eq!(s.request(Value::Null, false).await.status(), 200);
    assert_eq!(s.calls(), ["gpt-4o", "gpt-5.5"]);
}
#[tokio::test]
async fn connection_access_never_reaches_a_route() {
    // Every connection the route uses is in scope, and the public id is
    // listed; neither grants the route.
    for permission in [
        ApiKeyPermission {
            connection_scopes: Some(vec![
                ApiKeyConnectionScope {
                    connection_id: "conn".into(),
                    models: vec![],
                },
                ApiKeyConnectionScope {
                    connection_id: "other".into(),
                    models: vec![],
                },
            ]),
            ..Default::default()
        },
        ApiKeyPermission {
            models: Some(vec!["chalk/assistant".into()]),
            ..Default::default()
        },
        ApiKeyPermission {
            route_ids: Some(vec!["route-simple".into()]),
            ..Default::default()
        },
    ] {
        let s = setup(judged(), permission).await;
        assert_eq!(s.request(Value::Null, false).await.status(), 403);
        assert!(s.calls().is_empty());
    }
}
#[tokio::test]
async fn caller_fallbacks_need_their_own_model_access() {
    for (scoped, status, calls) in [
        (vec![], 503, vec!["gpt-4o"]),
        (vec!["conn/opus"], 200, vec!["gpt-4o", "opus"]),
    ] {
        let permission = ApiKeyPermission {
            connection_scopes: Some(vec![ApiKeyConnectionScope {
                connection_id: "conn".into(),
                models: if scoped.is_empty() {
                    vec!["conn/unrelated".into()]
                } else {
                    scoped.into_iter().map(Into::into).collect()
                },
            }]),
            ..assistant_grant()
        };
        let s = setup(judged(), permission).await;
        s.leaf.failures.lock().unwrap().insert("gpt-4o".into(), 503);
        let response = s
            .request(json!({"conn/gpt-4o": ["conn/opus"]}), false)
            .await;
        assert_eq!(response.status(), status);
        assert_eq!(s.calls(), calls);
    }
}
#[tokio::test]
async fn an_empty_plan_reports_why_nothing_could_serve() {
    let mut grid = judged();
    let outcome = grid.judge.as_mut().unwrap().outcomes.get_mut("0").unwrap();
    outcome.model = "other/gpt-4o".into();
    outcome.fallbacks = Some(vec![]);
    let scoped = ApiKeyPermission {
        connection_scopes: Some(vec![ApiKeyConnectionScope {
            connection_id: "conn".into(),
            models: vec![],
        }]),
        ..Default::default()
    };
    let s = setup(grid.clone(), scoped).await;
    let response = s.request(Value::Null, false).await;
    assert_eq!(response.status(), 403);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "permission_error");
    assert!(s.calls().is_empty());

    let outcome = grid.judge.as_mut().unwrap().outcomes.get_mut("0").unwrap();
    outcome.model = "conn/missing".into();
    let s = setup(grid, Default::default()).await;
    assert_eq!(s.request(Value::Null, false).await.status(), 503);
    assert!(s.calls().is_empty());
}
#[tokio::test]
async fn missing_judge_selected_destination_uses_its_chain() {
    let mut grid = judged();
    let choice = grid.judge.as_mut().unwrap().outcomes.get_mut("0").unwrap();
    choice.model = "conn/missing".into();
    choice.fallbacks = Some(vec!["conn/astra".into()]);
    let s = setup(grid, Default::default()).await;
    assert_eq!(s.request(Value::Null, false).await.status(), 200);
    assert_eq!(s.calls(), ["astra"]);
}
#[tokio::test]
async fn client_errors_and_unreadable_responses_do_not_fallback() {
    for stream in [false, true] {
        for status in [400, 401, 403, 404, 422, 0] {
            let s = setup(judged(), Default::default()).await;
            s.leaf
                .failures
                .lock()
                .unwrap()
                .insert("gpt-4o".into(), status);
            assert_eq!(
                s.request(Value::Null, stream).await.status().as_u16(),
                if status == 0 { 502 } else { status }
            );
            assert_eq!(s.calls(), ["gpt-4o"]);
        }
    }
}
#[tokio::test]
async fn alias_or_duplicate_request_overrides_are_rejected_before_dispatch() {
    let s = setup(judged(), Default::default()).await;
    for overrides in [
        json!({"conn/gpt-4o":["chalk/assistant"]}),
        json!({"conn/gpt-4o":["conn/gpt-4o"]}),
        json!({"conn/gpt-4o":["conn/astra","conn/astra"]}),
    ] {
        assert_eq!(s.request(overrides, false).await.status(), 400);
    }
    assert!(s.calls().is_empty());
}

#[tokio::test]
async fn decisions_transport_classifies_task_labels() {
    let grid = RouteGrid {
        default_target: RouteTarget {
            connection_id: "conn".into(),
            upstream_model_id: "astra".into(),
        },
        judge: Some(RouteJudge {
            mode: JudgeMode::Live,
            classification: Classification::Task,
            backend: RouteJudgeBackend::Decisions(DecisionsJudgeBackend {
                target: RouteTarget {
                    connection_id: "conn".into(),
                    upstream_model_id: "decisions-judge".into(),
                },
                ..Default::default()
            }),
            outcomes: BTreeMap::from([(
                "conversation".into(),
                JudgeOutcomeConfig {
                    model: "conn/gpt-4o".into(),
                    description: "conversation".into(),
                    fallbacks: None,
                },
            )]),
            ..Default::default()
        }),
        fallbacks: BTreeMap::new(),
    };
    let s = setup(grid, Default::default()).await;
    assert_eq!(s.request(Value::Null, false).await.status(), 200);
    assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 1);
    assert_eq!(s.calls(), ["gpt-4o"]);
    for surface in ["messages", "responses"] {
        assert_eq!(
            s.conversation(surface, conversation_body(surface, false))
                .await
                .status(),
            200
        );
    }
    assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 3);
    assert_eq!(s.calls(), ["gpt-4o", "gpt-4o", "gpt-4o"]);
}

#[tokio::test]
async fn decisions_backend_deadline_uses_custom_default_and_its_fallbacks() {
    let mut grid = judged();
    grid.default_target = RouteTarget {
        connection_id: "conn".into(),
        upstream_model_id: "opus".into(),
    };
    grid.fallbacks
        .insert("conn/opus".into(), vec!["conn/gpt-5.6".into()]);
    let judge = grid.judge.as_mut().unwrap();
    judge.classification = Classification::Task;
    judge.backend = RouteJudgeBackend::Decisions(DecisionsJudgeBackend {
        target: RouteTarget {
            connection_id: "conn".into(),
            upstream_model_id: "decisions-judge".into(),
        },
        timeout_ms: 5,
    });
    judge.outcomes = BTreeMap::from([(
        "conversation".into(),
        JudgeOutcomeConfig {
            model: "conn/gpt-4o".into(),
            description: "conversation".into(),
            fallbacks: None,
        },
    )]);
    let s = setup(grid, Default::default()).await;
    s.leaf.judge_delay_ms.store(50, Ordering::SeqCst);
    s.leaf.failures.lock().unwrap().insert("opus".into(), 503);
    assert_eq!(s.request(Value::Null, false).await.status(), 200);
    assert_eq!(s.calls(), ["opus", "gpt-5.6"]);
}

use openai_server::quota::{
    MemoryQuota, QuotaAction, QuotaBackend, QuotaBackendError, QuotaDecision, QuotaGrant,
    QuotaMeter, QuotaOutcome,
};
#[derive(Default)]
struct RecordedQuota {
    inner: MemoryQuota,
    admissions: Mutex<Vec<Vec<QuotaAction>>>,
    settlements: Mutex<Vec<(Vec<QuotaAction>, QuotaOutcome)>>,
}
#[async_trait]
impl QuotaBackend for RecordedQuota {
    fn enabled(&self) -> bool {
        true
    }
    async fn acquire_many(
        &self,
        actions: &[QuotaAction],
        failed_amount: Option<u64>,
    ) -> Result<QuotaDecision, QuotaBackendError> {
        self.admissions.lock().unwrap().push(actions.to_vec());
        self.inner.acquire_many(actions, failed_amount).await
    }
    async fn finish(
        &self,
        grant: &QuotaGrant,
        outcome: QuotaOutcome,
    ) -> Result<(), QuotaBackendError> {
        self.settlements
            .lock()
            .unwrap()
            .push((grant.actions.clone(), outcome));
        self.inner.finish(grant, outcome).await
    }
    async fn renew(&self, id: uuid::Uuid) -> Result<(), QuotaBackendError> {
        self.inner.renew(id).await
    }
    async fn cleanup(&self, size: u64) -> Result<(u64, u64), QuotaBackendError> {
        self.inner.cleanup(size).await
    }
}
async fn quota_setup(budget: u64) -> (Setup, Arc<RecordedQuota>) {
    use openai_server::{
        budget::{BudgetPolicy, MemoryBudgetPolicyStore},
        quota_policy::QuotaPolicyResolver,
        ratelimit::{MemoryRateLimitPolicyStore, RateLimitPolicy, RateLimitPolicyStore},
    };
    let hash = KeyHasher::new(b"test-pepper".to_vec()).hash(KEY);
    let budgets = Arc::new(MemoryBudgetPolicyStore::new("test".into()));
    budgets
        .insert(BudgetPolicy {
            quota_policy_id: "budget".into(),
            id: "budget-source".into(),
            environment_id: "test".into(),
            scope: "api_key".into(),
            scope_id: hash.clone(),
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
            limit_amount: budget,
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
    let rates = Arc::new(MemoryRateLimitPolicyStore::new("test".into()));
    rates
        .create(RateLimitPolicy {
            quota_policy_id: "logical-requests".into(),
            id: "request-source".into(),
            limit_type: "requests_per_minute".into(),
            limit_value: 1000,
            burst_value: Some(1000),
            connection_id: None,
            models: vec![],
            scope: "api_key".into(),
            scope_id: hash,
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    rates
        .create(RateLimitPolicy {
            quota_policy_id: "selected-tpm".into(),
            id: "selected-tpm-source".into(),
            limit_type: "tokens_per_minute".into(),
            limit_value: 10000,
            burst_value: Some(10000),
            connection_id: Some("conn".into()),
            // Route-owned destinations are limited under the requested route.
            models: vec!["chalk/assistant".into()],
            scope: "api_key".into(),
            scope_id: KeyHasher::new(b"test-pepper".to_vec()).hash(KEY),
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    let resolver = Arc::new(QuotaPolicyResolver::new(
        "test".into(),
        budgets.clone(),
        rates.clone(),
    ));
    let quota = Arc::new(RecordedQuota::default());
    let q = quota.clone();
    let mut grid = judged();
    let j = grid.judge.as_mut().unwrap();
    j.backend = RouteJudgeBackend::Completion(CompletionJudgeBackend {
        target: RouteTarget {
            connection_id: "conn".into(),
            upstream_model_id: "judge".into(),
        },
        ..Default::default()
    });
    let setup = setup_with(grid, Default::default(), move |b| {
        b.budget_store(budgets)
            .rate_limit_store(rates)
            .quota_backend(q)
            .quota_resolver(resolver)
    })
    .await;
    (setup, quota)
}
#[tokio::test]
async fn remote_judge_charges_tokens_once_and_does_not_double_count_logical_requests() {
    let (s, quota) = quota_setup(10000).await;
    for _ in 0..2 {
        assert_eq!(s.request(Value::Null, false).await.status(), 200);
    }
    assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 1);
    assert_eq!(s.calls(), ["gpt-4o", "gpt-4o"]);
    let admissions = quota.admissions.lock().unwrap();
    assert_eq!(
        admissions
            .iter()
            .flatten()
            .filter(|a| a.meter == QuotaMeter::Requests)
            .count(),
        2
    );
    assert_eq!(
        admissions
            .iter()
            .flatten()
            .filter(|a| a.policy_id == "budget")
            .count(),
        3
    );
    let settlements = quota.settlements.lock().unwrap();
    let tokens: u64 = settlements
        .iter()
        .filter(|(actions, _)| actions.iter().any(|a| a.policy_id == "budget"))
        .map(|(_, outcome)| match outcome {
            QuotaOutcome::Actual(n) => *n,
            QuotaOutcome::Unknown | QuotaOutcome::Failed => {
                panic!("known usage must settle exactly")
            }
        })
        .sum();
    assert_eq!(tokens, 9, "one classifier plus two generation calls");
}
#[tokio::test]
async fn shared_budget_denial_stops_before_judge_or_generation() {
    let (s, _) = quota_setup(1).await;
    assert_eq!(s.request(Value::Null, false).await.status(), 429);
    assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 0);
    assert!(s.calls().is_empty());
}

#[tokio::test]
async fn unreadable_generation_retains_the_candidate_token_reservation() {
    let (s, quota) = quota_setup(10000).await;
    s.leaf.failures.lock().unwrap().insert("gpt-4o".into(), 0);
    assert_eq!(s.request(Value::Null, false).await.status(), 502);
    assert_eq!(s.calls(), ["gpt-4o"]);
    assert!(quota
        .settlements
        .lock()
        .unwrap()
        .iter()
        .any(|(actions, outcome)| {
            actions.iter().any(|a| a.policy_id == "selected-tpm")
                && matches!(outcome, QuotaOutcome::Unknown)
        }));
}

fn conversation_body(surface: &str, stream: bool) -> Value {
    if surface == "messages" {
        json!({"model":"chalk/assistant","messages":[{"role":"user","content":"hello"}],"max_tokens":32,"stream":stream})
    } else {
        json!({"model":"chalk/assistant","input":"hello","max_output_tokens":32,"stream":stream})
    }
}
fn remote_grid() -> RouteGrid {
    let mut grid = judged();
    grid.judge.as_mut().unwrap().backend = RouteJudgeBackend::Completion(CompletionJudgeBackend {
        target: RouteTarget {
            connection_id: "conn".into(),
            upstream_model_id: "judge".into(),
        },
        ..Default::default()
    });
    grid
}

#[tokio::test]
async fn heuristic_keyword_edits_change_selection_across_conversation_endpoints() {
    for surface in ["chat/completions", "messages", "responses"] {
        let mut grid = judged();
        let RouteJudgeBackend::Heuristic(config) = &mut grid.judge.as_mut().unwrap().backend else {
            panic!("expected heuristic backend");
        };
        config.use_default_keywords = Some(false);
        config.added_hard_keywords = vec!["HELLO".into()];
        config.code_blocks_are_hard = Some(false);
        config.max_easy_input_bytes = Some(0);
        let s = setup(grid, Default::default()).await;
        let body = if surface == "chat/completions" {
            json!({"model":"chalk/assistant","messages":[{"role":"user","content":"hello"}],"max_completion_tokens":32})
        } else {
            conversation_body(surface, false)
        };
        assert_eq!(s.conversation(surface, body.clone()).await.status(), 200);
        {
            let mut grid = s.grid.lock().unwrap();
            let RouteJudgeBackend::Heuristic(config) = &mut grid.judge.as_mut().unwrap().backend
            else {
                panic!("expected heuristic backend");
            };
            config.added_hard_keywords.clear();
        }
        assert_eq!(s.conversation(surface, body).await.status(), 200);
        assert_eq!(s.calls(), ["astra", "gpt-4o"]);
        assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn conversation_endpoints_judge_then_walk_the_chain_on_native_and_translated_transports() {
    for surface in ["messages", "responses"] {
        for native in [false, true] {
            for stream in [false, true] {
                let s = setup(judged(), Default::default()).await;
                if native {
                    s.native(surface);
                }
                s.leaf
                    .failures
                    .lock()
                    .unwrap()
                    .extend([("gpt-4o".into(), 503), ("gpt-5.5".into(), 429)]);
                let response = s
                    .conversation(surface, conversation_body(surface, stream))
                    .await;
                assert_eq!(
                    response.status(),
                    200,
                    "{surface}, native={native}, stream={stream}"
                );
                assert_eq!(response.headers()["x-chalk-routed-model"], "conn/opus");
                assert_eq!(response.headers()["x-fallback-model"], "conn/opus");
                let body = response.text().await.unwrap();
                assert!(body.contains("ok"), "{body}");
                assert_eq!(s.calls(), ["gpt-4o", "gpt-5.5", "opus"]);
            }
        }
    }
}

#[tokio::test]
async fn native_conversations_keep_their_request_shape_and_cache_the_full_judge_input() {
    for surface in ["messages", "responses"] {
        for stream in [false, true] {
            let s = setup(remote_grid(), Default::default()).await;
            s.native(surface);
            let mut body = conversation_body(surface, stream);
            let instruction = if surface == "messages" {
                "system"
            } else {
                "instructions"
            };
            body[instruction] = json!("Summarize the supplied tool result");
            body["future_native_field"] = json!({"preserve":true});
            if surface == "messages" {
                body["system"] = json!([{"type":"text","text":"Summarize the supplied tool result","cache_control":{"type":"ephemeral"}}]);
                body["tools"] = json!([{"name":"lookup","input_schema":{"type":"object"}}]);
                body["messages"] = json!([
                    {"role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"lookup","input":{"q":"weather"}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","content":"sunny"}]}
                ]);
            } else {
                body["tools"] =
                    json!([{"type":"function","name":"lookup","parameters":{"type":"object"}}]);
                body["input"] = json!([
                    {"type":"function_call","call_id":"tool-1","name":"lookup","arguments":"{}"},
                    {"type":"function_call_output","call_id":"tool-1","output":"sunny"}
                ]);
                body["text"] = json!({"format":{"type":"json_object"}});
            }
            for _ in 0..2 {
                let response = s.conversation(surface, body.clone()).await;
                assert_eq!(response.status(), 200);
                assert_eq!(response.headers()["x-chalk-routed-model"], "conn/gpt-4o");
                assert!(response.headers().get("x-fallback-model").is_none());
                response.bytes().await.unwrap();
            }
            assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 1);
            let captured = s.leaf.judge_inputs.lock().unwrap()[0].clone();
            assert_eq!(captured["surface"], surface);
            assert_eq!(captured["request"][instruction], body[instruction]);
            assert_eq!(captured["request"]["tools"], body["tools"]);
            let conversation = if surface == "messages" {
                "messages"
            } else {
                "input"
            };
            assert_eq!(captured["request"][conversation], body[conversation]);
            let mut expected = body.clone();
            expected["model"] = json!("gpt-4o");
            assert_eq!(s.leaf.native_calls.lock().unwrap()[0], expected);
            // Even fields omitted from the judge's bounded view affect cache identity.
            body["future_native_field"] = json!({"preserve":"changed"});
            assert_eq!(s.conversation(surface, body).await.status(), 200);
            assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 2);
        }
    }
}

#[tokio::test]
async fn conversation_outcome_fallbacks_override_defaults_and_empty_lists_stop() {
    for surface in ["messages", "responses"] {
        for alternatives in [vec!["conn/opus".into()], vec![]] {
            let mut grid = judged();
            grid.judge
                .as_mut()
                .unwrap()
                .outcomes
                .get_mut("0")
                .unwrap()
                .fallbacks = Some(alternatives.clone());
            let s = setup(grid, Default::default()).await;
            s.leaf.failures.lock().unwrap().insert("gpt-4o".into(), 503);
            let response = s
                .conversation(surface, conversation_body(surface, false))
                .await;
            assert_eq!(
                response.status(),
                if alternatives.is_empty() { 503 } else { 200 }
            );
            assert_eq!(
                s.calls(),
                if alternatives.is_empty() {
                    vec!["gpt-4o"]
                } else {
                    vec!["gpt-4o", "opus"]
                }
            );
        }
    }
}

#[tokio::test]
async fn conversation_off_shadow_and_unavailable_judges_use_the_route_default() {
    for surface in ["messages", "responses"] {
        for mode in [JudgeMode::Off, JudgeMode::Shadow, JudgeMode::Live] {
            for stream in [false, true] {
                let mut grid = remote_grid();
                grid.judge.as_mut().unwrap().mode = mode;
                let s = setup(grid, Default::default()).await;
                s.native(surface);
                if mode == JudgeMode::Live {
                    *s.leaf.judge_reply.lock().unwrap() = "malformed".into();
                }
                s.leaf.failures.lock().unwrap().insert("astra".into(), 503);
                let response = s
                    .conversation(surface, conversation_body(surface, stream))
                    .await;
                assert_eq!(response.status(), 200);
                response.bytes().await.unwrap();
                assert_eq!(s.calls(), ["astra", "gpt-5.6"]);
                if mode == JudgeMode::Shadow {
                    tokio::time::timeout(Duration::from_secs(2), async {
                        while s.leaf.judge_calls.load(Ordering::SeqCst) == 0 {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                }
                assert_eq!(
                    s.leaf.judge_calls.load(Ordering::SeqCst),
                    usize::from(mode != JudgeMode::Off)
                );
            }
        }
        let mut grid = remote_grid();
        if let RouteJudgeBackend::Completion(config) = &mut grid.judge.as_mut().unwrap().backend {
            config.timeout_ms = 1;
        }
        let s = setup(grid, Default::default()).await;
        s.leaf.judge_delay_ms.store(50, Ordering::SeqCst);
        assert_eq!(
            s.conversation(surface, conversation_body(surface, false))
                .await
                .status(),
            200
        );
        assert_eq!(s.calls(), ["astra"]);
    }
}

#[tokio::test]
async fn conversation_candidates_still_enforce_permissions_and_capabilities() {
    for surface in ["messages", "responses"] {
        // The judge picks a destination on a connection outside the key's
        // scopes; the route grant covers it.
        let mut grid = judged();
        let outcome = grid.judge.as_mut().unwrap().outcomes.get_mut("0").unwrap();
        outcome.model = "other/gpt-4o".into();
        outcome.fallbacks = Some(vec!["conn/opus".into()]);
        let permission = ApiKeyPermission {
            connection_scopes: Some(vec![ApiKeyConnectionScope {
                connection_id: "conn".into(),
                models: vec![],
            }]),
            ..assistant_grant()
        };
        let s = setup(grid, permission).await;
        assert_eq!(
            s.conversation(surface, conversation_body(surface, false))
                .await
                .status(),
            200
        );
        assert_eq!(s.calls(), ["gpt-4o"]);
        let s = setup(
            remote_grid(),
            ApiKeyPermission {
                models: Some(vec!["conn/gpt-4o".into()]),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            s.conversation(surface, conversation_body(surface, false))
                .await
                .status(),
            403
        );
        assert!(s.calls().is_empty());
        assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 0);
        let mut grid = judged();
        let outcome = grid.judge.as_mut().unwrap().outcomes.get_mut("0").unwrap();
        outcome.model = "conn/incompatible".into();
        outcome.fallbacks = Some(vec!["conn/gpt-4o".into()]);
        let s = setup(grid, Default::default()).await;
        assert_eq!(
            s.conversation(surface, conversation_body(surface, false))
                .await
                .status(),
            200
        );
        assert_eq!(s.calls(), ["gpt-4o"]);
    }
}

#[tokio::test]
async fn responses_with_external_context_skip_judging_and_keep_connection_affinity() {
    for stream in [false, true] {
        for field in ["previous_response_id", "conversation", "prompt"] {
            let s = setup(remote_grid(), Default::default()).await;
            s.native("responses");
            let mut body = conversation_body("responses", stream);
            body[field] = if field == "previous_response_id" {
                json!(s.locator.encode("conn", None, "upstream-previous"))
            } else {
                json!({"id":"stored-context"})
            };
            let response = s.conversation("responses", body.clone()).await;
            assert_eq!(response.status(), 200);
            response.bytes().await.unwrap();
            assert_eq!(s.calls(), ["astra"]);
            assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 0);
            if field == "previous_response_id" {
                assert_eq!(
                    s.leaf.native_calls.lock().unwrap()[0][field],
                    "upstream-previous"
                );
                // The stored response pins this continuation to its connection,
                // even when the default's fallback list names another connection.
                s.grid.lock().unwrap().fallbacks.insert(
                    "conn/astra".into(),
                    vec!["other/opus".into(), "conn/gpt-5.6".into()],
                );
                s.leaf.failures.lock().unwrap().insert("astra".into(), 503);
                s.leaf.calls.lock().unwrap().clear();
                assert_eq!(s.conversation("responses", body).await.status(), 200);
                assert_eq!(s.calls(), ["astra", "gpt-5.6"]);
                assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 0);
            }
        }
    }
}

#[tokio::test]
async fn conversation_errors_are_terminal_and_committed_streams_never_fallback() {
    for surface in ["messages", "responses"] {
        for stream in [false, true] {
            for status in [400, 401, 404, 0] {
                let s = setup(judged(), Default::default()).await;
                s.native(surface);
                s.leaf
                    .failures
                    .lock()
                    .unwrap()
                    .insert("gpt-4o".into(), status);
                let response = s
                    .conversation(surface, conversation_body(surface, stream))
                    .await;
                assert_eq!(
                    response.status().as_u16(),
                    if status == 0 { 502 } else { status }
                );
                assert_eq!(s.calls(), ["gpt-4o"]);
            }
        }
        let s = setup(judged(), Default::default()).await;
        s.native(surface);
        s.leaf.stream_error.store(1, Ordering::SeqCst);
        let response = s
            .conversation(surface, conversation_body(surface, true))
            .await;
        assert_eq!(response.status(), 200);
        assert!(response.text().await.unwrap().contains("stream failed"));
        assert_eq!(s.calls(), ["gpt-4o"]);
    }
}

#[tokio::test]
async fn conversation_remote_judges_share_accounting_without_double_charging_requests() {
    for surface in ["messages", "responses"] {
        for stream in [false, true] {
            let (s, quota) = quota_setup(10000).await;
            s.native(surface);
            for _ in 0..2 {
                let response = s
                    .conversation(surface, conversation_body(surface, stream))
                    .await;
                assert_eq!(response.status(), 200);
                response.bytes().await.unwrap();
            }
            assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 1);
            assert_eq!(s.calls(), ["gpt-4o", "gpt-4o"]);
            // Streaming settlement may finish just after the last body chunk.
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let count = quota
                        .settlements
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(actions, _)| actions.iter().any(|a| a.policy_id == "budget"))
                        .count();
                    if count == 3 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                quota
                    .admissions
                    .lock()
                    .unwrap()
                    .iter()
                    .flatten()
                    .filter(|a| a.meter == QuotaMeter::Requests)
                    .count(),
                2
            );
            let tokens: u64 = quota
                .settlements
                .lock()
                .unwrap()
                .iter()
                .filter(|(actions, _)| actions.iter().any(|a| a.policy_id == "budget"))
                .map(|(_, outcome)| match outcome {
                    QuotaOutcome::Actual(n) => *n,
                    QuotaOutcome::Unknown | QuotaOutcome::Failed => {
                        panic!("known usage must settle exactly")
                    }
                })
                .sum();
            assert_eq!(tokens, 9, "{surface}, stream={stream}");
        }
        let (s, _) = quota_setup(1).await;
        assert_eq!(
            s.conversation(surface, conversation_body(surface, false))
                .await
                .status(),
            429
        );
        assert_eq!(s.leaf.judge_calls.load(Ordering::SeqCst), 0);
        assert!(s.calls().is_empty());
    }
}
