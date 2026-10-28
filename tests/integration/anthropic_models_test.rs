//! Anthropic model catalog tests using a local stand-in for the Models API.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use openai_server::backend::{
    anthropic_backend, AnthropicBackend, BackendError, BackendProvider, RouterBackend,
};
use openai_server::capabilities::{DeliveryMode, Protocol, ProtocolOperation};
use openai_server::model_routes::InMemoryModelRouteStore;
use openai_server::provider_connections::{
    ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

const MAX_MODEL_PAGES: usize = 32;
const MAX_MODELS: usize = 10_000;

#[derive(Debug, PartialEq, Eq)]
struct CapturedRequest {
    path: String,
    model_id: Option<String>,
    query: HashMap<String, String>,
    api_key: Option<String>,
    version: Option<String>,
}

struct MockResponse {
    status: StatusCode,
    body: Value,
    headers: Vec<(&'static str, &'static str)>,
}

impl MockResponse {
    fn ok(body: Value) -> Self {
        Self {
            status: StatusCode::OK,
            body,
            headers: Vec::new(),
        }
    }
}

struct MockState {
    requests: Mutex<Vec<CapturedRequest>>,
    responses: Mutex<VecDeque<MockResponse>>,
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn model(id: &str, created_at: Option<&str>) -> Value {
    let mut model = json!({
        "type": "model",
        "id": id,
        "display_name": id,
    });
    if let Some(created_at) = created_at {
        model["created_at"] = json!(created_at);
    }
    model
}

fn page(data: Vec<Value>, has_more: bool, last_id: Option<&str>) -> Value {
    json!({
        "data": data,
        "last_id": last_id,
        "has_more": has_more,
    })
}

async fn scripted_response(state: &MockState) -> Response {
    let Some(scripted) = state.responses.lock().await.pop_front() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "type": "error",
                "error": {
                    "type": "mock_script_exhausted",
                    "message": "unexpected upstream request"
                }
            })),
        )
            .into_response();
    };
    let mut response = (scripted.status, Json(scripted.body)).into_response();
    for (name, value) in scripted.headers {
        response.headers_mut().insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    response
}

async fn list_models(
    State(state): State<Arc<MockState>>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    state.requests.lock().await.push(CapturedRequest {
        path: uri.path().to_string(),
        model_id: None,
        query,
        api_key: header(&headers, "x-api-key"),
        version: header(&headers, "anthropic-version"),
    });
    scripted_response(&state).await
}

async fn get_model(
    State(state): State<Arc<MockState>>,
    OriginalUri(uri): OriginalUri,
    Path(model_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    state.requests.lock().await.push(CapturedRequest {
        path: uri.path().to_string(),
        model_id: Some(model_id),
        query: HashMap::new(),
        api_key: header(&headers, "x-api-key"),
        version: header(&headers, "anthropic-version"),
    });
    scripted_response(&state).await
}

async fn unexpected_messages(
    State(state): State<Arc<MockState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    state.requests.lock().await.push(CapturedRequest {
        path: uri.path().to_string(),
        model_id: None,
        query: HashMap::new(),
        api_key: header(&headers, "x-api-key"),
        version: header(&headers, "anthropic-version"),
    });
    scripted_response(&state).await
}

async fn start_mock(responses: Vec<MockResponse>) -> (AnthropicBackend, Arc<MockState>) {
    let (base_url, state) = start_mock_server(responses).await;
    (
        anthropic_backend(
            reqwest::Client::new(),
            "test-key".to_string(),
            Some(&base_url),
        ),
        state,
    )
}

async fn start_mock_server(responses: Vec<MockResponse>) -> (String, Arc<MockState>) {
    let state = Arc::new(MockState {
        requests: Mutex::new(Vec::new()),
        responses: Mutex::new(responses.into()),
    });
    let app = Router::new()
        .route("/models", get(list_models))
        .route("/models/:model_id", get(get_model))
        .route("/messages", get(unexpected_messages))
        .with_state(state.clone());
    let base = crate::support::mock::serve(app).await;

    (base, state)
}

#[tokio::test]
async fn list_models_paginates_deduplicates_and_maps_timestamps() {
    let (backend, state) = start_mock(vec![
        MockResponse::ok(page(
            vec![model("claude-opus-5", Some("2026-07-24T00:00:00Z"))],
            true,
            Some("page-1"),
        )),
        MockResponse::ok(page(
            vec![
                model("claude-opus-5", Some("2026-07-24T00:00:00Z")),
                model("claude-sonnet-5", Some("2026-06-15T00:00:00Z")),
            ],
            false,
            Some("page-2"),
        )),
    ])
    .await;

    let response = backend.list_models().await.expect("list models");
    assert_eq!(
        response
            .data
            .iter()
            .map(|model| (model.id.as_str(), model.created))
            .collect::<Vec<_>>(),
        vec![
            ("claude-opus-5", 1_784_851_200),
            ("claude-sonnet-5", 1_781_481_600)
        ]
    );
    assert!(response
        .data
        .iter()
        .all(|model| model.object == "model" && model.owned_by == "anthropic"));
    assert_eq!(response.first_id, None);
    assert_eq!(response.last_id, None);
    assert!(!response.has_more);

    let requests = state.requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path, "/models");
    assert_eq!(
        requests[0].query.get("limit").map(String::as_str),
        Some("1000")
    );
    assert!(!requests[0].query.contains_key("after_id"));
    assert_eq!(
        requests[1].query.get("after_id").map(String::as_str),
        Some("page-1")
    );
    for request in requests.iter() {
        assert_eq!(request.api_key.as_deref(), Some("test-key"));
        assert_eq!(request.version.as_deref(), Some("2023-06-01"));
    }
}

#[tokio::test]
async fn get_model_encodes_the_id_as_one_path_segment() {
    let dangerous_id = "../messages";
    let (backend, state) = start_mock(vec![MockResponse::ok(model(
        dangerous_id,
        Some("2026-07-24T00:00:00Z"),
    ))])
    .await;

    let first = backend.get_model(dangerous_id).await.expect("get model");
    assert_eq!(first.id, dangerous_id);
    let requests = state.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert_ne!(requests[0].path, "/messages");
    assert!(requests[0].path.starts_with("/models/"));
    assert!(requests[0].path.to_ascii_lowercase().contains("%2f"));
    assert_eq!(requests[0].model_id.as_deref(), Some(dangerous_id));
}

#[tokio::test]
async fn get_model_rejects_empty_and_dot_segments_without_an_upstream_request() {
    let (backend, state) = start_mock(vec![]).await;

    for model_id in ["", ".", ".."] {
        let error = backend
            .get_model(model_id)
            .await
            .expect_err("invalid model id");
        assert!(matches!(error, BackendError::ApiError { status: 400, .. }));
    }

    assert!(state.requests.lock().await.is_empty());
}

#[tokio::test]
async fn router_get_model_uses_the_connection_endpoint_and_shared_detail_cache() {
    let (base_url, state) = start_mock_server(vec![MockResponse::ok(model(
        "claude-opus-5-20260801",
        Some("2026-07-24T00:00:00Z"),
    ))])
    .await;
    let connections: Arc<dyn ProviderConnectionStore> =
        Arc::new(InMemoryProviderConnectionStore::new());
    connections
        .create(ConnectionInput {
            name: "anthropic".to_string(),
            provider_kind: "anthropic".to_string(),
            base_url: Some(base_url),
            prefix: Some("anthropic/".to_string()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "test-key".to_string(),
        })
        .await
        .expect("store connection");
    let router = RouterBackend::new(
        reqwest::Client::new(),
        connections,
        Arc::new(InMemoryModelRouteStore::new()),
    );

    let first = router
        .get_model("anthropic/claude-opus-5")
        .await
        .expect("first lookup");
    let second = router
        .get_model("anthropic/claude-opus-5")
        .await
        .expect("cached lookup");

    assert_eq!(first.id, "anthropic/claude-opus-5-20260801");
    assert_eq!(second.id, "anthropic/claude-opus-5-20260801");
    let requests = state.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/models/claude-opus-5");
    assert_eq!(requests[0].api_key.as_deref(), Some("test-key"));
}

#[tokio::test]
async fn list_models_tolerates_missing_and_invalid_created_at() {
    let (backend, _) = start_mock(vec![MockResponse::ok(page(
        vec![
            model("missing-date", None),
            model("invalid-date", Some("not-a-date")),
        ],
        false,
        None,
    ))])
    .await;

    let response = backend.list_models().await.expect("list models");

    assert_eq!(
        response
            .data
            .iter()
            .map(|model| model.created)
            .collect::<Vec<_>>(),
        vec![0, 0]
    );
}

#[tokio::test]
async fn list_models_skips_records_without_usable_ids() {
    let (backend, _) = start_mock(vec![MockResponse::ok(page(
        vec![
            json!({"type": "model", "created_at": "2026-07-24T00:00:00Z"}),
            json!({"type": "model", "id": null, "created_at": "2026-07-24T00:00:00Z"}),
            json!({"type": "model", "id": "", "created_at": "2026-07-24T00:00:00Z"}),
            json!({"type": "model", "id": 42, "created_at": "2026-07-24T00:00:00Z"}),
            json!("not-a-model-record"),
            Value::Null,
            model("usable", Some("2026-07-24T00:00:00Z")),
        ],
        false,
        None,
    ))])
    .await;

    let response = backend.list_models().await.expect("resilient model list");

    assert_eq!(response.data.len(), 1);
    assert_eq!(response.data[0].id, "usable");
}

#[tokio::test]
async fn list_models_returns_useful_pages_when_a_later_page_fails() {
    let (backend, state) = start_mock(vec![
        MockResponse::ok(page(
            vec![model("usable", Some("2026-07-24T00:00:00Z"))],
            true,
            Some("page-1"),
        )),
        MockResponse {
            status: StatusCode::SERVICE_UNAVAILABLE,
            body: json!({
                "type": "error",
                "error": {"type": "overloaded_error", "message": "try again"}
            }),
            headers: Vec::new(),
        },
    ])
    .await;

    let response = backend.list_models().await.expect("useful partial catalog");

    assert_eq!(response.data.len(), 1);
    assert_eq!(response.data[0].id, "usable");
    assert!(!response.has_more);
    assert_eq!(state.requests.lock().await.len(), 2);
}

#[tokio::test]
async fn list_models_rejects_pages_missing_required_control_fields() {
    for malformed_page in [
        json!({"has_more": false, "last_id": null}),
        json!({"data": [], "last_id": null}),
    ] {
        let (backend, _) = start_mock(vec![MockResponse::ok(malformed_page)]).await;
        assert!(matches!(
            backend.list_models().await,
            Err(BackendError::ApiError { status: 502, .. })
        ));
    }
}

#[tokio::test]
async fn list_models_rejects_missing_and_repeated_pagination_cursors() {
    let (missing_backend, _) = start_mock(vec![MockResponse::ok(page(vec![], true, None))]).await;
    let missing_error = missing_backend
        .list_models()
        .await
        .expect_err("missing cursor");
    assert!(missing_error
        .to_string()
        .contains("without a last_id cursor"));

    let (repeated_backend, state) = start_mock(vec![
        MockResponse::ok(page(vec![], true, Some("cursor-a"))),
        MockResponse::ok(page(vec![], true, Some("cursor-b"))),
        MockResponse::ok(page(vec![], true, Some("cursor-a"))),
    ])
    .await;
    let repeated_error = repeated_backend
        .list_models()
        .await
        .expect_err("repeated cursor");
    assert!(repeated_error
        .to_string()
        .contains("repeated pagination cursor 'cursor-a'"));
    assert_eq!(state.requests.lock().await.len(), 3);
}

#[tokio::test]
async fn list_models_returns_partial_catalogs_at_page_and_record_caps() {
    let pages = (0..MAX_MODEL_PAGES)
        .map(|index| {
            MockResponse::ok(page(
                vec![model(
                    &format!("page-model-{index}"),
                    Some("2026-07-24T00:00:00Z"),
                )],
                true,
                Some(&format!("cursor-{index}")),
            ))
        })
        .collect();
    let (page_backend, state) = start_mock(pages).await;
    let page_response = page_backend
        .list_models()
        .await
        .expect("partial page-limited catalog");
    assert!(!page_response.has_more);
    assert_eq!(page_response.data.len(), MAX_MODEL_PAGES);
    assert_eq!(page_response.data.first().unwrap().id, "page-model-0");
    assert_eq!(page_response.data.last().unwrap().id, "page-model-31");
    assert_eq!(state.requests.lock().await.len(), MAX_MODEL_PAGES);

    let router_pages = (0..MAX_MODEL_PAGES)
        .map(|index| {
            MockResponse::ok(page(
                vec![model(
                    &format!("router-page-model-{index}"),
                    Some("2026-07-24T00:00:00Z"),
                )],
                true,
                Some(&format!("router-cursor-{index}")),
            ))
        })
        .collect();
    let (base_url, router_state) = start_mock_server(router_pages).await;
    let connections: Arc<dyn ProviderConnectionStore> =
        Arc::new(InMemoryProviderConnectionStore::new());
    connections
        .create(ConnectionInput {
            name: "anthropic".to_string(),
            provider_kind: "anthropic".to_string(),
            base_url: Some(base_url),
            prefix: Some("anthropic/".to_string()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "test-key".to_string(),
        })
        .await
        .expect("store connection");
    let router = RouterBackend::new(
        reqwest::Client::new(),
        connections,
        Arc::new(InMemoryModelRouteStore::new()),
    );
    let router_response = router.list_models().await.expect("router partial catalog");
    assert!(!router_response.has_more);
    assert_eq!(router_response.data.len(), MAX_MODEL_PAGES);
    let capabilities = router_response.data[0]
        .capabilities
        .as_ref()
        .expect("router attaches a public capability profile");
    assert_eq!(
        capabilities
            .operation(Protocol::AnthropicMessages, ProtocolOperation::Create)
            .expect("messages create")
            .mode,
        DeliveryMode::Native
    );
    assert_eq!(router_state.requests.lock().await.len(), MAX_MODEL_PAGES);

    let models = (0..=MAX_MODELS)
        .map(|index| model(&format!("model-{index}"), None))
        .collect();
    let (model_backend, state) =
        start_mock(vec![MockResponse::ok(page(models, false, None))]).await;
    let model_response = model_backend
        .list_models()
        .await
        .expect("partial record-limited catalog");
    assert!(!model_response.has_more);
    assert_eq!(model_response.data.len(), MAX_MODELS);
    assert_eq!(model_response.data.first().unwrap().id, "model-0");
    assert_eq!(model_response.data.last().unwrap().id, "model-9999");
    assert_eq!(state.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn get_model_resolves_an_alias_to_the_canonical_response_id() {
    let (backend, _) = start_mock(vec![MockResponse::ok(model(
        "different-model",
        Some("2026-07-24T00:00:00Z"),
    ))])
    .await;

    let response = backend
        .get_model("requested-model")
        .await
        .expect("alias lookup");

    assert_eq!(response.id, "different-model");
}

#[tokio::test]
async fn probe_models_fetches_only_the_first_page() {
    let (backend, state) = start_mock(vec![MockResponse::ok(page(
        vec![model("first-page-model", Some("2026-07-24T00:00:00Z"))],
        true,
        Some("next-page"),
    ))])
    .await;

    let response = backend.probe_models().await.expect("one-page probe");

    assert_eq!(response.data.len(), 1);
    assert!(!response.has_more);
    assert_eq!(state.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn exhausted_mock_scripts_return_an_explicit_error() {
    let (backend, _) = start_mock(vec![]).await;

    let error = backend.list_models().await.expect_err("script exhaustion");

    match error {
        BackendError::ApiError {
            message, status, ..
        } => {
            assert_eq!(status, 500);
            assert_eq!(message, "unexpected upstream request");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn model_requests_preserve_anthropic_errors_and_forwardable_headers() {
    let (backend, _) = start_mock(vec![MockResponse {
        status: StatusCode::TOO_MANY_REQUESTS,
        body: json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "slow down"}
        }),
        headers: vec![("retry-after", "2"), ("request-id", "req_123")],
    }])
    .await;

    let error = backend.list_models().await.expect_err("rate limit error");

    match error {
        BackendError::ApiError {
            message,
            status,
            headers,
            original: None,
            reasoning_incompatible: false,
        } => {
            assert_eq!(message, "slow down");
            assert_eq!(status, 429);
            assert!(headers.contains(&("retry-after".to_string(), "2".to_string())));
            assert!(headers.contains(&("request-id".to_string(), "req_123".to_string())));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}
