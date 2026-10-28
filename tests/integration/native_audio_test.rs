//! Literal-wire audio regressions through real routing, auth, and handlers.
//! The upstream recorder observes multipart parts independently of the router.
//! The SSE fixture checks buffered complete-body parity; incremental audio
//! event delivery remains outside this contract.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use openai_server::{
    api_keys::{ApiKeyConnectionScope, ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore},
    backend::RouterBackend,
    handlers,
    hashing::KeyHasher,
    middleware::auth_middleware,
    model_routes::InMemoryModelRouteStore,
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
};
use reqwest::multipart::{Form, Part};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::support::{app::AppStateBuilder, mock::serve};

const FILE: &[u8] = &[0, 255, 1, 128, b'\r', b'\n', 3];
const PUBLIC_AUDIO: &str = "audio/whisper-1";
const PUBLIC_SPEECH: &str = "audio/tts-1";
const PROVIDER_ERROR: &str = r#"{ "error":{"message":"invalid native audio option","param":"deliberately_invalid","code":"unknown_option","future":[null,1]}, "request_id":"provider-error" }"#;

#[derive(Debug, PartialEq)]
struct RecordedPart {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    extension: Option<String>,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Recorder {
    calls: AtomicUsize,
    multipart: Mutex<Vec<Vec<RecordedPart>>>,
    speech: Mutex<Vec<Value>>,
}

fn native_reply(content_type: &str, body: impl Into<Bytes>) -> Response {
    let mut response = (StatusCode::OK, body.into()).into_response();
    for (name, value) in [
        ("content-type", content_type),
        ("x-request-id", "audio-native-request"),
        ("openai-processing-ms", "12.5"),
        ("x-ratelimit-limit-requests", "100"),
        ("retry-after", "2"),
        ("set-cookie", "upstream-secret=hidden"),
        ("authorization", "Bearer upstream-secret"),
        ("x-provider-secret", "hidden"),
    ] {
        response.headers_mut().insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    response
}

fn format_fixture(format: &str) -> (&'static str, &'static str) {
    match format {
        "verbose_json" => ("application/json", r#"{ "text":"hello", "language":"en", "duration":1.0000000000000001, "segments":[{"id":0,"text":"hello","confidence":0.9876543210987654321}], "future":null }"#),
        "text" => ("text/plain; charset=utf-8", "hello\n"),
        "srt" => ("application/x-subrip", "1\n00:00:00,000 --> 00:00:01,000\nhello\n"),
        "vtt" => ("text/vtt", "WEBVTT\n\n00:00.000 --> 00:01.000\nhello\n"),
        "sse" => ("text/event-stream", "event: transcript.text.delta\ndata: {\"delta\":\"hello\",\"future\":null}\n\nevent: transcript.text.done\ndata: {\"text\":\"hello\"}\n\n"),
        other => panic!("unknown fixture: {other}"),
    }
}

async fn multipart_upstream(
    State(recorder): State<Arc<Recorder>>,
    headers: HeaderMap,
    mut body: Multipart,
) -> Response {
    recorder.calls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(headers["authorization"], "Bearer upstream-audio-key");
    let mut parts = Vec::new();
    while let Some(field) = body.next_field().await.unwrap() {
        let name = field.name().unwrap().to_string();
        let filename = field.file_name().map(str::to_string);
        let content_type = field.content_type().map(str::to_string);
        let extension = field
            .headers()
            .get("x-audio-part")
            .map(|value| value.to_str().unwrap().to_string());
        let bytes = field.bytes().await.unwrap().to_vec();
        parts.push(RecordedPart {
            name,
            filename,
            content_type,
            extension,
            bytes,
        });
    }
    if parts.iter().any(|part| part.name == "deliberately_invalid") {
        recorder.multipart.lock().await.push(parts);
        let mut response = native_reply("application/json", PROVIDER_ERROR);
        *response.status_mut() = StatusCode::BAD_REQUEST;
        return response;
    }
    let format = parts
        .iter()
        .find(|part| part.name == "response_format")
        .map(|part| std::str::from_utf8(&part.bytes).unwrap())
        .unwrap_or("verbose_json");
    let (content_type, output) = format_fixture(format);
    let response = native_reply(content_type, output);
    recorder.multipart.lock().await.push(parts);
    response
}

async fn speech_upstream(
    State(recorder): State<Arc<Recorder>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    recorder.calls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(headers["authorization"], "Bearer upstream-audio-key");
    let invalid = body.get("deliberately_invalid").is_some();
    recorder.speech.lock().await.push(body);
    if invalid {
        let mut response = native_reply("application/json", PROVIDER_ERROR);
        *response.status_mut() = StatusCode::BAD_REQUEST;
        return response;
    }
    native_reply("audio/ogg", Bytes::from_static(FILE))
}

async fn upstream(recorder: Arc<Recorder>) -> String {
    serve(
        Router::new()
            .route("/audio/transcriptions", post(multipart_upstream))
            .route("/audio/translations", post(multipart_upstream))
            .route("/audio/speech", post(speech_upstream))
            .with_state(recorder),
    )
    .await
}

async fn app(base: &str, permission: ApiKeyPermission) -> (String, String) {
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    connections
        .create(ConnectionInput {
            // A misleading connection name must never satisfy provider scope.
            name: "openai".into(),
            provider_kind: "openai-compatible".into(),
            base_url: Some(base.into()),
            prefix: Some("audio/".into()),
            exposure: ExposurePolicy::Unlisted,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "upstream-audio-key".into(),
        })
        .await
        .unwrap();
    let routes = Arc::new(InMemoryModelRouteStore::new());
    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections.clone(),
        routes.clone(),
    ));
    let keys = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    let key = keys
        .create("audio-client".into(), permission, HashMap::new(), None)
        .await
        .unwrap()
        .raw_key;
    let state = AppStateBuilder::new(backend)
        .api_key_store(keys)
        .seed_test_api_key(false)
        .provider_connection_store(connections)
        .model_route_store(routes)
        .build()
        .await;
    let app = Router::new()
        .route(
            "/v1/audio/transcriptions",
            post(handlers::audio::create_transcription),
        )
        .route(
            "/v1/audio/translations",
            post(handlers::audio::create_translation),
        )
        .route("/v1/audio/speech", post(handlers::audio::create_speech))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state);
    (serve(app).await, key)
}

fn multipart(model: &str, format: &str) -> Form {
    let mut file_headers = reqwest::header::HeaderMap::new();
    file_headers.insert("x-audio-part", "file-extension".parse().unwrap());
    Form::new()
        .text("model", model.to_owned())
        .text("timestamp_granularities[]", "word")
        .part(
            "file",
            Part::bytes(FILE.to_vec())
                .file_name("clip.unusual")
                .mime_str("audio/x-router-fixture")
                .unwrap()
                .headers(file_headers),
        )
        .text("timestamp_granularities[]", "segment")
        .text("future_audio_option", "retain me")
        .text("response_format", format.to_owned())
}

fn assert_headers(response: &reqwest::Response, content_type: &str) {
    assert_eq!(response.headers()["content-type"], content_type);
    assert_eq!(response.headers()["x-request-id"], "audio-native-request");
    assert_eq!(response.headers()["openai-processing-ms"], "12.5");
    assert_eq!(response.headers()["x-ratelimit-limit-requests"], "100");
    assert_eq!(response.headers()["retry-after"], "2");
    for excluded in ["set-cookie", "authorization", "x-provider-secret"] {
        assert!(!response.headers().contains_key(excluded), "{excluded}");
    }
}

#[tokio::test]
async fn ordered_multipart_native_formats_and_buffered_sse_body_survive_audio_handlers() {
    let recorder = Arc::new(Recorder::default());
    let base = upstream(recorder.clone()).await;
    let (app, key) = app(&base, ApiKeyPermission::default()).await;
    let client = reqwest::Client::new();
    for endpoint in ["transcriptions", "translations"] {
        for format in ["verbose_json", "text", "srt", "vtt", "sse"] {
            let response = client
                .post(format!("{app}/v1/audio/{endpoint}"))
                .bearer_auth(&key)
                .multipart(multipart(PUBLIC_AUDIO, format))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let (mime, expected) = format_fixture(format);
            assert_eq!(status, 200, "{}", response.text().await.unwrap_or_default());
            assert_headers(&response, mime);
            assert_eq!(response.bytes().await.unwrap(), expected.as_bytes());
            let records = recorder.multipart.lock().await;
            let parts = records.last().unwrap();
            assert_eq!(
                parts
                    .iter()
                    .map(|part| part.name.as_str())
                    .collect::<Vec<_>>(),
                [
                    "model",
                    "timestamp_granularities[]",
                    "file",
                    "timestamp_granularities[]",
                    "future_audio_option",
                    "response_format"
                ]
            );
            assert_eq!(parts[0].bytes, b"whisper-1");
            assert_eq!(parts[1].bytes, b"word");
            assert_eq!(parts[3].bytes, b"segment");
            assert_eq!(parts[4].bytes, b"retain me");
            assert_eq!(parts[5].bytes, format.as_bytes());
            assert_eq!(
                parts[2],
                RecordedPart {
                    name: "file".into(),
                    filename: Some("clip.unusual".into()),
                    content_type: Some("audio/x-router-fixture".into()),
                    extension: Some("file-extension".into()),
                    bytes: FILE.to_vec(),
                }
            );
        }
    }
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 10);
}

#[tokio::test]
async fn speech_preserves_unknown_json_and_native_binary_metadata() {
    let recorder = Arc::new(Recorder::default());
    let base = upstream(recorder.clone()).await;
    let (app, key) = app(&base, ApiKeyPermission::default()).await;
    let response = reqwest::Client::new()
        .post(format!("{app}/v1/audio/speech"))
        .bearer_auth(key)
        .json(
            &json!({"model":PUBLIC_SPEECH,"input":"hello","voice":"alloy",
            "response_format":"mp3","instructions":"Speak quietly",
            "future_audio_options":{"nested":[1,null,"keep"]}}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_headers(&response, "audio/ogg");
    assert_eq!(response.bytes().await.unwrap(), FILE);
    assert_eq!(
        *recorder.speech.lock().await,
        [json!({"model":"tts-1","input":"hello","voice":"alloy",
        "response_format":"mp3","instructions":"Speak quietly",
        "future_audio_options":{"nested":[1,null,"keep"]}})]
    );
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn audio_permissions_gate_provider_public_model_and_connection_before_upstream() {
    let recorder = Arc::new(Recorder::default());
    let base = upstream(recorder.clone()).await;
    let client = reqwest::Client::new();
    for permission in [
        ApiKeyPermission {
            provider: Some("openai".into()),
            ..Default::default()
        },
        ApiKeyPermission {
            models: Some(vec!["whisper-1".into(), "tts-1".into()]),
            ..Default::default()
        },
        ApiKeyPermission {
            connection_id: Some("another-connection".into()),
            ..Default::default()
        },
        ApiKeyPermission {
            connection_scopes: Some(vec![ApiKeyConnectionScope {
                connection_id: "another-connection".into(),
                models: Vec::new(),
            }]),
            ..Default::default()
        },
    ] {
        let (app, key) = app(&base, permission).await;
        for endpoint in ["transcriptions", "translations"] {
            let response = client
                .post(format!("{app}/v1/audio/{endpoint}"))
                .bearer_auth(&key)
                .multipart(multipart(PUBLIC_AUDIO, "text"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 403, "{}", response.text().await.unwrap());
        }
        let response = client
            .post(format!("{app}/v1/audio/speech"))
            .bearer_auth(&key)
            .json(&json!({"model":PUBLIC_SPEECH,"input":"hello","voice":"alloy"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{}", response.text().await.unwrap());
    }
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn permitted_public_audio_models_dispatch_with_stripped_upstream_ids() {
    let recorder = Arc::new(Recorder::default());
    let base = upstream(recorder.clone()).await;
    let permission = ApiKeyPermission {
        provider: Some("openai-compatible".into()),
        models: Some(vec![PUBLIC_AUDIO.into(), PUBLIC_SPEECH.into()]),
        ..Default::default()
    };
    let (app, key) = app(&base, permission).await;
    let client = reqwest::Client::new();
    for endpoint in ["transcriptions", "translations"] {
        let response = client
            .post(format!("{app}/v1/audio/{endpoint}"))
            .bearer_auth(&key)
            .multipart(multipart(PUBLIC_AUDIO, "text"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    }
    let response = client
        .post(format!("{app}/v1/audio/speech"))
        .bearer_auth(&key)
        .json(&json!({"model":PUBLIC_SPEECH,"input":"hello","voice":"alloy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 3);
    assert_eq!(recorder.speech.lock().await[0]["model"], "tts-1");
    for parts in recorder.multipart.lock().await.iter() {
        assert_eq!(parts[0].bytes, b"whisper-1");
    }
}

#[tokio::test]
async fn malformed_audio_multipart_returns_400_without_upstream_calls() {
    let recorder = Arc::new(Recorder::default());
    let base = upstream(recorder.clone()).await;
    let (app, key) = app(&base, ApiKeyPermission::default()).await;
    let client = reqwest::Client::new();
    for endpoint in ["transcriptions", "translations"] {
        let path = format!("{app}/v1/audio/{endpoint}");
        for form in [
            Form::new().part("file", Part::bytes(FILE.to_vec())),
            multipart("", "text"),
        ] {
            let response = client
                .post(&path)
                .bearer_auth(&key)
                .multipart(form)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 400, "{}", response.text().await.unwrap());
        }
        for body in [
            "--broken\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\naudio/whisper-1\r\n",
            "--broken\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\naudio/whisper-1\r\n--broken--\r\n",
        ] {
            let response = client.post(&path).bearer_auth(&key)
                .header("content-type", "multipart/form-data; boundary=broken").body(body).send().await.unwrap();
            assert_eq!(response.status(), 400, "{}", response.text().await.unwrap());
        }
        let response = client
            .post(&path)
            .bearer_auth(&key)
            .multipart(multipart(PUBLIC_AUDIO, "text").text("model", "audio/other"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{}", response.text().await.unwrap());
        let response = client
            .post(&path)
            .bearer_auth(&key)
            .multipart(
                Form::new()
                    .part("model", Part::bytes(vec![255]))
                    .part("file", Part::bytes(FILE.to_vec())),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{}", response.text().await.unwrap());
    }
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn invalid_native_audio_options_reach_provider_and_retain_its_error() {
    let recorder = Arc::new(Recorder::default());
    let base = upstream(recorder.clone()).await;
    let (app, key) = app(&base, ApiKeyPermission::default()).await;
    let client = reqwest::Client::new();
    for endpoint in ["transcriptions", "translations"] {
        let response = client
            .post(format!("{app}/v1/audio/{endpoint}"))
            .bearer_auth(&key)
            .multipart(multipart(PUBLIC_AUDIO, "text").text("deliberately_invalid", "bad"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert_headers(&response, "application/json");
        assert_eq!(response.text().await.unwrap(), PROVIDER_ERROR);
    }
    let response = client.post(format!("{app}/v1/audio/speech"))
        .bearer_auth(&key).json(&json!({"model":PUBLIC_SPEECH,"input":"hello","voice":"alloy","deliberately_invalid":true}))
        .send().await.unwrap();
    assert_eq!(response.status(), 400);
    assert_headers(&response, "application/json");
    assert_eq!(response.text().await.unwrap(), PROVIDER_ERROR);
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 3);
}
