//! Gemini image embeddings through real authentication, routing and settlement.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    middleware,
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use openai_server::{
    api_keys::ApiKeyPermission,
    backend::RouterBackend,
    handlers,
    middleware::auth_middleware,
    model_routes::{InMemoryModelRouteStore, ModelRouteStore, RouteInput},
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
    AppState,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::support::{
    app::{AppStateBuilder, TEST_API_KEY},
    mock::serve,
    otel::{setup_otel, span_attrs},
};

const IMAGE: &str = "data:image/png;base64,aW1hZ2U=";

pub(super) async fn state(upstream: &str) -> AppState {
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    connections
        .create(ConnectionInput {
            name: "google".into(),
            provider_kind: "gemini".into(),
            base_url: Some(format!("{upstream}/custom/v1beta/openai")),
            prefix: Some("google/".into()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "saved-google-key".into(),
        })
        .await
        .unwrap();
    AppStateBuilder::new(Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections,
        Arc::new(InMemoryModelRouteStore::new()),
    )))
    .build()
    .await
}

pub(super) async fn send(state: AppState, body: Value) -> (StatusCode, HeaderMap, Value) {
    let app = Router::new()
        .route(
            "/v1/embeddings",
            post(handlers::embeddings::create_embedding),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);
    let response = app
        .oneshot(
            Request::post("/v1/embeddings")
                .header("authorization", format!("Bearer {TEST_API_KEY}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn gemini_embedding_limits_are_published_for_custom_prefixes_and_route_aliases() {
    let upstream = serve(Router::new().route(
        "/custom/v1beta/openai/models",
        get(|| async {
            Json(json!({"object": "list", "data": [
                {"id": "models/gemini-embedding-2", "object": "model"},
                {"id": "models/gemini-embedding-2-preview", "object": "model"},
                {"id": "models/gemini-embedding-2-custom", "object": "model"},
                {"id": "models/gemini-embedding-001", "object": "model"}
            ]}))
        }),
    ))
    .await;
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    let connection = connections
        .create(ConnectionInput {
            name: "research-google".into(),
            provider_kind: "gemini".into(),
            base_url: Some(format!("{upstream}/custom/v1beta/openai")),
            prefix: Some("research/".into()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "saved-google-key".into(),
        })
        .await
        .unwrap();
    let routes = Arc::new(InMemoryModelRouteStore::new());
    routes
        .create(RouteInput {
            routing: Default::default(),
            public_model_id: "chalk/visual-search".into(),
            connection_id: connection.id,
            upstream_model_id: "gemini-embedding-2".into(),
            published: true,
        })
        .await
        .unwrap();
    let state = AppStateBuilder::new(Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections,
        routes,
    )))
    .build()
    .await;
    let app = Router::new()
        .route("/v1/models", get(handlers::models::list_models))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);
    let response = app
        .oneshot(
            Request::get("/v1/models")
                .header("authorization", format!("Bearer {TEST_API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let catalog: Value = serde_json::from_slice(&body).unwrap();
    let data = catalog["data"].as_array().unwrap();
    for model in [
        "research/models/gemini-embedding-2",
        "research/models/gemini-embedding-2-preview",
        "chalk/visual-search",
    ] {
        let limits =
            &data.iter().find(|entry| entry["id"] == model).unwrap()["capabilities"]["limits"];
        assert_eq!(limits["max_input_tokens"], 8192, "{model}");
        assert_eq!(
            limits["embedding_input"],
            json!({
                "max_images_per_document": 6,
                "image_mime_types": ["image/png", "image/jpeg"]
            }),
            "{model}"
        );
    }
    for model in [
        "research/models/gemini-embedding-2-custom",
        "research/models/gemini-embedding-001",
    ] {
        let limits =
            &data.iter().find(|entry| entry["id"] == model).unwrap()["capabilities"]["limits"];
        assert!(limits.get("embedding_input").is_none(), "{model}");
    }
}

#[tokio::test]
async fn gemini_images_combine_parts_and_account_reported_usage() {
    let calls = Arc::new(AtomicUsize::new(0));
    let upstream_calls = calls.clone();
    let upstream = serve(Router::new().route("/custom/v1beta/models/gemini-embedding-2:embedContent", post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let calls = upstream_calls.clone();
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(headers["x-goog-api-key"], "saved-google-key");
            assert!(!headers.contains_key("authorization"));
            assert_eq!(body, json!({"model":"models/gemini-embedding-2", "outputDimensionality":768,
                "content":{"parts":[{"text":"a dog"}, {"inlineData":{"mimeType":"image/png", "data":"aW1hZ2U="}}]}}));
            Json(json!({"embedding":{"values":[0.5, -0.25]}, "usageMetadata":{"promptTokenCount":258}}))
        }
    }))).await;
    let state = state(&upstream).await;
    let route = state
        .backend
        .resolve("google/gemini-embedding-2")
        .await
        .unwrap();
    let metadata = serde_json::to_value(&route.capabilities).unwrap();
    assert_eq!(metadata["modalities"]["input"]["image"], "supported");
    use openai_server::capabilities::{DeliveryMode, Interface, InterfaceOperation, OperationKey};
    assert_eq!(
        route.capabilities.delivery[&OperationKey {
            interface: Interface::OpenaiEmbeddings,
            operation: InterfaceOperation::Create,
        }],
        DeliveryMode::Adapted
    );

    let (provider, exporter, subscriber) = setup_otel();
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let (status, _, body) = send(state, json!({"model":"google/models/gemini-embedding-2", "dimensions":768,
            "input":[{"type":"text", "text":"a dog"}, {"type":"image_url", "image_url":{"url":IMAGE}}]})).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body["data"],
            json!([{"object":"embedding", "index":0, "embedding":[0.5, -0.25]}])
        );
        assert_eq!(body["usage"]["prompt_tokens"], 258);
        assert_eq!(body["usage"]["total_tokens"], 258);
    }
    for result in provider.force_flush() {
        result.unwrap();
    }
    let spans = exporter.get_finished_spans().unwrap();
    let spans: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "embedding.create")
        .collect();
    assert_eq!(spans.len(), 1);
    let attrs = span_attrs(spans[0]);
    assert_eq!(attrs["llm.token_count.prompt"], "258");
    assert_eq!(attrs["embedding.input_count"], "1");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn gemini_private_images_are_denied_but_inline_images_use_native_api() {
    let fetches = Arc::new(AtomicUsize::new(0));
    let image_fetches = fetches.clone();
    let upstream = serve(Router::new()
        .route("/photo.jpg", get(move || {
            image_fetches.fetch_add(1, Ordering::SeqCst);
            async { ([("content-type", "image/jpeg")], b"image") }
        }))
        .route("/custom/v1beta/models/gemini-embedding-2-preview:embedContent", post(|Json(body): Json<Value>| async move {
            assert_eq!(body["content"]["parts"], json!([{"inlineData":{"mimeType":"image/jpeg", "data":"aW1hZ2U="}}]));
            assert!(body.get("outputDimensionality").is_none());
            Json(json!({"embedding":{"values":[1.0, -0.5]}, "usageMetadata":{"promptTokenCount":258}}))
        }))).await;
    let state = state(&upstream).await;
    // This environment trusts the operator's private upstream, but its callers
    // must not inherit that trust for image downloads.
    assert!(!state.connection_policy.public_egress_only());
    let request = |url| {
        json!({"model":"google/gemini-embedding-2-preview", "encoding_format":"base64",
        "input":[{"type":"image_url", "image_url":{"url":url}}]})
    };
    for policy in [
        openai_server::provider_connections::ConnectionPolicy::trusted(),
        openai_server::provider_connections::ConnectionPolicy::isolated(),
    ] {
        let mut request_state = state.clone();
        request_state.connection_policy = policy;
        let (status, _, body) = send(request_state, request(format!("{upstream}/photo.jpg"))).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
    }
    let (status, _, body) = send(
        state,
        request("data:image/jpeg;base64,aW1hZ2U=".to_string()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"][0]["embedding"], "AACAPwAAAL8=");
}

/// Data URIs are case-insensitive, may carry parameters and may be MIME-wrapped.
#[tokio::test]
async fn gemini_data_uris_accept_any_case_parameters_and_line_wrapping() {
    let upstream = serve(Router::new().route(
        "/custom/v1beta/models/gemini-embedding-2:embedContent",
        post(|Json(body): Json<Value>| async move {
            assert_eq!(
                body["content"]["parts"],
                json!([{"inlineData":{"mimeType":"image/png", "data":"aW1hZ2U="}}])
            );
            Json(json!({"embedding":{"values":[0.5]}, "usageMetadata":{"promptTokenCount":258}}))
        }),
    ))
    .await;
    let state = state(&upstream).await;
    for url in [
        "data:image/png;base64,aW1hZ2U=",
        "DATA:IMAGE/PNG;charset=utf-8;BASE64,aW1h\r\nZ2U=",
        "data:image/png;base64,\taW1h Z2U=\r\n\x0c",
    ] {
        let (status, _, body) = send(
            state.clone(),
            json!({"model":"google/gemini-embedding-2", "input":[{"type":"image_url",
                "image_url":{"url":url}}]}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }
}

#[tokio::test]
async fn gemini_inline_jpg_is_normalized_to_jpeg() {
    let upstream = serve(Router::new().route(
        "/custom/v1beta/models/gemini-embedding-2:embedContent",
        post(|Json(body): Json<Value>| async move {
            assert_eq!(
                body["content"]["parts"],
                json!([{"inlineData":{"mimeType":"image/jpeg", "data":"aW1hZ2U="}}])
            );
            Json(json!({"embedding":{"values":[0.5]}, "usageMetadata":{"promptTokenCount":258}}))
        }),
    ))
    .await;
    let (status, _, body) = send(
        state(&upstream).await,
        json!({"model":"google/gemini-embedding-2", "input":[{"type":"image_url",
            "image_url":{"url":"DATA:IMAGE/JPG;charset=utf-8;BASE64,aW1hZ2U="}}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn gemini_embedding_traces_price_reported_modalities_and_omit_unknown_costs() {
    for (input, metadata, cost) in [
        (
            json!("hello"),
            json!({"promptTokenCount":7}),
            Some(0.0000014),
        ),
        (
            json!([{"type":"image_url", "image_url":{"url":IMAGE}}]),
            json!({"promptTokenCount":258, "promptTokenDetails":[{"modality":"IMAGE", "tokenCount":258}]}),
            Some(0.0001161),
        ),
        (
            json!([{"type":"text", "text":"a dog"}, {"type":"image_url", "image_url":{"url":IMAGE}}]),
            json!({"promptTokenCount":265, "promptTokenDetails":[{"modality":"TEXT", "tokenCount":7}, {"modality":"IMAGE", "tokenCount":258}]}),
            Some(0.0001175),
        ),
        (
            json!([{"type":"image_url", "image_url":{"url":IMAGE}}]),
            json!({"promptTokenCount":258}),
            None,
        ),
        (
            json!([{"type":"text", "text":"a dog"}, {"type":"image_url", "image_url":{"url":IMAGE}}]),
            json!({"promptTokenCount":265, "promptTokenDetails":[{"modality":"IMAGE", "tokenCount":258}]}),
            None,
        ),
    ] {
        let total = metadata["promptTokenCount"].as_u64().unwrap();
        let upstream = serve(Router::new().route(
            "/custom/v1beta/models/gemini-embedding-2:embedContent",
            post(move || {
                let metadata = metadata.clone();
                async move { Json(json!({"embedding":{"values":[0.5]}, "usageMetadata":metadata})) }
            }),
        ))
        .await;
        let state = state(&upstream).await;
        let (provider, exporter, subscriber) = setup_otel();
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let (status, _, body) = send(
                state,
                json!({"model":"google/gemini-embedding-2", "input":input}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                body["usage"],
                json!({"prompt_tokens":total, "total_tokens":total})
            );
        }
        for result in provider.force_flush() {
            result.unwrap();
        }
        let spans = exporter.get_finished_spans().unwrap();
        let attrs = span_attrs(
            spans
                .iter()
                .find(|span| span.name == "embedding.create")
                .unwrap(),
        );
        assert_eq!(attrs["llm.token_count.prompt"], total.to_string());
        match cost {
            Some(expected) => {
                let actual: f64 = attrs["llm.cost.total"].parse().unwrap();
                assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
                assert_eq!(attrs["chalk.cost.source"], "estimated");
            }
            None => {
                assert!(!attrs.contains_key("llm.cost.total"));
                assert!(!attrs.contains_key("llm.cost.prompt"));
                assert!(!attrs.contains_key("llm.cost.completion"));
            }
        }
    }
}

#[tokio::test]
async fn gemini_text_batch_preserves_order_and_usage() {
    let upstream = serve(Router::new().route("/custom/v1beta/models/gemini-embedding-2:batchEmbedContents", post(|Json(body): Json<Value>| async move {
        assert_eq!(body, json!({"requests":[
            {"model":"models/gemini-embedding-2", "content":{"parts":[{"text":"first"}]}},
            {"model":"models/gemini-embedding-2", "content":{"parts":[{"text":"second"}]}}
        ]}));
        Json(json!({"embeddings":[{"values":[0.5]}, {"values":[0.25]}], "usageMetadata":{"promptTokenCount":4}}))
    }))).await;
    let (status, _, body) = send(
        state(&upstream).await,
        json!({"model":"google/gemini-embedding-2", "input":["first", "second"]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["data"],
        json!([
            {"object":"embedding", "index":0, "embedding":[0.5]},
            {"object":"embedding", "index":1, "embedding":[0.25]}
        ])
    );
    assert_eq!(body["usage"]["total_tokens"], 4);
}

#[tokio::test]
async fn gemini_single_text_embedding_can_estimate_missing_usage() {
    let upstream = serve(Router::new().route(
        "/custom/v1beta/models/gemini-embedding-2:embedContent",
        post(|Json(body): Json<Value>| async move {
            assert_eq!(
                body,
                json!({"model":"models/gemini-embedding-2", "content":{"parts":[{"text":"hello"}]}})
            );
            Json(json!({"embedding":{"values":[0.5]}}))
        }),
    ))
    .await;
    let (status, _, body) = send(
        state(&upstream).await,
        json!({"model":"google/gemini-embedding-2", "input":"hello"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"][0]["embedding"], json!([0.5]));
    assert!(body.get("usage").is_none());
}

#[tokio::test]
async fn gemini_embedding_validation_and_permission_reject_before_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let upstream_calls = calls.clone();
    let upstream = serve(Router::new().fallback(move || {
        upstream_calls.fetch_add(1, Ordering::SeqCst);
        async { StatusCode::INTERNAL_SERVER_ERROR }
    }))
    .await;
    let state = state(&upstream).await;
    for fields in [
        json!({"input":[1,2]}),
        json!({"input":[[1,2]]}),
        json!({"input":[]}),
        json!({"input":"hi", "dimensions":0}),
        json!({"input":"hi", "encoding_format":"invalid"}),
        json!({"input":"hi", "unknown_option":true}),
        json!({"input":[{"type":"image_url", "image_url":{"url":"data:image/png;base64,broken"}}]}),
        json!({"input":[{"type":"image_url", "image_url":{"url":"data:image/png;base64,Zh=="}}]}),
        json!({"input":[{"type":"image_url", "image_url":{"url":"data:image/png;base64,Zg==AAAA"}}]}),
        json!({"input":[{"type":"image_url", "image_url":{"url":format!("data:image/png;base64,{}AAAA", STANDARD.encode(vec![42; 767]))}}]}),
        json!({"input":[{"type":"image_url", "image_url":{"url":"data:image/png;base64,aGk=\u{a0}"}}]}),
        json!({"input":[{"type":"image_url", "image_url":{"url":"data:image/png;base64, \r\n"}}]}),
        json!({"input":[{"type":"image_url", "image_url":{"url":"data:image/bin;base64,aGk="}}]}),
        json!({"input":[{"type":"image_url", "image_url":{"url":"data:image/png;base64"}}]}),
        json!({"input":[{"type":"image_url", "image_url":{"url":"data:image/png,aGk="}}]}),
        json!({"input":[{"type":"image_url", "image_url":{"url":"data:text/plain;base64,aGk="}}]}),
        json!({"input":[{"type":"image_url", "image_url":{"url":"file:///etc/passwd"}}]}),
    ] {
        let mut body = fields;
        body["model"] = json!("google/gemini-embedding-2");
        let (status, _, body) = send(state.clone(), body).await;
        assert_eq!(status, 400, "{body}");
    }
    // Text-only models must still reject image input at the capability gate.
    let (status, _, body) = send(
        state.clone(),
        json!({"model":"google/gemini-embedding-001",
        "input":[{"type":"image_url", "image_url":{"url":IMAGE}}]}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let key = state
        .api_key_store
        .authenticate(TEST_API_KEY)
        .await
        .unwrap();
    state
        .api_key_store
        .update(
            &key.key_hash,
            None,
            None,
            Some(ApiKeyPermission {
                provider: Some("openai".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    let (status, _, body) = send(
        state,
        json!({"model":"google/gemini-embedding-2", "input":"hi"}),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn gemini_image_embedding_missing_usage_and_malformed_vectors_fail_closed() {
    for response in [
        json!({"embedding":{"values":[0.5]}}),
        json!({"embedding":{"values":[0.5]}, "usageMetadata":{"promptTokenCount":-1}}),
        json!({"embedding":{"values":[]}, "usageMetadata":{"promptTokenCount":258}}),
        json!({"embeddings":[{"values":[0.5]}], "usageMetadata":{"promptTokenCount":258}}),
    ] {
        let upstream = serve(Router::new().route(
            "/custom/v1beta/models/gemini-embedding-2:embedContent",
            post(move || {
                let response = response.clone();
                async move { Json(response) }
            }),
        ))
        .await;
        let (status, _, body) = send(
            state(&upstream).await,
            json!({"model":"google/gemini-embedding-2",
            "input":[{"type":"image_url", "image_url":{"url":IMAGE}}]}),
        )
        .await;
        assert!(status.is_server_error(), "{status}: {body}");
    }
}

#[tokio::test]
async fn gemini_embedding_upstream_errors_keep_status_message_and_retry_headers() {
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let upstream_calls = calls.clone();
        let upstream = serve(Router::new().route("/custom/v1beta/models/gemini-embedding-2:embedContent", post(move || {
            upstream_calls.fetch_add(1, Ordering::SeqCst);
            async move { (status, [("retry-after", "7")], Json(json!({"error":{"code":status.as_u16(), "message":"Google unavailable"}}))) }
        }))).await;
        let (actual, headers, body) = send(
            state(&upstream).await,
            json!({"model":"google/gemini-embedding-2", "input":"hi"}),
        )
        .await;
        assert_eq!(actual, status, "{body}");
        assert_eq!(headers["retry-after"], "7");
        assert_eq!(body["error"]["message"], "Google unavailable");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
