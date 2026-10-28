//! OpenAI compatibility endpoints without an implementation return 501 with
//! `error.type == "not_implemented"` instead of fabricated success responses.

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::{delete, get, post},
    Router,
};
use openai_server::{
    backend::{BackendError, BackendProvider},
    types::{
        ChatCompletionRequest, ChatCompletionResponse, CreateEmbeddingRequest,
        CreateEmbeddingResponse, CreateImageRequest, CreateModerationRequest,
        CreateModerationResponse, CreateSpeechRequest, ImageResponse, ListResponse, Model,
        TranscriptionResponse,
    },
    AppState,
};
use tower::ServiceExt;

use crate::support::app::AppStateBuilder;

/// None of these methods are reachable from the routes under test — every
/// route mounted below returns 501 before ever consulting the backend.
#[derive(Clone, Default)]
struct MockBackend;

#[async_trait]
impl BackendProvider for MockBackend {
    async fn create_chat_completion(
        &self,
        _request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn create_embedding(
        &self,
        _request: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn create_image(
        &self,
        _request: CreateImageRequest,
    ) -> Result<ImageResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn create_speech(&self, _r: CreateSpeechRequest) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn create_transcription(
        &self,
        _model: String,
        _b: bytes::Bytes,
        _fname: String,
        _lang: Option<String>,
        _prompt: Option<String>,
        _fmt: Option<String>,
        _temp: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn create_translation(
        &self,
        _model: String,
        _b: bytes::Bytes,
        _fname: String,
        _prompt: Option<String>,
        _fmt: Option<String>,
        _temp: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn create_moderation(
        &self,
        _r: CreateModerationRequest,
    ) -> Result<CreateModerationResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn get_model(&self, _id: &str) -> Result<Model, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    fn name(&self) -> &str {
        "mock"
    }
}

async fn test_state() -> AppState {
    // No route mounted below extracts `ResolvedApiKey` or applies auth
    // middleware, so the default seeded api-key store is inert here.
    AppStateBuilder::new(Arc::new(MockBackend)).build().await
}

/// Mounts only the routes under test, mirroring the paths/methods registered
/// in `main.rs`. None of these handlers extract `ResolvedApiKey`, so no auth
/// middleware is needed to drive them.
async fn test_router() -> Router {
    Router::new()
        .route(
            "/v1/batches",
            post(openai_server::handlers::batch::create_batch),
        )
        .route(
            "/v1/batches",
            get(openai_server::handlers::batch::list_batches),
        )
        .route(
            "/v1/fine_tuning/jobs",
            post(openai_server::handlers::fine_tuning::create_fine_tuning_job),
        )
        .route(
            "/v1/fine_tuning/jobs",
            get(openai_server::handlers::fine_tuning::list_fine_tuning_jobs),
        )
        .route(
            "/v1/assistants",
            get(openai_server::handlers::assistants::list_assistants),
        )
        .route(
            "/v1/assistants",
            post(openai_server::handlers::assistants::create_assistant),
        )
        .route(
            "/v1/assistants/:assistant_id",
            delete(openai_server::handlers::assistants::delete_assistant),
        )
        .route(
            "/v1/files",
            get(openai_server::handlers::files::list_files)
                .post(openai_server::handlers::files::upload_file),
        )
        .route(
            "/v1/files/:file_id",
            delete(openai_server::handlers::files::delete_file),
        )
        .route(
            "/v1/files/:file_id/content",
            get(openai_server::handlers::files::download_file),
        )
        .route(
            "/v1/images/edits",
            post(openai_server::handlers::images::create_image_edit),
        )
        .route(
            "/v1/images/variations",
            post(openai_server::handlers::images::create_image_variation),
        )
        .route(
            "/v1/moderations",
            post(openai_server::handlers::moderations::create_moderation),
        )
        .with_state(test_state().await)
}

/// Sends `req` through the router and asserts a 501 whose body is
/// `error.type == "not_implemented"` (the wire key for `Error::error_type`,
/// which carries `#[serde(rename = "type")]`).
async fn assert_not_implemented(router: Router, req: Request<Body>) {
    let response = router.oneshot(req).await.expect("request should succeed");
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should be readable");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("body should be JSON");
    assert_eq!(json["error"]["type"], "not_implemented");
}

fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request should build")
}

fn empty_request(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request should build")
}

#[tokio::test]
async fn batch_create_and_list_return_not_implemented() {
    assert_not_implemented(
        test_router().await,
        json_request(
            "POST",
            "/v1/batches",
            serde_json::json!({
                "input_file_id": "file-abc",
                "endpoint": "/v1/chat/completions",
                "completion_window": "24h",
            }),
        ),
    )
    .await;

    assert_not_implemented(test_router().await, empty_request("GET", "/v1/batches")).await;
}

#[tokio::test]
async fn fine_tuning_create_and_list_return_not_implemented() {
    assert_not_implemented(
        test_router().await,
        json_request(
            "POST",
            "/v1/fine_tuning/jobs",
            serde_json::json!({
                "model": "gpt-4o-mini",
                "training_file": "file-abc",
            }),
        ),
    )
    .await;

    assert_not_implemented(
        test_router().await,
        empty_request("GET", "/v1/fine_tuning/jobs"),
    )
    .await;
}

#[tokio::test]
async fn assistants_list_create_delete_return_not_implemented() {
    assert_not_implemented(test_router().await, empty_request("GET", "/v1/assistants")).await;

    assert_not_implemented(
        test_router().await,
        json_request(
            "POST",
            "/v1/assistants",
            serde_json::json!({ "model": "gpt-4o-mini" }),
        ),
    )
    .await;

    assert_not_implemented(
        test_router().await,
        empty_request("DELETE", "/v1/assistants/asst-123"),
    )
    .await;
}

#[tokio::test]
async fn files_list_delete_download_return_not_implemented() {
    assert_not_implemented(test_router().await, empty_request("GET", "/v1/files")).await;

    assert_not_implemented(
        test_router().await,
        empty_request("DELETE", "/v1/files/file-123"),
    )
    .await;

    assert_not_implemented(
        test_router().await,
        empty_request("GET", "/v1/files/file-123/content"),
    )
    .await;
}

#[tokio::test]
async fn image_edit_and_variation_return_not_implemented() {
    assert_not_implemented(
        test_router().await,
        json_request("POST", "/v1/images/edits", serde_json::json!({})),
    )
    .await;

    assert_not_implemented(
        test_router().await,
        json_request("POST", "/v1/images/variations", serde_json::json!({})),
    )
    .await;
}

#[tokio::test]
async fn uploads_are_rejected_without_reading_even_a_malformed_or_unbounded_body() {
    // Polling this stream would panic: disabled uploads must not parse, buffer,
    // wait for, or consume any of the body, including with no multipart header.
    let body = Body::from_stream(futures::stream::poll_fn(
        |_| -> std::task::Poll<Option<Result<bytes::Bytes, std::io::Error>>> {
            panic!("disabled upload read request body")
        },
    ));
    let request = Request::builder()
        .method("POST")
        .uri("/v1/files")
        .header("content-length", "18446744073709551615")
        .body(body)
        .unwrap();
    assert_not_implemented(test_router().await, request).await;
}

#[tokio::test]
async fn http_model_delete_never_mutates_routes_under_inference_or_admin_credentials() {
    use crate::support::app::TEST_API_KEY;
    use openai_server::model_routes::RouteInput;

    for anonymous_development in [false, true] {
        let mut state = test_state().await;
        state.admin_key = Some("synthetic-delete-test-admin".into());
        state.dangerously_permit_unauthenticated = anonymous_development;
        let created = state
            .model_route_store
            .create(RouteInput {
                public_model_id: "chalk/keep-this-route".into(),
                connection_id: "synthetic-connection".into(),
                upstream_model_id: "synthetic-model".into(),
                published: true,
                routing: None,
            })
            .await
            .unwrap();
        let app = Router::new()
            .route(
                "/v1/models/:model_id",
                delete(openai_server::handlers::models::delete_model),
            )
            .with_state(state.clone())
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                openai_server::middleware::auth_middleware,
            ));

        for credential in [
            None,
            Some("invalid"),
            Some(TEST_API_KEY),
            Some("synthetic-delete-test-admin"),
        ] {
            for model in ["chalk%2Fkeep-this-route", "unknown-model"] {
                let mut req = Request::builder()
                    .method("DELETE")
                    .uri(format!("/v1/models/{model}"));
                if let Some(key) = credential {
                    req = req.header("authorization", format!("Bearer {key}"));
                }
                let response = app
                    .clone()
                    .oneshot(req.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                let allowed_by_auth = credential == Some(TEST_API_KEY)
                    || credential == Some("synthetic-delete-test-admin")
                    || (credential.is_none() && anonymous_development);
                assert_eq!(
                    response.status(),
                    if allowed_by_auth {
                        StatusCode::NOT_IMPLEMENTED
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                );
                if allowed_by_auth {
                    let body = axum::body::to_bytes(response.into_body(), 4096)
                        .await
                        .unwrap();
                    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(json["error"]["type"], "not_implemented");
                    assert!(json["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("DeleteModelRoute"));
                }
                let retained = state
                    .model_route_store
                    .get_by_public_id("chalk/keep-this-route")
                    .await
                    .unwrap()
                    .expect("route must survive every HTTP delete attempt");
                assert_eq!(retained.id, created.id);
            }
        }
    }
}
