use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Body,
    http::{self, Request, StatusCode},
    Router,
};
use futures::StreamExt;
use http_body_util::BodyExt;
use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::TracerProvider;
use tower::ServiceExt;

use openai_server::{
    backend::{BackendError, BackendProvider, ResolvedRoute, RouteResolution, SseByteStream},
    fallback::FallbackPolicy,
    hashing::KeyHasher,
    sampling_policy::{
        InMemorySamplingPolicyStore, OutcomeComparator, OutcomeRule, SamplingPolicy,
        SamplingPolicyStore,
    },
    telemetry::SAMPLING_OUTCOME_RATE_ATTRIBUTE,
    types::{
        ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
        CreateEmbeddingRequest, CreateEmbeddingResponse, CreateImageRequest,
        CreateModerationRequest, CreateModerationResponse, CreateSpeechRequest, Embedding,
        EmbeddingData, FunctionCall, Image, ImageResponse, ListResponse, Model, ToolCall,
        TranscriptionResponse, Usage,
    },
};

use crate::support::app::{
    build_app, build_app_with_fallback_policy, build_app_with_sampling_policy, build_full_app,
    chat_request_body, TEST_API_KEY,
};
use crate::support::otel::{setup_otel, span_attrs};

// ---------------------------------------------------------------------------
// Mock backend — non-streaming only
// ---------------------------------------------------------------------------

/// Build a `ResolvedRoute` that dispatches to `backend`, reporting `name` as the
/// provider/system and the request `model` unchanged as the upstream id —
/// mirroring the leaf-adapter default `resolve_route`. Lets each stateless mock
/// implement `resolve`/`resolve_strict` (which the handlers now call to resolve
/// once and dispatch to the resolved backend).
fn mock_route(backend: Arc<dyn BackendProvider>, name: &str, model: &str) -> ResolvedRoute {
    ResolvedRoute {
        routing: Default::default(),
        backend,
        stripped_model: model.to_string(),
        capabilities: Arc::new(
            openai_server::capabilities::resolve("openai-compatible", model, None, None).unwrap(),
        ),
        resolution: RouteResolution {
            provider_kind: name.to_string(),
            connection_id: None,
            connection_name: name.to_string(),
            upstream_model_id: model.to_string(),
            connection_prefix: None,
            model_route_id: None,
        },
    }
}

pub(crate) struct MockBackend;

#[async_trait::async_trait]
impl BackendProvider for MockBackend {
    async fn create_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        let oversized_content = request.model == "oversized-output-model";
        let oversized_arguments = request.model == "oversized-tool-arguments-model";
        let wide_output = request.model == "wide-output-model";
        let content = if oversized_content {
            Some(serde_json::json!("y".repeat(300 * 1024)))
        } else if oversized_arguments {
            None
        } else {
            Some(serde_json::json!("Hello from mock!"))
        };
        let tool_calls = if wide_output {
            Some(
                (0..50)
                    .map(|index| ToolCall {
                        id: format!("output-call-{index}"),
                        tool_type: "function".to_string(),
                        function: FunctionCall {
                            name: format!("tool_{index}"),
                            arguments: format!("{{\"index\":{index}}}"),
                        },
                    })
                    .collect(),
            )
        } else if oversized_content || oversized_arguments {
            Some(vec![ToolCall {
                id: "output-call-1".to_string(),
                tool_type: "function".to_string(),
                function: FunctionCall {
                    name: "notify".to_string(),
                    arguments: if oversized_arguments {
                        "z".repeat(300 * 1024)
                    } else {
                        "{\"ok\":true}".to_string()
                    },
                },
            }])
        } else {
            None
        };
        Ok(ChatCompletionResponse {
            id: "chatcmpl-mock-123".to_string(),
            object: "chat.completion".to_string(),
            created: 1234567890,
            model: "mock-model".to_string(),
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content,
                    name: None,
                    tool_calls,
                    tool_call_id: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: Some(5),
                total_tokens: 15,
                ..Default::default()
            }),
        })
    }

    async fn create_embedding(
        &self,
        _r: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_image(&self, _r: CreateImageRequest) -> Result<ImageResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_speech(&self, _r: CreateSpeechRequest) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_transcription(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _l: Option<String>,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_translation(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
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

    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        if model == "anthropic/claude-sonnet-4-5" {
            let upstream_model = "claude-sonnet-4-5";
            return Ok(ResolvedRoute {
                routing: Default::default(),
                backend: Arc::new(MockBackend),
                stripped_model: upstream_model.to_string(),
                capabilities: Arc::new(
                    openai_server::capabilities::resolve("anthropic", upstream_model, None, None)
                        .unwrap(),
                ),
                resolution: RouteResolution {
                    provider_kind: "anthropic".to_string(),
                    connection_id: Some("anthropic-test-connection".to_string()),
                    connection_name: "Anthropic test".to_string(),
                    upstream_model_id: upstream_model.to_string(),
                    connection_prefix: None,
                    model_route_id: None,
                },
            });
        }
        Ok(mock_route(Arc::new(MockBackend), self.name(), model))
    }
    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        self.resolve(model).await
    }
}

// ---------------------------------------------------------------------------
// Mock router — failed primary followed by a successful fallback
// ---------------------------------------------------------------------------

struct FailedAttemptBackend;

#[async_trait::async_trait]
impl BackendProvider for FailedAttemptBackend {
    async fn create_chat_completion(
        &self,
        _request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        Err(BackendError::api_status(503, "primary unavailable"))
    }

    async fn create_embedding(
        &self,
        _r: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_image(&self, _r: CreateImageRequest) -> Result<ImageResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_speech(&self, _r: CreateSpeechRequest) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_transcription(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _l: Option<String>,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_translation(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
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
        "failed-attempt"
    }
}

struct FallbackRouterBackend;

#[async_trait::async_trait]
impl BackendProvider for FallbackRouterBackend {
    async fn create_chat_completion(
        &self,
        _request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        Err(BackendError::Other("resolve before dispatch".into()))
    }
    async fn create_embedding(
        &self,
        _r: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_image(&self, _r: CreateImageRequest) -> Result<ImageResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_speech(&self, _r: CreateSpeechRequest) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_transcription(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _l: Option<String>,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_translation(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
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
        "fallback-router"
    }

    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(mock_route(
            Arc::new(FailedAttemptBackend),
            "primary-provider",
            model,
        ))
    }

    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        match model {
            "fallback-model" => Ok(mock_route(
                Arc::new(MockBackend),
                "fallback-provider",
                model,
            )),
            _ => Err(BackendError::ConfigError(format!(
                "unknown fallback model: {model}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Mock backend — returns real SSE stream
// ---------------------------------------------------------------------------

struct MockStreamingBackend;

#[async_trait::async_trait]
impl BackendProvider for MockStreamingBackend {
    async fn create_chat_completion(
        &self,
        _request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        // Fallback used by default stream impl; not relevant for this test
        Err(BackendError::Other("use streaming".into()))
    }

    async fn create_chat_completion_stream(
        &self,
        _request: ChatCompletionRequest,
    ) -> Result<SseByteStream, BackendError> {
        let chunks: Vec<Result<bytes::Bytes, BackendError>> = vec![
            Ok(bytes::Bytes::from(
                "data: {\"id\":\"chatcmpl-mock-stream\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
            )),
            Ok(bytes::Bytes::from(
                "data: {\"id\":\"chatcmpl-mock-stream\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello!\"},\"finish_reason\":null}]}\n\n",
            )),
            Ok(bytes::Bytes::from(
                "data: {\"id\":\"chatcmpl-mock-stream\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"total_tokens\":13}}\n\n",
            )),
            Ok(bytes::Bytes::from("data: [DONE]\n\n")),
        ];
        Ok(Box::pin(futures::stream::iter(chunks)))
    }

    async fn create_embedding(
        &self,
        _r: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_image(&self, _r: CreateImageRequest) -> Result<ImageResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_speech(&self, _r: CreateSpeechRequest) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_transcription(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _l: Option<String>,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_translation(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
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
        "mock-streaming"
    }

    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(mock_route(
            Arc::new(MockStreamingBackend),
            self.name(),
            model,
        ))
    }
    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        self.resolve(model).await
    }
}

// ---------------------------------------------------------------------------
// Mock backend — streams arbitrary byte chunks, optionally with a delay before
// each one (to simulate a model generating over wall-clock time)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MockChunkStreamBackend {
    chunks: Vec<bytes::Bytes>,
    delay_ms: u64,
}

#[async_trait::async_trait]
impl BackendProvider for MockChunkStreamBackend {
    async fn create_chat_completion(
        &self,
        _request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        Err(BackendError::Other("use streaming".into()))
    }

    async fn create_chat_completion_stream(
        &self,
        _request: ChatCompletionRequest,
    ) -> Result<SseByteStream, BackendError> {
        let delay = std::time::Duration::from_millis(self.delay_ms);
        let stream = futures::stream::iter(
            self.chunks.clone().into_iter().map(Ok::<_, BackendError>),
        )
        .then(move |chunk| async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            chunk
        });
        Ok(Box::pin(stream))
    }

    async fn create_embedding(
        &self,
        _r: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_image(&self, _r: CreateImageRequest) -> Result<ImageResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_speech(&self, _r: CreateSpeechRequest) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_transcription(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _l: Option<String>,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_translation(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
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
        "mock-streaming"
    }

    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(mock_route(Arc::new(self.clone()), self.name(), model))
    }
    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        self.resolve(model).await
    }
}

// ---------------------------------------------------------------------------
// Mock backend — returns successful embedding
// ---------------------------------------------------------------------------

struct MockEmbeddingBackend;

#[async_trait::async_trait]
impl BackendProvider for MockEmbeddingBackend {
    async fn create_chat_completion(
        &self,
        _r: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_embedding(
        &self,
        _r: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Ok(CreateEmbeddingResponse {
            object: "list".to_string(),
            data: vec![Embedding {
                object: "embedding".to_string(),
                index: 0,
                embedding: EmbeddingData::Float(vec![0.1, 0.2, 0.3]),
            }],
            model: "text-embedding-ada-002".to_string(),
            usage: Some(Usage {
                prompt_tokens: 5,
                completion_tokens: None,
                total_tokens: 5,
                ..Default::default()
            }),
        })
    }
    async fn create_image(&self, _r: CreateImageRequest) -> Result<ImageResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_speech(&self, _r: CreateSpeechRequest) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_transcription(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _l: Option<String>,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_translation(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
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
        "mock-embedding"
    }

    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(mock_route(
            Arc::new(MockEmbeddingBackend),
            self.name(),
            model,
        ))
    }
    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        self.resolve(model).await
    }
}

// ---------------------------------------------------------------------------
// Mock backend — returns successful transcription
// ---------------------------------------------------------------------------

struct MockTranscriptionBackend;

#[async_trait::async_trait]
impl BackendProvider for MockTranscriptionBackend {
    async fn create_chat_completion(
        &self,
        _r: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_embedding(
        &self,
        _r: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_image(&self, _r: CreateImageRequest) -> Result<ImageResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_speech(&self, _r: CreateSpeechRequest) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_transcription(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _l: Option<String>,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Ok(TranscriptionResponse {
            text: "Hello world".to_string(),
        })
    }
    async fn create_translation(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
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
        "mock-transcription"
    }

    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(mock_route(
            Arc::new(MockTranscriptionBackend),
            self.name(),
            model,
        ))
    }
    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        self.resolve(model).await
    }
}

// ---------------------------------------------------------------------------
// Mock backend — returns successful image generation
// ---------------------------------------------------------------------------

struct MockImageBackend;

#[async_trait::async_trait]
impl BackendProvider for MockImageBackend {
    async fn create_chat_completion(
        &self,
        _r: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_embedding(
        &self,
        _r: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_image(&self, _r: CreateImageRequest) -> Result<ImageResponse, BackendError> {
        Ok(ImageResponse {
            created: 1234567890,
            data: vec![Image {
                url: Some("https://example.com/image.png".to_string()),
                b64_json: None,
                revised_prompt: None,
            }],
        })
    }
    async fn create_speech(&self, _r: CreateSpeechRequest) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_transcription(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _l: Option<String>,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_translation(
        &self,
        _m: String,
        _f: bytes::Bytes,
        _n: String,
        _p: Option<String>,
        _rf: Option<String>,
        _t: Option<f32>,
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
        "mock-image"
    }

    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(mock_route(Arc::new(MockImageBackend), self.name(), model))
    }
    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        self.resolve(model).await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
//
// `build_app`, `build_app_with_fallback_policy`, `build_app_with_federated_auth`,
// `build_full_app`, `chat_request_body`, `TEST_API_KEY`, `setup_otel`, and
// `span_attrs` all live in `tests/integration/support/` (imported above) —
// this file only keeps the fixtures specific to span testing.

fn event_attrs(event: &opentelemetry::trace::Event) -> HashMap<String, String> {
    event
        .attributes
        .iter()
        .map(|kv| (kv.key.to_string(), kv.value.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_non_streaming_exports_chat_completion_span() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(Arc::new(MockBackend)).await;

    let response = {
        let _guard = tracing::subscriber::set_default(subscriber);

        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(
                serde_json::to_vec(&chat_request_body(false)).unwrap(),
            ))
            .unwrap();

        app.oneshot(req).await.unwrap()
    };

    assert_eq!(response.status(), StatusCode::OK);

    for r in provider.force_flush() {
        r.expect("flush should succeed");
    }

    let spans = exporter.get_finished_spans().unwrap();
    let completion_spans: Vec<_> = spans
        .iter()
        .filter(|s| s.name.as_ref() == "chat.completion")
        .collect();

    assert!(
        !completion_spans.is_empty(),
        "expected a 'chat.completion' span; got: {:?}",
        spans.iter().map(|s| s.name.as_ref()).collect::<Vec<_>>()
    );

    let attrs = span_attrs(completion_spans[0]);

    assert!(
        spans
            .iter()
            .all(|span| span.name.as_ref() != "chat.completion.attempt"),
        "a primary-only request must not create an attempt child span"
    );

    assert_eq!(
        attrs.get("openinference.span.kind").map(String::as_str),
        Some("LLM")
    );
    let expected_principal = KeyHasher::new(b"test-pepper".to_vec()).hash(TEST_API_KEY);
    assert_eq!(
        attrs.get("chalk.auth.principal.id").map(String::as_str),
        Some(expected_principal.as_str())
    );
    assert_eq!(
        attrs.get("chalk.auth.principal.type").map(String::as_str),
        Some("router_api_key")
    );
    assert!(!attrs.contains_key("user.id"));
    assert_eq!(
        attrs.get("llm.model_name").map(String::as_str),
        Some("mock-model")
    );
    assert_eq!(attrs.get("llm.system").map(String::as_str), Some("mock"));
    assert!(
        attrs.contains_key("llm.token_count.prompt"),
        "should have prompt token count"
    );
    assert!(
        attrs.contains_key("llm.token_count.total"),
        "should have total token count"
    );
    assert!(
        attrs.contains_key("output.value"),
        "non-streaming should have output.value"
    );
    assert_eq!(
        attrs.get("llm.finish_reason").map(String::as_str),
        Some("stop")
    );
    assert_eq!(
        attrs.get("llm.stream").map(String::as_str),
        Some("false"),
        "non-streaming span should record llm.stream=false"
    );
    assert_eq!(
        attrs.get("chalk.router.attempt_count").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        attrs.get("chalk.router.fallback_used").map(String::as_str),
        Some("false")
    );
    assert_eq!(
        completion_spans[0]
            .attributes
            .iter()
            .filter(|attribute| attribute.key.as_str() == "chalk.router.attempt_count")
            .count(),
        1,
        "attempt summary must be stamped only once"
    );
}

#[tokio::test]
async fn test_completed_routed_request_exports_estimated_cost() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(Arc::new(MockBackend)).await;

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let body = serde_json::json!({
            "model": "anthropic/claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": false,
        });
        let request = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    for result in provider.force_flush() {
        result.expect("flush should succeed");
    }
    let spans = exporter.get_finished_spans().unwrap();
    let attrs = span_attrs(
        spans
            .iter()
            .find(|span| span.name.as_ref() == "chat.completion")
            .expect("completed request should export its LLM span"),
    );

    for key in ["llm.cost.total", "llm.cost.prompt", "llm.cost.completion"] {
        let cost = attrs
            .get(key)
            .unwrap_or_else(|| panic!("routed span is missing {key}: {attrs:?}"))
            .parse::<f64>()
            .expect("cost attribute should be numeric");
        assert!(cost > 0.0, "{key} should be a positive list-price estimate");
    }
    assert_eq!(
        attrs.get("chalk.cost.source").map(String::as_str),
        Some("estimated")
    );
    assert_eq!(
        attrs
            .get("chalk.cost.rate_card_version")
            .map(String::as_str),
        Some(openai_server::pricing::current().effective_from.as_str())
    );
    assert_eq!(
        attrs.get("llm.system").map(String::as_str),
        Some("anthropic")
    );
    assert_eq!(
        attrs.get("llm.model_name").map(String::as_str),
        Some("claude-sonnet-4-5")
    );
}

#[tokio::test]
async fn test_fallback_records_failed_attempt_event_without_child_spans() {
    let (provider, exporter, subscriber) = setup_otel();
    let fallback_policy = FallbackPolicy::from_map(HashMap::from([(
        "primary-model".to_string(),
        vec!["fallback-model".to_string()],
    )]));
    let app =
        build_app_with_fallback_policy(Arc::new(FallbackRouterBackend), fallback_policy).await;

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let body = serde_json::json!({
            "model": "primary-model",
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": false,
        });
        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    for result in provider.force_flush() {
        result.expect("flush should succeed");
    }

    let spans = exporter.get_finished_spans().unwrap();
    let request_span = spans
        .iter()
        .find(|span| span.name.as_ref() == "chat.completion")
        .expect("request span");
    assert!(
        spans
            .iter()
            .all(|span| span.name.as_ref() != "chat.completion.attempt"),
        "fallback dispatches must not create attempt child spans"
    );

    let failed_attempt_events: Vec<_> = request_span
        .events
        .iter()
        .filter(|event| event.name.as_ref() == "chalk.router.attempt.failed")
        .collect();
    assert_eq!(
        failed_attempt_events.len(),
        1,
        "failed primary must emit one event"
    );
    let failed_attempt_attrs = event_attrs(failed_attempt_events[0]);
    assert_eq!(
        failed_attempt_attrs
            .get("chalk.router.attempt.index")
            .map(String::as_str),
        Some("0")
    );
    assert_eq!(
        failed_attempt_attrs
            .get("chalk.router.attempt.model")
            .map(String::as_str),
        Some("primary-model")
    );
    assert_eq!(
        failed_attempt_attrs.get("llm.system").map(String::as_str),
        Some("primary-provider")
    );
    assert_eq!(
        failed_attempt_attrs
            .get("llm.model_name")
            .map(String::as_str),
        Some("primary-model")
    );
    assert_eq!(
        failed_attempt_attrs.get("error.type").map(String::as_str),
        Some("api_error")
    );
    assert!(failed_attempt_attrs
        .get("error.message")
        .is_some_and(|message| message.contains("primary unavailable")));
    assert!(failed_attempt_attrs.contains_key("chalk.router.attempt.duration_ms"));
    assert!(
        !matches!(
            request_span.status,
            opentelemetry::trace::Status::Error { .. }
        ),
        "a recovered fallback must not mark the logical request as failed"
    );

    let request_attrs = span_attrs(request_span);
    assert_eq!(
        request_attrs.get("llm.model_name").map(String::as_str),
        Some("fallback-model")
    );
    assert_eq!(
        request_attrs.get("llm.finish_reason").map(String::as_str),
        Some("stop")
    );
    assert_eq!(
        request_attrs
            .get("chalk.router.attempt_count")
            .map(String::as_str),
        Some("2")
    );
    assert_eq!(
        request_attrs
            .get("chalk.router.fallback_used")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        request_span
            .attributes
            .iter()
            .filter(|attribute| attribute.key.as_str() == "chalk.router.attempt_count")
            .count(),
        1,
        "retries must not duplicate the attempt summary"
    );
    assert_eq!(
        request_span
            .attributes
            .iter()
            .filter(|attribute| attribute.key.as_str() == "chalk.router.fallback_used")
            .count(),
        1,
        "retries must not duplicate the fallback summary"
    );
}

#[tokio::test]
async fn test_streaming_exports_chat_completion_span() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(Arc::new(MockStreamingBackend)).await;

    {
        let _guard = tracing::subscriber::set_default(subscriber);

        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(
                serde_json::to_vec(&chat_request_body(true)).unwrap(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Consume the entire body so the stream map closure runs
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body_text = String::from_utf8_lossy(&body_bytes);
        assert!(body_text.contains("[DONE]"), "body should contain [DONE]");
    }

    for r in provider.force_flush() {
        r.expect("flush should succeed");
    }

    let spans = exporter.get_finished_spans().unwrap();
    let completion_spans: Vec<_> = spans
        .iter()
        .filter(|s| s.name.as_ref() == "chat.completion")
        .collect();

    assert!(
        !completion_spans.is_empty(),
        "expected a 'chat.completion' span for streaming; got: {:?}",
        spans.iter().map(|s| s.name.as_ref()).collect::<Vec<_>>()
    );

    let attrs = span_attrs(completion_spans[0]);

    assert_eq!(
        attrs.get("openinference.span.kind").map(String::as_str),
        Some("LLM")
    );
    assert_eq!(
        attrs.get("llm.model_name").map(String::as_str),
        Some("mock-model")
    );
    assert_eq!(
        attrs.get("llm.system").map(String::as_str),
        Some("mock-streaming")
    );
    assert_eq!(
        attrs.get("llm.finish_reason").map(String::as_str),
        Some("stop")
    );
    assert!(
        attrs.contains_key("input.value"),
        "streaming should have input.value"
    );
}

#[tokio::test]
async fn test_streaming_outcome_rule_matches_the_same_response_shape_as_non_streaming() {
    assert_streaming_outcome_rule("choices.0.message.content", serde_json::json!("Hello!")).await;
}

#[tokio::test]
async fn test_streaming_metadata_outcome_rule_matches_with_content_logging_disabled() {
    assert_streaming_outcome_rule("choices.0.finish_reason", serde_json::json!("stop")).await;
}

async fn assert_streaming_outcome_rule(path: &str, value: serde_json::Value) {
    // Regression: the streamed response used to be evaluated as a bare
    // `choices` array rather than wrapped in a `choices` key, so a rule
    // written against the documented `choices.0.message.content` path (the
    // same path a non-streaming response satisfies) silently never matched.
    let policy = SamplingPolicy {
        enabled: true,
        default_rate: 0.0,
        default_log_content: false,
        outcome_rules: vec![OutcomeRule {
            path: path.to_string(),
            op: OutcomeComparator::Eq,
            value,
            rate: 1.0,
        }],
        ..SamplingPolicy::default()
    };
    let store: Arc<dyn SamplingPolicyStore> = Arc::new(InMemorySamplingPolicyStore::new(policy));
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app_with_sampling_policy(Arc::new(MockStreamingBackend), store).await;

    {
        let _guard = tracing::subscriber::set_default(subscriber);

        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(
                serde_json::to_vec(&chat_request_body(true)).unwrap(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = resp.into_body().collect().await.unwrap().to_bytes();
    }

    for r in provider.force_flush() {
        r.expect("flush should succeed");
    }

    let spans = exporter.get_finished_spans().unwrap();
    let completion_span = spans
        .iter()
        .find(|s| s.name.as_ref() == "chat.completion")
        .expect("expected a chat.completion span");
    let attrs = span_attrs(completion_span);

    let outcome_rate: f64 = attrs
        .get(SAMPLING_OUTCOME_RATE_ATTRIBUTE)
        .unwrap_or_else(|| {
            panic!("outcome rate attribute must be stamped when the rule matches: {attrs:?}")
        })
        .parse()
        .expect("outcome rate attribute must parse as f64");
    assert_eq!(
        outcome_rate, 1.0,
        "the matched outcome rule's rate must be the one stamped"
    );
}

#[tokio::test]
async fn test_streaming_span_records_token_counts_from_sse() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(Arc::new(MockStreamingBackend)).await;

    {
        let _guard = tracing::subscriber::set_default(subscriber);

        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(
                serde_json::to_vec(&chat_request_body(true)).unwrap(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let _ = resp.into_body().collect().await.unwrap().to_bytes();
    }

    for r in provider.force_flush() {
        r.expect("flush should succeed");
    }

    let spans = exporter.get_finished_spans().unwrap();
    let completion_spans: Vec<_> = spans
        .iter()
        .filter(|s| s.name.as_ref() == "chat.completion")
        .collect();

    assert!(
        !completion_spans.is_empty(),
        "should have chat.completion span"
    );

    let attrs = span_attrs(completion_spans[0]);

    // If the streaming passthrough broke token instrumentation, these fail.
    assert!(
        attrs.contains_key("llm.token_count.prompt"),
        "streaming span should record llm.token_count.prompt from final SSE chunk; attrs: {attrs:?}"
    );
    assert!(
        attrs.contains_key("llm.token_count.total"),
        "streaming span should record llm.token_count.total from final SSE chunk; attrs: {attrs:?}"
    );
}

#[tokio::test]
async fn test_streaming_span_records_usage_with_many_tools() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(Arc::new(MockStreamingBackend)).await;
    let mut body = chat_request_body(true);
    body["tools"] = serde_json::Value::Array(
        (0..40)
            .map(|i| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": format!("tool_{i}"),
                        "description": "A test tool",
                        "parameters": {"type": "object", "properties": {"value": {"type": "string"}}},
                    },
                })
            })
            .collect(),
    );

    {
        let _guard = tracing::subscriber::set_default(subscriber);

        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let _ = resp.into_body().collect().await.unwrap().to_bytes();
    }

    for r in provider.force_flush() {
        r.expect("flush should succeed");
    }

    let spans = exporter.get_finished_spans().unwrap();
    let span = spans
        .iter()
        .find(|span| span.name.as_ref() == "chat.completion")
        .unwrap();
    let attrs = span_attrs(span);

    assert_eq!(
        attrs.get("llm.token_count.total").map(String::as_str),
        Some("13")
    );
    let recorded_tools = attrs
        .keys()
        .filter(|key| key.starts_with("llm.tools.") && key.ends_with(".tool.json_schema"))
        .count();
    assert!(recorded_tools > 0 && recorded_tools < 40);
    assert!(attrs
        .get(&format!(
            "llm.tools.{}.tool.json_schema",
            recorded_tools - 1
        ))
        .is_some_and(|tool| {
            serde_json::from_str::<serde_json::Value>(tool)
                .ok()
                .and_then(|tool| {
                    tool.get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .as_deref()
                == Some(format!("tool_{}", recorded_tools - 1).as_str())
        }));
    assert!(
        !attrs.contains_key(&format!("llm.tools.{recorded_tools}.tool.json_schema")),
        "tools after the request-side budget should be omitted: {attrs:?}"
    );
    assert!(!attrs.contains_key("llm.tools.0"));
    let truncation: serde_json::Value = serde_json::from_str(
        attrs
            .get("chalk.trace.input.truncation")
            .expect("tool truncation should be explicit"),
    )
    .unwrap();
    assert_eq!(truncation["tools"]["recorded"], recorded_tools);
    assert_eq!(truncation["tools"]["total"], 40);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&attrs["input.value"]).unwrap(),
        body,
        "raw input must preserve every tool and request option when the preview is truncated"
    );
    assert!(
        attrs.len() < 128,
        "tool attributes must leave room for stream metadata: {attrs:?}"
    );
}

#[tokio::test]
async fn test_input_assistant_tool_calls_are_flattened() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(Arc::new(MockBackend)).await;
    let body = serde_json::json!({
        "model": "mock-model",
        "messages": [
            {"role": "user", "content": "look it up"},
            {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"id\":7}"}
                }]
            },
            {"role": "tool", "tool_call_id": "call-1", "content": "found"}
        ]
    });

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let request = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    for result in provider.force_flush() {
        result.unwrap();
    }
    let spans = exporter.get_finished_spans().unwrap();
    let llm_span = spans
        .iter()
        .find(|span| span.name.as_ref() == "chat.completion")
        .unwrap();
    assert_eq!(
        llm_span.dropped_attributes_count, 0,
        "the writer must stay below the SDK's first-N attribute limit"
    );
    let attrs = span_attrs(llm_span);

    assert_eq!(
        attrs
            .get("llm.input_messages.1.message.tool_calls.0.tool_call.id")
            .map(String::as_str),
        Some("call-1")
    );
    assert_eq!(
        attrs
            .get("llm.input_messages.1.message.tool_calls.0.tool_call.function.name")
            .map(String::as_str),
        Some("lookup")
    );
    assert_eq!(
        attrs
            .get("llm.input_messages.1.message.tool_calls.0.tool_call.function.arguments")
            .map(String::as_str),
        Some("{\"id\":7}")
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(attrs.get("input.value").unwrap()).unwrap(),
        body,
        "input.value should preserve the caller request, not the effective provider request"
    );
    assert!(!attrs.contains_key("chalk.trace.input.truncation"));
}

#[tokio::test]
async fn test_input_message_budget_keeps_newest_messages() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(Arc::new(MockBackend)).await;
    let messages: Vec<_> = (0..40)
        .map(|i| serde_json::json!({"role": "user", "content": format!("message-{i}")}))
        .collect();
    let body = serde_json::json!({"model": "mock-model", "messages": messages});

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let request = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    for result in provider.force_flush() {
        result.unwrap();
    }
    let spans = exporter.get_finished_spans().unwrap();
    let attrs = span_attrs(
        spans
            .iter()
            .find(|span| span.name.as_ref() == "chat.completion")
            .unwrap(),
    );

    let truncation: serde_json::Value = serde_json::from_str(
        attrs
            .get("chalk.trace.input.truncation")
            .expect("message truncation should be explicit"),
    )
    .unwrap();
    let recorded = truncation["messages"]["recorded"].as_u64().unwrap() as usize;
    let source_start = 40 - recorded;
    assert_eq!(truncation["messages"]["total"], 40);
    assert_eq!(truncation["messages"]["omitted_before"], source_start);
    assert_eq!(truncation["messages"]["source_start_index"], source_start);
    assert_eq!(
        attrs
            .get(&format!("llm.input_messages.{source_start}.message.role"))
            .map(String::as_str),
        Some("user")
    );
    assert_eq!(
        attrs
            .get("llm.input_messages.39.message.content")
            .map(String::as_str),
        Some("message-39")
    );
    if source_start > 0 {
        assert!(!attrs.contains_key(&format!(
            "llm.input_messages.{}.message.role",
            source_start - 1
        )));
    }
    assert!(attrs.contains_key("llm.token_count.total"));
    assert!(attrs.contains_key("output.value"));
}

#[tokio::test]
async fn test_oversized_input_without_object_storage_keeps_metadata() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(Arc::new(MockBackend)).await;
    let content = "x".repeat(300 * 1024);
    let body = serde_json::json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": content}]
    });

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let request = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    for result in provider.force_flush() {
        result.unwrap();
    }
    let spans = exporter.get_finished_spans().unwrap();
    let attrs = span_attrs(
        spans
            .iter()
            .find(|span| span.name.as_ref() == "chat.completion")
            .unwrap(),
    );

    assert!(!attrs.contains_key("input.value"));
    assert!(!attrs.contains_key("input.mime_type"));
    assert_eq!(
        attrs
            .get("chalk.trace.input.upload_omitted")
            .map(String::as_str),
        Some("object_storage_disabled")
    );
    assert!(attrs.contains_key("chalk.trace.input.content_bytes"));
    assert_eq!(
        attrs
            .get("llm.input_messages.0.message.role")
            .map(String::as_str),
        Some("user")
    );
    assert!(!attrs.contains_key("llm.input_messages.0.message.content"));
    let truncation: serde_json::Value = serde_json::from_str(
        attrs
            .get("chalk.trace.input.truncation")
            .expect("omitted flattened content should be explicit"),
    )
    .unwrap();
    assert_eq!(truncation["messages"]["content_fields_omitted"], 1);
    assert_eq!(
        attrs.get("llm.model_name").map(String::as_str),
        Some("mock-model")
    );
    assert_eq!(
        attrs.get("llm.token_count.total").map(String::as_str),
        Some("15")
    );
    assert!(attrs.contains_key("output.value"));
}

#[tokio::test]
async fn test_oversized_output_without_object_storage_keeps_tool_metadata() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(Arc::new(MockBackend)).await;
    let body = serde_json::json!({
        "model": "oversized-output-model",
        "messages": [{"role": "user", "content": "hello"}]
    });

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let request = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    for result in provider.force_flush() {
        result.unwrap();
    }
    let spans = exporter.get_finished_spans().unwrap();
    let attrs = span_attrs(
        spans
            .iter()
            .find(|span| span.name.as_ref() == "chat.completion")
            .unwrap(),
    );

    assert!(!attrs.contains_key("output.value"));
    assert!(!attrs.contains_key("output.mime_type"));
    assert_eq!(
        attrs
            .get("chalk.trace.output.upload_omitted")
            .map(String::as_str),
        Some("object_storage_disabled")
    );
    assert!(attrs.contains_key("chalk.trace.output.content_bytes"));
    assert_eq!(
        attrs
            .get("llm.output_messages.0.message.tool_calls.0.tool_call.id")
            .map(String::as_str),
        Some("output-call-1")
    );
    assert_eq!(
        attrs
            .get("llm.output_messages.0.message.tool_calls.0.tool_call.function.name")
            .map(String::as_str),
        Some("notify")
    );
    assert_eq!(
        attrs
            .get("llm.output_messages.0.message.tool_calls.0.tool_call.function.arguments")
            .map(String::as_str),
        Some("{\"ok\":true}")
    );
    assert!(!attrs.contains_key("llm.output_messages.0.message.content"));
    let truncation: serde_json::Value = serde_json::from_str(
        attrs
            .get("chalk.trace.output.truncation")
            .expect("omitted flattened output should be explicit"),
    )
    .unwrap();
    assert_eq!(truncation["messages"]["content_fields_omitted"], 1);
    assert_eq!(
        attrs.get("llm.token_count.total").map(String::as_str),
        Some("15")
    );
    assert_eq!(
        attrs.get("llm.finish_reason").map(String::as_str),
        Some("stop")
    );
}

#[tokio::test]
async fn test_oversized_newest_input_keeps_metadata_and_smaller_tools() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(Arc::new(MockBackend)).await;
    let body = serde_json::json!({
        "model": "mock-model",
        "messages": [
            {"role": "user", "content": "older"},
            {"role": "user", "content": "x".repeat(300 * 1024)}
        ],
        "tools": [
            {"type": "function", "function": {"name": "first", "parameters": {"type": "object"}}},
            {"type": "function", "function": {"name": "second", "parameters": {"type": "object"}}}
        ]
    });

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let request = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    for result in provider.force_flush() {
        result.unwrap();
    }
    let spans = exporter.get_finished_spans().unwrap();
    let attrs = span_attrs(
        spans
            .iter()
            .find(|span| span.name.as_ref() == "chat.completion")
            .unwrap(),
    );

    assert_eq!(
        attrs
            .get("llm.input_messages.0.message.content")
            .map(String::as_str),
        Some("older")
    );
    assert_eq!(
        attrs
            .get("llm.input_messages.1.message.role")
            .map(String::as_str),
        Some("user")
    );
    assert!(!attrs.contains_key("llm.input_messages.1.message.content"));
    assert!(attrs.contains_key("llm.tools.0.tool.json_schema"));
    assert!(attrs.contains_key("llm.tools.1.tool.json_schema"));
}

#[tokio::test]
async fn test_oversized_tool_schema_does_not_hide_later_small_tools() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(Arc::new(MockBackend)).await;
    let body = serde_json::json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [
            {"type": "function", "function": {"name": "huge", "description": "x".repeat(300 * 1024), "parameters": {"type": "object"}}},
            {"type": "function", "function": {"name": "small", "parameters": {"type": "object"}}}
        ]
    });

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let request = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    for result in provider.force_flush() {
        result.unwrap();
    }
    let spans = exporter.get_finished_spans().unwrap();
    let attrs = span_attrs(
        spans
            .iter()
            .find(|span| span.name.as_ref() == "chat.completion")
            .unwrap(),
    );
    assert!(!attrs.contains_key("llm.tools.0.tool.json_schema"));
    let recorded: serde_json::Value =
        serde_json::from_str(attrs.get("llm.tools.1.tool.json_schema").unwrap()).unwrap();
    assert_eq!(recorded["function"]["name"], "small");
    let truncation: serde_json::Value =
        serde_json::from_str(attrs.get("chalk.trace.input.truncation").unwrap()).unwrap();
    assert_eq!(truncation["tools"]["total"], 2);
    assert_eq!(truncation["tools"]["recorded"], 1);
}

#[tokio::test]
async fn test_oversized_output_tool_arguments_are_not_reinlined() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(Arc::new(MockBackend)).await;
    let body = serde_json::json!({
        "model": "oversized-tool-arguments-model",
        "messages": [{"role": "user", "content": "hello"}]
    });

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let request = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    for result in provider.force_flush() {
        result.unwrap();
    }
    let spans = exporter.get_finished_spans().unwrap();
    let attrs = span_attrs(
        spans
            .iter()
            .find(|span| span.name.as_ref() == "chat.completion")
            .unwrap(),
    );

    assert!(!attrs
        .contains_key("llm.output_messages.0.message.tool_calls.0.tool_call.function.arguments"));
    assert_eq!(
        attrs
            .get("llm.output_messages.0.message.tool_calls.0.tool_call.function.name")
            .map(String::as_str),
        Some("notify")
    );
    let truncation: serde_json::Value =
        serde_json::from_str(attrs.get("chalk.trace.output.truncation").unwrap()).unwrap();
    assert_eq!(truncation["tool_calls"]["arguments_omitted"], 1);
}

#[tokio::test]
async fn test_wide_output_preserves_fixed_metadata_before_tool_calls() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(Arc::new(MockBackend)).await;
    let body = serde_json::json!({
        "model": "wide-output-model",
        "messages": [{"role": "user", "content": "hello"}]
    });

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let request = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    for result in provider.force_flush() {
        result.unwrap();
    }
    let spans = exporter.get_finished_spans().unwrap();
    let llm_span = spans
        .iter()
        .find(|span| span.name.as_ref() == "chat.completion")
        .unwrap();
    assert_eq!(
        llm_span.dropped_attributes_count, 0,
        "the writer must stay below the SDK's first-N attribute limit"
    );
    let attrs = span_attrs(llm_span);

    assert_eq!(
        attrs.get("llm.token_count.total").map(String::as_str),
        Some("15")
    );
    assert_eq!(
        attrs.get("llm.finish_reason").map(String::as_str),
        Some("stop")
    );
    assert!(attrs.contains_key("chalk.trace.output.truncation"));
    assert!(
        attrs.len() <= 128,
        "span exceeded the configured SDK limit: {attrs:?}"
    );
}

/// The four standard SSE chunks of a streamed completion: role announcement,
/// one content delta, finish + usage, and the [DONE] sentinel.
fn standard_sse_chunks() -> Vec<bytes::Bytes> {
    vec![
        bytes::Bytes::from(
            "data: {\"id\":\"chatcmpl-mock-stream\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
        ),
        bytes::Bytes::from(
            "data: {\"id\":\"chatcmpl-mock-stream\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello!\"},\"finish_reason\":null}]}\n\n",
        ),
        bytes::Bytes::from(
            "data: {\"id\":\"chatcmpl-mock-stream\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"total_tokens\":13}}\n\n",
        ),
        bytes::Bytes::from("data: [DONE]\n\n"),
    ]
}

/// Send a streaming chat request through `app` under the given subscriber and
/// drain the SSE body, returning the exported `chat.completion` span.
async fn run_streaming_request(
    app: Router,
    provider: &TracerProvider,
    exporter: &InMemorySpanExporter,
    subscriber: impl tracing::Subscriber + Send + Sync + 'static,
) -> opentelemetry_sdk::export::trace::SpanData {
    {
        let _guard = tracing::subscriber::set_default(subscriber);

        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(
                serde_json::to_vec(&chat_request_body(true)).unwrap(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = resp.into_body().collect().await.unwrap().to_bytes();
    }

    for r in provider.force_flush() {
        r.expect("flush should succeed");
    }

    let spans = exporter.get_finished_spans().unwrap();
    spans
        .iter()
        .find(|s| s.name.as_ref() == "chat.completion")
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "expected a 'chat.completion' span; got: {:?}",
                spans.iter().map(|s| s.name.as_ref()).collect::<Vec<_>>()
            )
        })
}

/// The span must cover the whole stream, not just the time to open it. With
/// 100ms between each of the 4 chunks the stream takes ~400ms; before the
/// `SseSpanTap` fix the span ended when the response headers were returned
/// (a few ms), because nothing re-entered the span while the body streamed.
#[tokio::test]
async fn test_streaming_span_duration_covers_full_stream() {
    let (provider, exporter, subscriber) = setup_otel();
    let backend = MockChunkStreamBackend {
        chunks: standard_sse_chunks(),
        delay_ms: 100,
    };
    let app = build_app(Arc::new(backend)).await;

    let span = run_streaming_request(app, &provider, &exporter, subscriber).await;

    let duration = span
        .end_time
        .duration_since(span.start_time)
        .unwrap_or_default();
    assert!(
        duration >= std::time::Duration::from_millis(300),
        "streaming chat.completion span should span the whole SSE stream (~400ms), got {duration:?}"
    );
}

/// The streamed deltas must be assembled into OpenInference output attributes,
/// like the non-streaming path.
#[tokio::test]
async fn test_streaming_span_records_output_messages() {
    let (provider, exporter, subscriber) = setup_otel();
    let backend = MockChunkStreamBackend {
        chunks: standard_sse_chunks(),
        delay_ms: 0,
    };
    let app = build_app(Arc::new(backend)).await;

    let span = run_streaming_request(app, &provider, &exporter, subscriber).await;
    let attrs = span_attrs(&span);

    assert_eq!(
        attrs
            .get("llm.output_messages.0.message.role")
            .map(String::as_str),
        Some("assistant"),
        "streaming span should record the assembled output role; attrs: {attrs:?}"
    );
    assert_eq!(
        attrs
            .get("llm.output_messages.0.message.content")
            .map(String::as_str),
        Some("Hello!"),
        "streaming span should record the assembled output content; attrs: {attrs:?}"
    );
    assert!(
        attrs
            .get("output.value")
            .is_some_and(|v| v.contains("Hello!")),
        "streaming span should record output.value with the assembled content; attrs: {attrs:?}"
    );
    assert_eq!(
        attrs.get("llm.stream").map(String::as_str),
        Some("true"),
        "streaming span should record llm.stream=true; attrs: {attrs:?}"
    );
}

/// `llm.time_to_first_token_ms` must measure the first *content-bearing* chunk,
/// not stream-open (~0ms) and not the role-only announcement chunk (~100ms in,
/// with this backend's 100ms per-chunk delay). The content chunk is the second
/// of four, so it lands ~200ms in.
#[tokio::test]
async fn test_streaming_span_records_time_to_first_token() {
    let (provider, exporter, subscriber) = setup_otel();
    let backend = MockChunkStreamBackend {
        chunks: standard_sse_chunks(),
        delay_ms: 100,
    };
    let app = build_app(Arc::new(backend)).await;

    let span = run_streaming_request(app, &provider, &exporter, subscriber).await;
    let attrs = span_attrs(&span);

    let ttft: i64 = attrs
        .get("llm.time_to_first_token_ms")
        .unwrap_or_else(|| panic!("expected llm.time_to_first_token_ms; attrs: {attrs:?}"))
        .parse()
        .expect("llm.time_to_first_token_ms should parse as an integer");
    assert!(
        ttft >= 150,
        "time to first token should reflect the content chunk (~200ms), not stream-open \
         (~0ms) or the role chunk (~100ms); got {ttft}ms"
    );
    assert!(
        ttft < 10_000,
        "sanity bound to catch a stuck/never-set clock, not a tight timing assertion; got {ttft}ms"
    );
}

/// An SSE event split across two network chunks must still be assembled into
/// the full output content (the tap buffers partial lines across chunks).
#[tokio::test]
async fn test_streaming_span_assembles_content_split_across_chunks() {
    let (provider, exporter, subscriber) = setup_otel();
    let backend = MockChunkStreamBackend {
        chunks: vec![
            // One SSE event split mid-JSON across two byte chunks.
            bytes::Bytes::from(
                "data: {\"id\":\"chatcmpl-mock-stream\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel",
            ),
            bytes::Bytes::from(
                "lo, world!\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n",
            ),
        ],
        delay_ms: 0,
    };
    let app = build_app(Arc::new(backend)).await;

    let span = run_streaming_request(app, &provider, &exporter, subscriber).await;
    let attrs = span_attrs(&span);

    assert_eq!(
        attrs
            .get("llm.output_messages.0.message.content")
            .map(String::as_str),
        Some("Hello, world!"),
        "content split across byte chunks should be assembled in full; attrs: {attrs:?}"
    );
}

/// A multibyte character split BETWEEN its own encoded bytes across two
/// network chunks must still decode intact — decoding each chunk with
/// `from_utf8_lossy` before buffering (rather than after a complete line is
/// assembled) would turn the split character into replacement characters.
#[tokio::test]
async fn test_streaming_span_survives_multibyte_char_split_across_chunks() {
    let (provider, exporter, subscriber) = setup_otel();

    let event = "data: {\"id\":\"chatcmpl-mock-stream\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"héllo\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";
    let event_bytes = event.as_bytes();
    // Split inside the 2-byte UTF-8 encoding of 'é' (0xC3 0xA9): landing right
    // after the leading byte is exactly the corruption-prone boundary.
    let split_at = event.find('é').expect("event contains é") + 1;

    let backend = MockChunkStreamBackend {
        chunks: vec![
            bytes::Bytes::copy_from_slice(&event_bytes[..split_at]),
            bytes::Bytes::copy_from_slice(&event_bytes[split_at..]),
        ],
        delay_ms: 0,
    };
    let app = build_app(Arc::new(backend)).await;

    let span = run_streaming_request(app, &provider, &exporter, subscriber).await;
    let attrs = span_attrs(&span);

    assert_eq!(
        attrs
            .get("llm.output_messages.0.message.content")
            .map(String::as_str),
        Some("héllo"),
        "a multibyte char split across byte chunks should decode intact, not as \
         U+FFFD replacement characters; attrs: {attrs:?}"
    );
}

/// OpenAI's role-announcement chunk commonly carries `"content":""`; that must
/// not stamp time-to-first-token any more than the role-only chunk does. With
/// 100ms between chunks, the empty-content chunk lands at ~100ms and the real
/// content lands at ~200ms.
#[tokio::test]
async fn test_streaming_span_ttft_ignores_empty_content_delta() {
    let (provider, exporter, subscriber) = setup_otel();
    let chunks = vec![
        bytes::Bytes::from(
            "data: {\"id\":\"chatcmpl-mock-stream\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        ),
        bytes::Bytes::from(
            "data: {\"id\":\"chatcmpl-mock-stream\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello!\"},\"finish_reason\":null}]}\n\n",
        ),
        bytes::Bytes::from(
            "data: {\"id\":\"chatcmpl-mock-stream\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"total_tokens\":13}}\n\n",
        ),
        bytes::Bytes::from("data: [DONE]\n\n"),
    ];
    let backend = MockChunkStreamBackend {
        chunks,
        delay_ms: 100,
    };
    let app = build_app(Arc::new(backend)).await;

    let span = run_streaming_request(app, &provider, &exporter, subscriber).await;
    let attrs = span_attrs(&span);

    let ttft: i64 = attrs
        .get("llm.time_to_first_token_ms")
        .unwrap_or_else(|| panic!("expected llm.time_to_first_token_ms; attrs: {attrs:?}"))
        .parse()
        .expect("llm.time_to_first_token_ms should parse as an integer");
    assert!(
        ttft >= 150,
        "the empty-content role chunk (~100ms) must not stamp time-to-first-token; \
         expected the real content chunk (~200ms), got {ttft}ms"
    );
}

#[tokio::test]
async fn test_embedding_span_has_openinference_kind() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_full_app(Arc::new(MockEmbeddingBackend)).await;

    {
        let _guard = tracing::subscriber::set_default(subscriber);

        let body = serde_json::json!({
            "model": "text-embedding-ada-002",
            "input": "Hello world",
        });
        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/embeddings")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    for r in provider.force_flush() {
        r.expect("flush should succeed");
    }

    let spans = exporter.get_finished_spans().unwrap();
    let embedding_spans: Vec<_> = spans
        .iter()
        .filter(|s| s.name.as_ref() == "embedding.create")
        .collect();

    assert!(
        !embedding_spans.is_empty(),
        "expected an 'embedding.create' span; got: {:?}",
        spans.iter().map(|s| s.name.as_ref()).collect::<Vec<_>>()
    );

    let attrs = span_attrs(embedding_spans[0]);
    assert_eq!(
        attrs.get("openinference.span.kind").map(String::as_str),
        Some("EMBEDDING"),
        "embedding.create span must have openinference.span.kind=EMBEDDING; attrs: {attrs:?}"
    );
    assert_eq!(
        attrs.get("llm.model_name").map(String::as_str),
        Some("text-embedding-ada-002")
    );
    assert_eq!(
        attrs.get("llm.system").map(String::as_str),
        Some("mock-embedding")
    );
    assert!(
        attrs.contains_key("llm.token_count.prompt"),
        "should have prompt token count"
    );
    assert!(
        attrs.contains_key("embedding.embeddings_count"),
        "should have embeddings_count"
    );
}

#[tokio::test]
async fn test_oversized_embedding_input_uses_shared_content_policy() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_full_app(Arc::new(MockEmbeddingBackend)).await;

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let body = serde_json::json!({
            "model": "text-embedding-ada-002",
            "input": "x".repeat(300 * 1024),
        });
        let request = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/embeddings")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    for result in provider.force_flush() {
        result.unwrap();
    }
    let spans = exporter.get_finished_spans().unwrap();
    let attrs = span_attrs(
        spans
            .iter()
            .find(|span| span.name.as_ref() == "embedding.create")
            .unwrap(),
    );

    assert!(!attrs.contains_key("input.value"));
    assert!(!attrs.contains_key("input.mime_type"));
    assert_eq!(
        attrs
            .get("chalk.trace.input.upload_omitted")
            .map(String::as_str),
        Some("object_storage_disabled")
    );
    assert_eq!(
        attrs.get("embedding.input_count").map(String::as_str),
        Some("1")
    );
    assert!(attrs.contains_key("llm.token_count.total"));
    assert!(attrs.contains_key("chalk.trace.input.truncation"));
}

#[tokio::test]
async fn test_embedding_text_attributes_keep_response_indices_after_omission() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_full_app(Arc::new(MockEmbeddingBackend)).await;

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let body = serde_json::json!({
            "model": "text-embedding-ada-002",
            "input": ["x".repeat(80 * 1024), "keep-index-one"],
        });
        let request = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/embeddings")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    for result in provider.force_flush() {
        result.unwrap();
    }
    let spans = exporter.get_finished_spans().unwrap();
    let attrs = span_attrs(
        spans
            .iter()
            .find(|span| span.name.as_ref() == "embedding.create")
            .unwrap(),
    );

    assert!(!attrs.contains_key("embedding.embeddings.0.embedding.text"));
    assert_eq!(
        attrs
            .get("embedding.embeddings.1.embedding.text")
            .map(String::as_str),
        Some("keep-index-one")
    );
}

#[tokio::test]
async fn test_audio_transcription_span_has_openinference_kind() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_full_app(Arc::new(MockTranscriptionBackend)).await;

    {
        let _guard = tracing::subscriber::set_default(subscriber);

        // This tracing mock implements the legacy typed audio interface, whose
        // file part uses the default MIME. Native MIME fidelity has its own fixtures.
        // Build a minimal multipart/form-data body with model and file fields
        let boundary = "test-boundary-audio-99";
        let body = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"model\"\r\n\
             \r\n\
             whisper-1\r\n\
             --{boundary}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n\
             Content-Type: application/octet-stream\r\n\
             \r\n\
             RIFF\r\n\
             --{boundary}--\r\n"
        );
        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/audio/transcriptions")
            .header(
                http::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    for r in provider.force_flush() {
        r.expect("flush should succeed");
    }

    let spans = exporter.get_finished_spans().unwrap();
    let transcription_spans: Vec<_> = spans
        .iter()
        .filter(|s| s.name.as_ref() == "audio.transcription")
        .collect();

    assert!(
        !transcription_spans.is_empty(),
        "expected an 'audio.transcription' span; got: {:?}",
        spans.iter().map(|s| s.name.as_ref()).collect::<Vec<_>>()
    );

    let attrs = span_attrs(transcription_spans[0]);
    assert_eq!(
        attrs.get("openinference.span.kind").map(String::as_str),
        Some("LLM"),
        "audio.transcription span must have openinference.span.kind=LLM; attrs: {attrs:?}"
    );
    assert_eq!(
        attrs.get("llm.model_name").map(String::as_str),
        Some("whisper-1")
    );
    assert_eq!(
        attrs.get("llm.system").map(String::as_str),
        Some("mock-transcription")
    );
    assert!(
        attrs.contains_key("output.value"),
        "should have transcription output text"
    );
}

#[tokio::test]
async fn test_image_create_span_has_openinference_kind() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_full_app(Arc::new(MockImageBackend)).await;

    {
        let _guard = tracing::subscriber::set_default(subscriber);

        let body = serde_json::json!({
            "prompt": "A beautiful sunset over the ocean",
            "model": "dall-e-3",
        });
        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/images/generations")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {TEST_API_KEY}"),
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    for r in provider.force_flush() {
        r.expect("flush should succeed");
    }

    let spans = exporter.get_finished_spans().unwrap();
    let image_spans: Vec<_> = spans
        .iter()
        .filter(|s| s.name.as_ref() == "image.create")
        .collect();

    assert!(
        !image_spans.is_empty(),
        "expected an 'image.create' span; got: {:?}",
        spans.iter().map(|s| s.name.as_ref()).collect::<Vec<_>>()
    );

    let attrs = span_attrs(image_spans[0]);
    assert_eq!(
        attrs.get("openinference.span.kind").map(String::as_str),
        Some("LLM"),
        "image.create span must have openinference.span.kind=LLM; attrs: {attrs:?}"
    );
    assert_eq!(
        attrs.get("llm.model_name").map(String::as_str),
        Some("dall-e-3")
    );
    assert_eq!(
        attrs.get("llm.system").map(String::as_str),
        Some("mock-image")
    );
    assert!(
        attrs.contains_key("input.value"),
        "should have prompt as input.value"
    );
}

// Run in a fresh process so other content-assertion fixtures cannot opt us in.
#[tokio::test]
async fn default_tracing_does_not_export_sensitive_chat_content() {
    if std::env::var_os("ROUTER_PRIVACY_TEST_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "span_test::default_tracing_does_not_export_sensitive_chat_content",
                "--nocapture",
            ])
            .env("ROUTER_PRIVACY_TEST_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::SimpleSpanProcessor;
    use tracing_subscriber::layer::SubscriberExt;
    assert!(!openai_server::handlers::content_logging_enabled());
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("privacy-test")));
    let app = build_app(Arc::new(MockBackend)).await;
    let body = serde_json::json!({"model":"gpt-4o", "messages":[
        {"role":"user","content":[{"type":"text","text":"secret-prompt"},
            {"type":"image_url","image_url":{"url":"https://example.com/secret-image"}},
            {"type":"file","file":{"file_data":"secret-attachment","filename":"secret-file"}}]},
        {"role":"assistant","content":null,"tool_calls":[{"id":"call1","type":"function","function":{"name":"test","arguments":"secret-tool-arguments"}}]}
    ]});
    let response = {
        let _guard = tracing::subscriber::set_default(subscriber);
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {TEST_API_KEY}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    };
    assert_eq!(response.status(), StatusCode::OK);
    for result in provider.force_flush() {
        result.unwrap();
    }
    let spans = exporter.get_finished_spans().unwrap();
    assert!(spans.iter().any(|s| s.name == "chat.completion"));
    let attributes = spans
        .iter()
        .flat_map(|s| &s.attributes)
        .map(|a| format!("{}={}", a.key, a.value))
        .collect::<Vec<_>>()
        .join("\n");
    for secret in [
        "secret-prompt",
        "secret-image",
        "secret-attachment",
        "secret-file",
        "secret-tool-arguments",
        "Hello from mock!",
    ] {
        assert!(
            !attributes.contains(secret),
            "leaked {secret}: {attributes}"
        );
    }
    assert!(attributes.contains("llm.model_name"));
}
