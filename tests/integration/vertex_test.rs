//! End-to-end tests for the Vertex AI backend and its GCP token minting.
//!
//! Nothing leaves the machine: one local axum mock stands in for both Google's
//! OAuth2 token endpoint and the `*-aiplatform.googleapis.com` host. The
//! service account key under test points its `token_uri` at that mock — exactly
//! the field a real key uses — so the assertion the router signs is verified
//! here against a throwaway RSA public key.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use axum::{
    extract::{Path, RawQuery, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
    Form, Router,
};
use futures::StreamExt;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use openai_server::backend::{vertex_backend, BackendProvider, VertexBackend};
use openai_server::types::{
    ChatCompletionRequest, ChatMessage, CreateEmbeddingRequest, EmbeddingData, EmbeddingInput,
    EmbeddingInputItem, ImageUrl,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// Generated afresh in each test process, never persisted or sent to Google.
/// OpenSSL is a test prerequisite; no committed key or scanner exception is needed.
struct TestKeyPair {
    private_pem: String,
    public_pem: Vec<u8>,
}

fn test_key_pair() -> &'static TestKeyPair {
    static KEY: OnceLock<TestKeyPair> = OnceLock::new();
    KEY.get_or_init(|| {
        let generated = Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
            ])
            .stderr(Stdio::null())
            .output()
            .expect("Vertex tests require openssl on PATH");
        assert!(generated.status.success(), "test RSA key generation failed");
        let mut public = Command::new("openssl")
            .args(["pkey", "-pubout"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("Vertex tests require openssl on PATH");
        public
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(&generated.stdout)
            .expect("write ephemeral test key");
        let public = public.wait_with_output().expect("read public test key");
        assert!(public.status.success(), "test public key derivation failed");
        TestKeyPair {
            private_pem: String::from_utf8(generated.stdout).expect("PEM is UTF-8"),
            public_pem: public.stdout,
        }
    })
}

/// What the mock recorded from the last upstream (non-token) call.
#[derive(Default)]
struct CapturedCall {
    path: Option<String>,
    authorization: Option<String>,
    quota_project: Option<String>,
    body: Option<Value>,
}

struct MockState {
    /// Every form the token endpoint received, in order.
    token_requests: Mutex<Vec<HashMap<String, String>>>,
    token_hits: AtomicUsize,
    /// Lifetime the token endpoint claims for each token it mints.
    token_lifetime_secs: u64,
    captured: Mutex<CapturedCall>,
    /// When set, upstream calls answer with this status and body instead.
    upstream_error: Option<(u16, Value)>,
    /// When set, successful chat calls return malformed JSON.
    invalid_chat_response: bool,
    /// Every `:embedContent` body received, in arrival order.
    embed_content_bodies: Mutex<Vec<Value>>,
}

impl MockState {
    fn new(token_lifetime_secs: u64) -> Self {
        Self {
            token_requests: Mutex::new(Vec::new()),
            token_hits: AtomicUsize::new(0),
            token_lifetime_secs,
            captured: Mutex::new(CapturedCall::default()),
            upstream_error: None,
            invalid_chat_response: false,
            embed_content_bodies: Mutex::new(Vec::new()),
        }
    }
}

async fn handle_token(
    State(state): State<Arc<MockState>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let nth = state.token_hits.fetch_add(1, Ordering::SeqCst);
    state.token_requests.lock().await.push(form);
    axum::Json(json!({
        "access_token": format!("ya29.minted-{nth}"),
        "token_type": "Bearer",
        "expires_in": state.token_lifetime_secs,
    }))
    .into_response()
}

/// Stands in for the whole aiplatform host: records the path, auth headers and
/// body, then answers based on which API was called.
async fn handle_upstream(
    State(state): State<Arc<MockState>>,
    Path(path): Path<String>,
    query: Option<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let mut captured = state.captured.lock().await;
        captured.path = Some(format!("/{path}"));
        captured.authorization = header("authorization");
        captured.quota_project = header("x-goog-user-project");
        captured.body = Some(parsed.clone());
    }

    if let Some((status, body)) = &state.upstream_error {
        return (
            axum::http::StatusCode::from_u16(*status).expect("status"),
            axum::Json(body.clone()),
        )
            .into_response();
    }

    if path.ends_with(":predict") {
        return axum::Json(json!({
            "predictions": [
                {"embeddings": {"values": [0.5, -0.25], "statistics": {"token_count": 3}}},
                {"embeddings": {"values": [0.125, 0.75], "statistics": {"token_count": 4}}}
            ]
        }))
        .into_response();
    }

    if path.ends_with(":embedContent") {
        state.embed_content_bodies.lock().await.push(parsed.clone());
        // Echo the part count so a test can tell which document a vector
        // came from.
        let parts = parsed["content"]["parts"].as_array().map_or(0, Vec::len);
        let first_text = parsed["content"]["parts"][0]["text"].as_str().unwrap_or("");
        return axum::Json(json!({
            "embedding": {"values": [parts as f32, first_text.len() as f32]},
            "usageMetadata": {"promptTokenCount": 5}
        }))
        .into_response();
    }

    if path.ends_with("publishers/google/models") {
        // Two pages, so the listing has to follow nextPageToken to see the
        // whole catalog.
        let is_second_page = query
            .as_deref()
            .map(|q| q.contains("pageToken=page-2"))
            .unwrap_or(false);
        return if is_second_page {
            axum::Json(json!({
                "publisherModels": [
                    {"name": "publishers/google/models/text-embedding-005", "versionId": "default"}
                ]
            }))
            .into_response()
        } else {
            axum::Json(json!({
                "publisherModels": [
                    {"name": "publishers/google/models/gemini-2.5-pro", "versionId": "default"}
                ],
                "nextPageToken": "page-2"
            }))
            .into_response()
        };
    }

    if parsed
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return (
            [("content-type", "text/event-stream")],
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
        )
            .into_response();
    }

    if state.invalid_chat_response {
        return ([("content-type", "application/json")], "{not-json").into_response();
    }

    axum::Json(json!({
        "id": "chatcmpl-vertex",
        "object": "chat.completion",
        "created": 1,
        "model": "google/gemini-2.5-pro",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
    .into_response()
}

/// Boot the mock. Returns its base URL and the recorded state.
async fn start_mock(state: MockState) -> (String, Arc<MockState>) {
    let state = Arc::new(state);
    let app = Router::new()
        .route("/token", post(handle_token))
        .route("/*path", post(handle_upstream_post))
        .route("/*path", axum::routing::get(handle_upstream_get))
        .with_state(state.clone());

    let base = crate::support::mock::serve(app).await;
    (base, state)
}

/// POST bodies carry everything; no query string is involved.
async fn handle_upstream_post(
    state: State<Arc<MockState>>,
    path: Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    handle_upstream(state, path, None, headers, body).await
}

/// GET has no body; `list_models` is the only GET the backend makes, and its
/// page token rides in the query string.
async fn handle_upstream_get(
    state: State<Arc<MockState>>,
    path: Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    handle_upstream(state, path, query, headers, String::new()).await
}

/// A service account key whose `token_uri` points at the mock, as a real key
/// points at Google's.
fn service_account_key(base_url: &str, extra: Value) -> String {
    let mut key = json!({
        "type": "service_account",
        "project_id": "my-proj",
        "private_key_id": "key-1",
        "private_key": test_key_pair().private_pem,
        "client_email": "router@my-proj.iam.gserviceaccount.com",
        "token_uri": format!("{base_url}/token"),
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        key[k] = v;
    }
    key.to_string()
}

/// A Vertex backend whose credential mints against the mock and whose API host
/// is the mock.
fn backend_against(base_url: &str) -> VertexBackend {
    vertex_backend(
        reqwest::Client::new(),
        &service_account_key(base_url, json!({})),
        Some(base_url),
    )
    .expect("build vertex backend")
}

fn chat_request(model: &str, stream: bool) -> ChatCompletionRequest {
    ChatCompletionRequest {
        fallbacks: Default::default(),
        model: model.to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(json!("hello")),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: None,
        top_p: None,
        n: None,
        stream: Some(stream),
        stop: None,
        max_tokens: None,
        max_completion_tokens: Some(64),
        presence_penalty: None,
        frequency_penalty: None,
        logit_bias: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        stream_options: None,
        parallel_tool_calls: None,
        reasoning_effort: None,
    }
}

#[derive(Debug, Deserialize)]
struct AssertionClaims {
    iss: String,
    scope: String,
    aud: String,
    exp: u64,
    iat: u64,
}

#[tokio::test]
async fn the_service_account_key_is_exchanged_for_a_signed_jwt_bearer_assertion() {
    let (base_url, state) = start_mock(MockState::new(3600)).await;
    let backend = backend_against(&base_url);

    backend
        .create_chat_completion(chat_request("gemini-2.5-pro", false))
        .await
        .expect("chat completion against mock");

    let requests = state.token_requests.lock().await;
    assert_eq!(requests.len(), 1, "expected exactly one token exchange");
    let form = &requests[0];
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("urn:ietf:params:oauth:grant-type:jwt-bearer"),
        "must use the RFC 7523 JWT-bearer grant"
    );

    // The assertion must verify against the key's public half — this is the
    // whole point of taking the PEM.
    let assertion = form.get("assertion").expect("assertion in token request");
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[format!("{base_url}/token")]);
    let decoded = jsonwebtoken::decode::<AssertionClaims>(
        assertion,
        &DecodingKey::from_rsa_pem(&test_key_pair().public_pem).expect("public key"),
        &validation,
    )
    .expect("assertion must verify with the service account public key");

    assert_eq!(decoded.header.kid.as_deref(), Some("key-1"));
    assert_eq!(decoded.claims.iss, "router@my-proj.iam.gserviceaccount.com");
    assert_eq!(decoded.claims.aud, format!("{base_url}/token"));
    assert_eq!(
        decoded.claims.scope,
        "https://www.googleapis.com/auth/cloud-platform"
    );
    assert!(
        decoded.claims.exp > decoded.claims.iat,
        "assertion must be live but short-lived"
    );
    assert!(
        decoded.claims.exp - decoded.claims.iat <= 3600,
        "assertion lifetime must stay within the hour Google allows"
    );
}

#[tokio::test]
async fn the_minted_token_is_sent_upstream_and_reused_across_requests() {
    let (base_url, state) = start_mock(MockState::new(3600)).await;
    let backend = backend_against(&base_url);

    for _ in 0..3 {
        backend
            .create_chat_completion(chat_request("gemini-2.5-pro", false))
            .await
            .expect("chat completion against mock");
    }

    assert_eq!(
        state.token_hits.load(Ordering::SeqCst),
        1,
        "a live token must be reused, not re-minted per request"
    );

    let captured = state.captured.lock().await;
    assert_eq!(
        captured.authorization.as_deref(),
        Some("Bearer ya29.minted-0"),
        "the upstream call must carry the minted access token"
    );
    assert_eq!(
        captured.path.as_deref(),
        Some("/v1beta1/projects/my-proj/locations/us-central1/endpoints/openapi/chat/completions"),
        "chat must go to the project- and region-scoped OpenAI-compatible endpoint"
    );
    assert_eq!(
        captured.quota_project, None,
        "a service account must not send x-goog-user-project: it would additionally \
         require serviceusage.services.use on the project"
    );

    let body = captured.body.as_ref().expect("captured body");
    assert_eq!(
        body["model"], "google/gemini-2.5-pro",
        "a bare model name must be qualified with the google publisher"
    );
    assert_eq!(
        body["max_tokens"], 64,
        "Vertex takes max_tokens, not OpenAI's newer max_completion_tokens"
    );
    assert!(
        body.get("max_completion_tokens").is_none() || body["max_completion_tokens"].is_null(),
        "max_completion_tokens must not also be sent"
    );
}

#[tokio::test]
async fn low_max_tokens_parse_failure_mentions_possible_thinking_budget() {
    let mut state = MockState::new(3600);
    state.invalid_chat_response = true;
    let (base_url, state) = start_mock(state).await;
    let backend = backend_against(&base_url);
    let mut request = chat_request("gemini-2.5-pro", false);
    request.max_completion_tokens = Some(512);

    let error = backend
        .create_chat_completion(request)
        .await
        .err()
        .expect("malformed response must fail to parse")
        .to_string();
    assert!(
        error.contains("Failed to parse Vertex response:"),
        "{error}"
    );
    assert!(error.contains("error decoding response body"), "{error}");
    assert!(error.contains("thinking model"), "{error}");
    assert!(error.contains("internal reasoning"), "{error}");

    let captured = state.captured.lock().await;
    let body = captured.body.as_ref().expect("captured upstream request");
    assert_eq!(body["max_tokens"], 512);
    assert!(body["max_completion_tokens"].is_null());
}

#[tokio::test]
async fn high_max_tokens_parse_failure_has_no_thinking_budget_hint() {
    let mut state = MockState::new(3600);
    state.invalid_chat_response = true;
    let (base_url, _state) = start_mock(state).await;
    let backend = backend_against(&base_url);
    let mut request = chat_request("gemini-2.5-pro", false);
    request.max_completion_tokens = Some(4096);

    let error = backend
        .create_chat_completion(request)
        .await
        .err()
        .expect("malformed response must fail to parse")
        .to_string();
    assert!(
        error.contains("Failed to parse Vertex response:"),
        "{error}"
    );
    assert!(error.contains("error decoding response body"), "{error}");
    assert!(!error.contains("thinking model"), "{error}");
}

#[tokio::test]
async fn a_token_expiring_within_the_skew_window_is_refreshed() {
    // 30s of stated life is inside the refresh skew, so the cached token is
    // already unusable by the time the next request starts.
    let (base_url, state) = start_mock(MockState::new(30)).await;
    let backend = backend_against(&base_url);

    for _ in 0..2 {
        backend
            .create_chat_completion(chat_request("gemini-2.5-pro", false))
            .await
            .expect("chat completion against mock");
    }

    assert_eq!(
        state.token_hits.load(Ordering::SeqCst),
        2,
        "a token expiring inside the skew window must be re-minted"
    );
    assert_eq!(
        state.captured.lock().await.authorization.as_deref(),
        Some("Bearer ya29.minted-1"),
        "the second request must use the refreshed token"
    );
}

#[tokio::test]
async fn concurrent_first_requests_mint_one_token_between_them() {
    let (base_url, state) = start_mock(MockState::new(3600)).await;
    let backend = Arc::new(backend_against(&base_url));

    let calls = (0..8).map(|_| {
        let backend = backend.clone();
        async move {
            backend
                .create_chat_completion(chat_request("gemini-2.5-pro", false))
                .await
                .expect("chat completion against mock")
        }
    });
    futures::future::join_all(calls).await;

    assert_eq!(
        state.token_hits.load(Ordering::SeqCst),
        1,
        "a burst on a cold cache must mint once, not once per request"
    );
}

#[tokio::test]
async fn user_credentials_send_the_quota_project() {
    let (base_url, state) = start_mock(MockState::new(3600)).await;
    let credential = json!({
        "type": "authorized_user",
        "client_id": "cid",
        "client_secret": "csecret",
        "refresh_token": "rtoken",
        "quota_project_id": "my-proj",
        "token_uri": format!("{base_url}/token"),
    })
    .to_string();
    let backend = vertex_backend(reqwest::Client::new(), &credential, Some(&base_url))
        .expect("build vertex backend");

    backend
        .create_chat_completion(chat_request("gemini-2.5-pro", false))
        .await
        .expect("chat completion against mock");

    let form = state.token_requests.lock().await[0].clone();
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("refresh_token"),
        "user credentials refresh rather than sign an assertion"
    );
    assert_eq!(
        state.captured.lock().await.quota_project.as_deref(),
        Some("my-proj"),
        "aiplatform.googleapis.com rejects user credentials with no quota project"
    );
}

#[tokio::test]
async fn a_region_named_in_the_key_moves_the_whole_request() {
    let (base_url, state) = start_mock(MockState::new(3600)).await;
    let credential = service_account_key(&base_url, json!({"location": "europe-west4"}));
    // No base-URL override: the host would be europe-west4's in production. The
    // mock stands in for it here, so only the path is observable.
    let backend = vertex_backend(reqwest::Client::new(), &credential, Some(&base_url))
        .expect("build vertex backend");

    backend
        .create_chat_completion(chat_request("gemini-2.5-pro", false))
        .await
        .expect("chat completion against mock");

    assert_eq!(
        state.captured.lock().await.path.as_deref(),
        Some("/v1beta1/projects/my-proj/locations/europe-west4/endpoints/openapi/chat/completions"),
    );
}

#[tokio::test]
async fn streaming_passes_the_upstream_sse_through() {
    let (base_url, _state) = start_mock(MockState::new(3600)).await;
    let backend = backend_against(&base_url);

    let mut stream = backend
        .create_chat_completion_stream(chat_request("gemini-2.5-pro", true))
        .await
        .expect("streaming chat completion against mock");

    let mut body = String::new();
    while let Some(chunk) = stream.next().await {
        body.push_str(&String::from_utf8_lossy(&chunk.expect("chunk")));
    }

    assert!(body.contains("chat.completion.chunk"), "body was: {body}");
    assert!(
        body.trim_end().ends_with("data: [DONE]"),
        "body was: {body}"
    );
}

#[tokio::test]
async fn embeddings_translate_to_and_from_the_native_predict_call() {
    let (base_url, state) = start_mock(MockState::new(3600)).await;
    let backend = backend_against(&base_url);

    let response = backend
        .create_embedding(CreateEmbeddingRequest {
            model: "text-embedding-005".to_string(),
            input: EmbeddingInput::Multiple(vec!["alpha".to_string(), "beta".to_string()]),
            encoding_format: None,
            dimensions: Some(2),
            user: None,
        })
        .await
        .expect("embeddings against mock");

    {
        let captured = state.captured.lock().await;
        assert_eq!(
            captured.path.as_deref(),
            Some("/v1/projects/my-proj/locations/us-central1/publishers/google/models/text-embedding-005:predict"),
            "embeddings have no OpenAI-compatible route on Vertex"
        );
        let body = captured.body.as_ref().expect("captured body");
        assert_eq!(body["instances"][0]["content"], "alpha");
        assert_eq!(body["instances"][1]["content"], "beta");
        assert_eq!(body["parameters"]["outputDimensionality"], 2);
    }

    assert_eq!(response.data.len(), 2);
    assert_eq!(response.data[1].index, 1);
    match &response.data[0].embedding {
        EmbeddingData::Float(values) => assert_eq!(values, &vec![0.5, -0.25]),
        other => panic!("expected float embeddings, got {other:?}"),
    }
    assert_eq!(
        response.usage.as_ref().unwrap().total_tokens,
        7,
        "Vertex counts tokens per prediction; the OpenAI shape carries one total"
    );
}

#[tokio::test]
async fn gemini_embedding_2_fuses_a_multimodal_document_through_embed_content() {
    let (base_url, state) = start_mock(MockState::new(3600)).await;
    let backend = backend_against(&base_url);
    let image = ImageUrl {
        url: "data:image/jpeg;base64,aW1hZ2U=".to_string(),
        detail: None,
    };

    let response = backend
        .create_embedding(CreateEmbeddingRequest {
            model: "gemini-embedding-2".to_string(),
            input: EmbeddingInput::Multimodal(vec![
                EmbeddingInputItem::Text {
                    text: "Profile photos.".to_string(),
                },
                EmbeddingInputItem::ImageUrl {
                    image_url: image.clone(),
                },
                EmbeddingInputItem::ImageUrl { image_url: image },
                EmbeddingInputItem::Text {
                    text: "likes hiking".to_string(),
                },
            ]),
            encoding_format: None,
            dimensions: Some(3072),
            user: None,
        })
        .await
        .expect("embeddings against mock");

    assert_eq!(
        state.captured.lock().await.path.as_deref(),
        Some("/v1/projects/my-proj/locations/us-central1/publishers/google/models/gemini-embedding-2:embedContent"),
        "gemini-embedding-2 rejects :predict"
    );
    let bodies = state.embed_content_bodies.lock().await;
    assert_eq!(bodies.len(), 1, "one document is one call");
    assert_eq!(
        bodies[0],
        json!({
            "content": {"parts": [
                {"text": "Profile photos."},
                {"inlineData": {"mimeType": "image/jpeg", "data": "aW1hZ2U="}},
                {"inlineData": {"mimeType": "image/jpeg", "data": "aW1hZ2U="}},
                {"text": "likes hiking"}
            ]},
            "outputDimensionality": 3072
        })
    );
    assert_eq!(response.data.len(), 1, "parts fuse into one vector");
    match &response.data[0].embedding {
        EmbeddingData::Float(values) => assert_eq!(values[0], 4.0),
        other => panic!("expected float embeddings, got {other:?}"),
    }
    assert_eq!(response.usage.as_ref().unwrap().total_tokens, 5);
}

#[tokio::test]
async fn gemini_embedding_2_text_batches_fan_out_in_order() {
    let (base_url, state) = start_mock(MockState::new(3600)).await;
    let backend = backend_against(&base_url);
    let texts: Vec<String> = (1..=20).map(|n| "x".repeat(n)).collect();

    let response = backend
        .create_embedding(CreateEmbeddingRequest {
            model: "google/gemini-embedding-2".to_string(),
            input: EmbeddingInput::Multiple(texts),
            encoding_format: None,
            dimensions: None,
            user: None,
        })
        .await
        .expect("embeddings against mock");

    assert_eq!(state.embed_content_bodies.lock().await.len(), 20);
    assert_eq!(response.data.len(), 20);
    for (index, embedding) in response.data.iter().enumerate() {
        assert_eq!(embedding.index, index as i32);
        match &embedding.embedding {
            EmbeddingData::Float(values) => assert_eq!(values[1], (index + 1) as f32),
            other => panic!("expected float embeddings, got {other:?}"),
        }
    }
    assert_eq!(response.usage.as_ref().unwrap().total_tokens, 100);
}

#[tokio::test]
async fn gemini_embedding_2_batches_beyond_the_cap_are_refused_before_dispatch() {
    let (base_url, state) = start_mock(MockState::new(3600)).await;
    let backend = backend_against(&base_url);
    let err = backend
        .create_embedding(CreateEmbeddingRequest {
            model: "gemini-embedding-2".to_string(),
            input: EmbeddingInput::Multiple(vec!["x".to_string(); 101]),
            encoding_format: None,
            dimensions: None,
            user: None,
        })
        .await
        .expect_err("an oversized batch must be refused");
    assert!(matches!(
        err,
        openai_server::backend::BackendError::ApiError { status: 400, .. }
    ));
    assert!(state.embed_content_bodies.lock().await.is_empty());
}

#[tokio::test]
async fn gemini_embedding_2_upstream_errors_keep_their_status_and_message() {
    let mut state = MockState::new(3600);
    state.upstream_error = Some((
        429,
        json!({"error": {"code": 429, "message": "Quota exceeded", "status": "RESOURCE_EXHAUSTED"}}),
    ));
    let (base_url, _state) = start_mock(state).await;
    let backend = backend_against(&base_url);

    let err = backend
        .create_embedding(CreateEmbeddingRequest {
            model: "gemini-embedding-2".to_string(),
            input: EmbeddingInput::Single("a red circle".to_string()),
            encoding_format: None,
            dimensions: None,
            user: None,
        })
        .await
        .expect_err("a 429 must not become an embedding");
    match err {
        openai_server::backend::BackendError::ApiError {
            status, message, ..
        } => {
            assert_eq!(status, 429);
            assert_eq!(message, "Quota exceeded");
        }
        other => panic!("expected ApiError, got {other:?}"),
    }
}

#[tokio::test]
async fn listing_models_reads_the_publisher_catalog() {
    let (base_url, state) = start_mock(MockState::new(3600)).await;
    let backend = backend_against(&base_url);

    let models = backend
        .list_models()
        .await
        .expect("list models against mock");

    assert_eq!(
        state.captured.lock().await.path.as_deref(),
        Some("/v1beta1/publishers/google/models"),
    );
    let ids: Vec<&str> = models.data.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["gemini-2.5-pro", "text-embedding-005"],
        "the listing must follow nextPageToken rather than stopping at page one"
    );
    assert!(models.data.iter().all(|m| m.owned_by == "google"));
}

#[tokio::test]
async fn an_upstream_error_surfaces_its_google_api_message() {
    // A Google API error nests `message` under `error` like OpenAI's does, but
    // its `code` is a number, so it cannot be read through the OpenAI shape.
    let mut state = MockState::new(3600);
    state.upstream_error = Some((
        403,
        json!({"error": {
            "code": 403,
            "message": "Vertex AI API has not been used in project my-proj before",
            "status": "PERMISSION_DENIED"
        }}),
    ));
    let (base_url, _state) = start_mock(state).await;
    let backend = backend_against(&base_url);

    let err = match backend.list_models().await {
        Err(err) => err,
        Ok(_) => panic!("a 403 must not be reported as a model list"),
    };

    let message = err.to_string();
    assert!(message.contains("403"), "status must survive: {message}");
    assert!(
        message.contains("has not been used in project"),
        "the Google message must survive: {message}"
    );
}

#[tokio::test]
async fn openai_embeddings_route_image_documents_to_a_vertex_connection() {
    use openai_server::{
        backend::RouterBackend,
        handlers,
        middleware::auth_middleware,
        model_routes::InMemoryModelRouteStore,
        provider_connections::{
            ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore,
            ProviderConnectionStore,
        },
    };
    use tower::ServiceExt;

    use crate::support::app::{AppStateBuilder, TEST_API_KEY};

    let (base_url, mock) = start_mock(MockState::new(3600)).await;
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    connections
        .create(ConnectionInput {
            name: "vertex-us-multi-region".into(),
            provider_kind: "vertex".into(),
            base_url: Some(base_url.clone()),
            prefix: Some("vertex-us/".into()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: service_account_key(&base_url, json!({})),
        })
        .await
        .unwrap();
    let state = AppStateBuilder::new(Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections,
        Arc::new(InMemoryModelRouteStore::new()),
    )))
    .build()
    .await;
    let app = Router::new()
        .route(
            "/v1/embeddings",
            post(handlers::embeddings::create_embedding),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);
    let response = app
        .oneshot(
            axum::http::Request::post("/v1/embeddings")
                .header("authorization", format!("Bearer {TEST_API_KEY}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "model": "vertex-us/gemini-embedding-2",
                        "dimensions": 3072,
                        "input": [
                            {"type": "text", "text": "Profile photos."},
                            {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,aW1hZ2U="}}
                        ]
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
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["data"][0]["embedding"][0], 2.0,
        "two parts, one vector"
    );
    assert_eq!(body["usage"]["prompt_tokens"], 5);
    assert_eq!(mock.embed_content_bodies.lock().await.len(), 1);
}
