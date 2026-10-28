//! Gemini image requests use bare model IDs and the advertised compatible parameters.

use std::sync::Arc;

use axum::{
    http::HeaderMap,
    routing::{get, post},
    Json, Router,
};
use openai_server::{
    backend::{BackendProvider, RouterBackend},
    model_routes::InMemoryModelRouteStore,
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
    types::{CreateImageRequest, ImageResponse},
};
use serde_json::{json, Value};

use super::support::{
    app::{build_full_app, TEST_API_KEY},
    mock::serve,
};

/// Follow a discovered model through real routing, auth and image dispatch.
#[tokio::test]
async fn gemini_catalog_models_use_bare_image_model_ids_and_compatible_defaults() {
    let upstream = serve(Router::new()
        .route("/v1beta/openai/models", get(|| async {
            Json(json!({"object": "list", "data": [
                {"id": "models/gemini-2.5-flash-image", "object": "model", "owned_by": "google"},
                {"id": "models/gemini-3-pro-image", "object": "model", "owned_by": "google"}
            ]}))
        }))
        .route("/v1beta/openai/images/generations", post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer upstream-test-key");
            match body["model"].as_str().unwrap() {
                "gemini-2.5-flash-image" => assert_eq!(body, json!({"model": "gemini-2.5-flash-image", "prompt": "a blue circle"})),
                "gemini-3-pro-image" => assert_eq!(body, json!({"model": "gemini-3-pro-image", "prompt": "a blue circle", "size": "1024x1024"})),
                model => panic!("Unexpected upstream model: {model}"),
            }
            Json(json!({"data": [{"b64_json": "aW1hZ2U="}]}))
        }))).await;
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    connections
        .create(ConnectionInput {
            name: "test-google".into(),
            provider_kind: "gemini".into(),
            base_url: Some(format!("{upstream}/v1beta/openai")),
            prefix: Some("google-test/".into()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "upstream-test-key".into(),
        })
        .await
        .unwrap();
    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections,
        Arc::new(InMemoryModelRouteStore::new()),
    ));
    let catalog = backend.list_models().await.unwrap();
    let app = serve(build_full_app(backend).await).await;
    let client = reqwest::Client::new();
    for upstream_model in ["gemini-2.5-flash-image", "gemini-3-pro-image"] {
        let public_id = format!("google-test/models/{upstream_model}");
        let discovered = catalog.data.iter().find(|m| m.id == public_id).unwrap();
        let metadata = serde_json::to_value(discovered).unwrap();
        let parameters = &metadata["capabilities"]["interfaces"]["openai_images"]["operations"]
            ["generate"]["parameters"];
        assert!(parameters.get("image_size").is_none());
        assert!(parameters.get("aspect_ratio").is_none());
        // The playground selects the first advertised size. Exercise that
        // payload rather than a minimal request that omits all UI defaults.
        let default_size = parameters["size"]["constraint"]["values"]
            .as_array()
            .and_then(|values| values.first());
        for model in [public_id, format!("google-test/{upstream_model}")] {
            let mut body = json!({"model": model, "prompt": "a blue circle"});
            if let Some(size) = default_size {
                body["size"] = size.clone();
            }
            let response = client
                .post(format!("{app}/v1/images/generations"))
                .bearer_auth(TEST_API_KEY)
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200, "image request for {model}");
            let image: ImageResponse = response.json().await.unwrap();
            assert_eq!(image.data[0].b64_json.as_deref(), Some("aW1hZ2U="));
        }
    }
}

/// A custom provider named Gemini must keep its own model-ID contract.
#[tokio::test]
async fn non_gemini_image_model_resource_names_are_preserved() {
    let upstream = serve(Router::new().route(
        "/images/generations",
        post(|Json(body): Json<Value>| async move {
            assert_eq!(body["model"], "models/custom-image");
            Json(json!({"data": [{"b64_json": "aW1hZ2U="}]}))
        }),
    ))
    .await;
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    connections
        .create(ConnectionInput {
            name: "gemini".into(),
            provider_kind: "openai-compatible".into(),
            base_url: Some(upstream),
            prefix: Some("custom/".into()),
            exposure: ExposurePolicy::Unlisted,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "test-key".into(),
        })
        .await
        .unwrap();
    let router = RouterBackend::new(
        reqwest::Client::new(),
        connections,
        Arc::new(InMemoryModelRouteStore::new()),
    );
    let route = router.resolve("custom/models/custom-image").await.unwrap();
    let request: CreateImageRequest =
        serde_json::from_value(json!({"model": route.stripped_model, "prompt": "a blue circle"}))
            .unwrap();
    let response = route.backend.create_image(request).await.unwrap();
    assert_eq!(response.data[0].b64_json.as_deref(), Some("aW1hZ2U="));
}
