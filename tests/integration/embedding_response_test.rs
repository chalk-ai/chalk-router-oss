use std::sync::Arc;

use axum::{body::Body, http::Request, routing::post, Json, Router};
use openai_server::{
    backend::RouterBackend,
    model_routes::InMemoryModelRouteStore,
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::support::{
    app::{build_full_app, TEST_API_KEY},
    mock::serve,
};

#[tokio::test]
async fn gemini_embeddings_accept_missing_metadata_and_preserve_reported_metadata() {
    for (upstream, indexes) in [
        // Shape observed from Gemini: neither per-entry index nor usage.
        (
            json!({"object": "list", "model": "gemini-embedding-001", "data": [
                {"object": "embedding", "embedding": [0.5]},
                {"object": "embedding", "embedding": [0.25]}
            ]}),
            [0, 1],
        ),
        (
            json!({"object": "list", "model": "gemini-embedding-001", "data": [
            {"object": "embedding", "index": 1, "embedding": "AACAPw=="},
            {"object": "embedding", "index": 0, "embedding": [0.5]}
        ], "usage": {"prompt_tokens": 5, "total_tokens": 5}}),
            [1, 0],
        ),
    ] {
        let fixture = upstream.clone();
        let base_url = serve(Router::new().route(
            "/embeddings",
            post(move |Json(request): Json<Value>| {
                let fixture = fixture.clone();
                async move {
                    assert_eq!(request["model"], "gemini-embedding-001");
                    assert!(request.get("user").is_none());
                    Json(fixture)
                }
            }),
        ))
        .await;
        let connections = Arc::new(InMemoryProviderConnectionStore::new());
        connections
            .create(ConnectionInput {
                name: "gemini".into(),
                provider_kind: "gemini".into(),
                base_url: Some(base_url),
                prefix: Some("gemini/".into()),
                exposure: ExposurePolicy::Dynamic,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "test-key".into(),
            })
            .await
            .unwrap();
        let backend = Arc::new(RouterBackend::new(
            reqwest::Client::new(),
            connections,
            Arc::new(InMemoryModelRouteStore::new()),
        ));
        let response = build_full_app(backend)
            .await
            .oneshot(
                Request::post("/v1/embeddings")
                    .header("authorization", format!("Bearer {TEST_API_KEY}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "gemini/gemini-embedding-001", "input": ["hello", "world"]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(status, 200, "{body}");
        assert_eq!(body.get("usage"), upstream.get("usage"));
        for (position, index) in indexes.into_iter().enumerate() {
            assert_eq!(body["data"][position]["index"], index);
            assert_eq!(
                body["data"][position]["embedding"],
                upstream["data"][position]["embedding"]
            );
        }
    }
}
