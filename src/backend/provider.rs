use async_trait::async_trait;
use futures::Stream;
use std::fmt;
use std::pin::Pin;

pub use crate::adapters::responses_chat::{
    create_response_stream_via_chat, create_response_via_chat,
};
use crate::provider_connections::StoredConnection;
use crate::responses::ResponsesRequest;
pub use crate::translation::responses_chat::synthesize_response_stream;
use crate::types::{
    ChatCompletionRequest, ChatCompletionResponse, CreateEmbeddingRequest, CreateEmbeddingResponse,
    CreateImageRequest, CreateModerationRequest, CreateModerationResponse, CreateSpeechRequest,
    ImageResponse, ListResponse, Model, TranscriptionResponse,
};

/// Native Anthropic endpoint selected by the HTTP compatibility frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicRequestEndpoint {
    Messages,
    CountTokens,
}

/// Caller headers that are safe to forward to a native Anthropic upstream.
///
/// Authentication and all non-Anthropic headers are deliberately discarded at
/// the HTTP boundary. The resolved leaf backend supplies its own credential.
#[derive(Debug, Clone, Default)]
pub struct AnthropicRequestHeaders(http::HeaderMap);

impl AnthropicRequestHeaders {
    pub fn from_header_map(headers: &http::HeaderMap) -> Self {
        let mut forwarded = http::HeaderMap::new();
        for (name, value) in headers {
            if name.as_str().starts_with("anthropic-") {
                forwarded.append(name.clone(), value.clone());
            }
        }
        Self(forwarded)
    }

    pub fn iter(&self) -> http::header::Iter<'_, http::HeaderValue> {
        self.0.iter()
    }
}

/// A stream of raw SSE bytes from an upstream provider.
pub type SseByteStream = Pin<Box<dyn Stream<Item = Result<bytes::Bytes, BackendError>> + Send>>;

/// Caller headers that are safe to forward to a Responses API upstream.
///
/// Authentication and hop-by-hop headers are deliberately never retained.
/// Construct this value at the HTTP boundary instead of handing a provider the
/// caller's complete header map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponsesRequestHeaders(Vec<(String, String)>);

impl ResponsesRequestHeaders {
    pub fn from_header_map(headers: &http::HeaderMap) -> Self {
        const ALLOWED: &[&str] = &[
            "x-client-request-id",
            "x-codex-installation-id",
            "x-codex-turn-state",
            "x-oai-attestation",
            "x-responsesapi-include-timing-metrics",
            "originator",
            "x-openai-product-sku",
            "traceparent",
            "tracestate",
        ];

        Self(
            headers
                .iter()
                .filter(|(name, _)| ALLOWED.contains(&name.as_str()))
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_string(), value.to_string()))
                })
                .collect(),
        )
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

/// An authoritative HTTP response and a separate view for accounting and tracing.
#[derive(Debug)]
pub struct BackendJsonResponse {
    pub body: serde_json::Value,
    pub response: BackendBodyResponse,
}

impl BackendJsonResponse {
    pub fn from_value(
        body: serde_json::Value,
        headers: Vec<(String, String)>,
    ) -> Result<Self, BackendError> {
        let mut response = BackendBodyResponse::from_json(&body)?;
        response.headers = headers;
        Ok(Self { body, response })
    }

    pub fn from_response(response: BackendBodyResponse) -> Result<Self, BackendError> {
        let body = if response.body.is_empty() && matches!(response.status, 204 | 205) {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&response.body)
                .map_err(|error| BackendError::InvalidResponse(error.to_string()))?
        };
        Ok(Self { body, response })
    }
}

/// A native Chat result and its observational accounting/translation view.
#[derive(Debug)]
pub struct BackendChatResponse {
    pub body: Box<serde_json::value::RawValue>,
    pub completion: ChatCompletionResponse,
    pub status: u16,
    pub content_type: String,
    pub headers: Vec<(String, String)>,
}

impl BackendChatResponse {
    pub fn from_completion(completion: ChatCompletionResponse) -> Result<Self, BackendError> {
        let body = serde_json::value::to_raw_value(&completion)
            .map_err(|error| BackendError::Other(error.to_string()))?;
        Ok(Self {
            body,
            completion,
            status: 200,
            content_type: "application/json".into(),
            headers: Vec::new(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamErrorFormat {
    Openai,
    Other,
}

#[derive(Debug, Clone)]
pub struct OriginalUpstreamError {
    pub body: bytes::Bytes,
    pub content_type: String,
    pub format: UpstreamErrorFormat,
}

/// An authoritative JSON, text, or binary native HTTP body.
#[derive(Debug)]
pub struct BackendBodyResponse {
    pub body: bytes::Bytes,
    pub status: u16,
    pub content_type: String,
    pub headers: Vec<(String, String)>,
}
impl BackendBodyResponse {
    pub fn from_json<T: serde::Serialize>(value: &T) -> Result<Self, BackendError> {
        Ok(Self {
            body: bytes::Bytes::from(
                serde_json::to_vec(value).map_err(|e| BackendError::Other(e.to_string()))?,
            ),
            status: 200,
            content_type: "application/json".into(),
            headers: Vec::new(),
        })
    }
    pub fn from_bytes(body: bytes::Bytes, content_type: impl Into<String>) -> Self {
        Self {
            body,
            status: 200,
            content_type: content_type.into(),
            headers: Vec::new(),
        }
    }
}
#[derive(Debug)]
pub struct BackendJsonView<T> {
    pub response: BackendBodyResponse,
    pub view: T,
}

/// An SSE byte stream plus the explicitly forwardable upstream response headers.
pub struct BackendSseResponse {
    pub stream: SseByteStream,
    pub headers: Vec<(String, String)>,
    pub status: u16,
    pub content_type: String,
}

/// Error type for backend operations
#[derive(Debug, Clone)]
pub enum BackendError {
    /// HTTP request error
    RequestError(std::sync::Arc<reqwest::Error>),
    /// API error from the backend. `headers` carries the subset of upstream
    /// response headers worth forwarding to the caller (rate-limit / retry
    /// signaling); see [`forwardable_upstream_headers`].
    ApiError {
        message: String,
        status: u16,
        headers: Vec<(String, String)>,
        original: Option<Box<OriginalUpstreamError>>,
        /// A provider rejection of the reasoning controls sent on this attempt.
        reasoning_incompatible: bool,
    },
    /// Configuration error
    ConfigError(String),
    /// A credential is stored for the provider but its ciphertext could not be
    /// decrypted (e.g. wrong/rotated `ROUTER_SECRETS_KEK`). Distinct from "not
    /// configured": we refuse to silently fall back to a different key.
    CredentialUnreadable(String),
    /// The provider response could not be decoded; never retry another model.
    InvalidResponse(String),
    /// The router's overall routing deadline expired. An upstream background
    /// job may still be running; this is not a definite provider rejection.
    RoutingDeadlineExceeded,
    /// The presented API key may not reach any destination that could serve
    /// the request. Raised before dispatch, so nothing upstream was contacted.
    PermissionDenied(String),
    /// Other errors
    Other(String),
}

impl BackendError {
    /// Build an API error without native body metadata or reasoning classification.
    pub fn api_error(
        message: impl Into<String>,
        status: u16,
        headers: Vec<(String, String)>,
    ) -> Self {
        Self::ApiError {
            message: message.into(),
            status,
            headers,
            original: None,
            reasoning_incompatible: false,
        }
    }

    /// Build a router-originated API error without upstream headers.
    pub fn api_status(status: u16, message: impl Into<String>) -> Self {
        Self::api_error(message, status, Vec::new())
    }

    /// A model id that no route serves. The caller-facing body is shaped like
    /// OpenAI's own unknown-model error (`model_not_found` on `param: model`),
    /// see [`Self::is_model_not_found`].
    pub fn model_not_found(model: &str) -> Self {
        Self::api_status(
            404,
            format!("{MODEL_NOT_FOUND_PREFIX}{model}' does not exist"),
        )
    }

    /// Whether this is a router-originated [`Self::model_not_found`] error.
    pub fn is_model_not_found(&self) -> bool {
        matches!(
            self,
            BackendError::ApiError {
                status: 404,
                original: None,
                message,
                ..
            } if message.starts_with(MODEL_NOT_FOUND_PREFIX)
        )
    }
}

/// Message prefix shared by [`BackendError::model_not_found`] and
/// [`BackendError::is_model_not_found`].
const MODEL_NOT_FOUND_PREFIX: &str = "model route '";

/// Whether an upstream response header should be forwarded to the caller. We
/// forward rate-limit / retry signaling so clients can back off correctly, plus
/// the upstream request id for debugging.
fn is_forwardable_upstream_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "openai-processing-ms"
        || name == "retry-after"
        || name == "request-id"
        || name == "x-request-id"
        || name == "x-amzn-requestid"
        || name.starts_with("x-ratelimit-")
        || name.starts_with("anthropic-ratelimit-")
}

/// Extract the subset of upstream response headers that should be forwarded to
/// the caller (rate-limit / retry signaling). Header values that aren't valid
/// UTF-8 are skipped.
pub fn forwardable_upstream_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| is_forwardable_upstream_header(name.as_str()))
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_string(), v.to_string()))
        })
        .collect()
}

/// Build a `BackendError::ApiError` from a non-2xx upstream response.
///
/// Captures forwardable rate-limit / retry headers before consuming the
/// body, reads the body as text, then hands that text to `parse_message` so
/// each backend can pull its own provider-specific field (or fields) out of
/// whatever error envelope that upstream uses. Falls back to the raw body
/// text when `parse_message` returns `None` (including when the body isn't
/// the shape `parse_message` expects).
pub(crate) async fn upstream_error(
    response: reqwest::Response,
    parse_message: impl Fn(&str) -> Option<String>,
) -> BackendError {
    upstream_error_with_fallback(response, |_, text| {
        parse_message(text).map(|message| (message, false))
    })
    .await
}

/// As [`upstream_error`], allowing a provider to classify the same parsed
/// diagnostic using the controls actually sent. Unrecognized envelopes and
/// ordinary callers keep the default terminal behavior for client errors.
pub(crate) async fn upstream_error_with_fallback(
    response: reqwest::Response,
    parse_error: impl Fn(u16, &str) -> Option<(String, bool)>,
) -> BackendError {
    let status = response.status().as_u16();
    let headers = forwardable_upstream_headers(response.headers());
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "Unknown error".to_string());
    let (message, reasoning_incompatible) = parse_error(status, &text).unwrap_or((text, false));
    BackendError::ApiError {
        message,
        status,
        headers,
        original: None,
        reasoning_incompatible,
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::RequestError(e) => write!(f, "Request error: {}", e),
            BackendError::ApiError { message, status, .. } => {
                write!(f, "API error ({}): {}", status, message)
            }
            BackendError::ConfigError(msg) => write!(f, "Configuration error: {}", msg),
            BackendError::CredentialUnreadable(provider) => write!(
                f,
                "stored credential for provider '{}' could not be decrypted (check ROUTER_SECRETS_KEK)",
                provider
            ),
            BackendError::InvalidResponse(msg) => write!(f, "Invalid provider response: {msg}"),
            BackendError::RoutingDeadlineExceeded => write!(f, "route attempt deadline exceeded"),
            BackendError::PermissionDenied(msg) => write!(f, "{msg}"),
            BackendError::Other(msg) => write!(f, "Error: {}", msg),
        }
    }
}

impl std::error::Error for BackendError {}

impl From<reqwest::Error> for BackendError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_decode() {
            BackendError::InvalidResponse(err.to_string())
        } else {
            BackendError::RequestError(std::sync::Arc::new(err))
        }
    }
}

pub fn unsupported_response_compaction() -> BackendError {
    BackendError::api_status(
        400,
        "response compaction requires a native Responses API backend",
    )
}

fn unsupported_operation(provider: &str, operation: &str) -> BackendError {
    BackendError::api_status(
        400,
        format!("provider '{provider}' does not support {operation}"),
    )
}

/// How a request `model` resolved to an upstream connection: the real provider
/// kind, the stored connection identity (when resolved through `RouterBackend`),
/// and the upstream model id actually sent. Drives usage attribution and span
/// tagging so a `chalk/*` public id is attributed to its upstream connection
/// rather than the literal prefix.
#[derive(Debug, Clone)]
pub struct RouteResolution {
    /// Real upstream kind (e.g. `"openai"`, `"anthropic"`), or the generic
    /// `"openai-compatible"` for a custom endpoint.
    pub provider_kind: String,
    /// The stored connection id — `Some` when resolved through `RouterBackend`,
    /// `None` for the leaf-adapter default impl.
    pub connection_id: Option<String>,
    /// The stored connection's name (its bespoke identity for `openai-compatible`).
    pub connection_name: String,
    /// The prefix-stripped / route-mapped model id actually sent upstream.
    pub upstream_model_id: String,
    /// The route prefix the stored connection owns (for example `openai/`),
    /// `None` when it owns none or the route resolved without a stored
    /// connection. Lets a bare model id be recognised as the shorthand for
    /// `<prefix><id>` only when this connection really is the one that prefix
    /// names (see `ApiKeyPermission::allows_route`).
    pub connection_prefix: Option<String>,
    /// Immutable id of the `chalk/*` model route the request resolved through;
    /// `None` for a direct model id. API-key route grants are checked against it.
    pub model_route_id: Option<String>,
}

/// A fully-resolved route: the live sub-backend to dispatch to, the prefix-
/// stripped model id to send it, and the [`RouteResolution`] describing the
/// connection it resolved through.
///
/// The point of resolving into this object is that a request resolves the route
/// **exactly once** and then carries a stable `(backend, stripped_model,
/// resolution)` triple through permission enforcement, dispatch, and usage
/// attribution — so a mutable routing snapshot can't shift the request from one
/// connection to another between those steps. Dispatch goes straight to
/// [`Self::backend`] (never back through the router, which would re-resolve).
pub struct ResolvedRoute {
    /// Custom grid captured with its default destination; None for simple/direct routes.
    pub routing: Option<std::sync::Arc<crate::route_grid::RouteGrid>>,
    /// The leaf backend that will actually serve the request.
    pub backend: std::sync::Arc<dyn BackendProvider>,
    /// The model id with any `chalk/`/provider prefix removed (or route-mapped),
    /// exactly as it should be sent upstream — matches
    /// [`RouteResolution::upstream_model_id`].
    pub stripped_model: String,
    /// Provider kind, connection identity, and upstream model id for the resolved
    /// connection. Drives permission gating, span tagging, and usage attribution.
    pub resolution: RouteResolution,
    /// Immutable advisory capability profile resolved in the same snapshot as
    /// the backend and upstream model id.
    pub capabilities: std::sync::Arc<crate::capabilities::ResolvedCapabilityProfile>,
}

impl ResolvedRoute {
    pub fn concrete_model(&self) -> String {
        crate::route_grid::target(
            self.resolution
                .connection_id
                .as_deref()
                .unwrap_or(&self.resolution.provider_kind),
            &self.resolution.upstream_model_id,
        )
    }
}

/// One model reachable through a particular stored provider connection.
///
/// `public_model_id` is the identifier callers submit to the router; usage is
/// recorded under the corresponding `upstream_model_id` after resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionModel {
    pub connection_id: String,
    pub public_model_id: String,
    pub upstream_model_id: String,
    pub capabilities: Option<crate::capabilities::CapabilityProfile>,
}

/// Trait that all backend providers must implement
#[async_trait]
#[allow(clippy::too_many_arguments)]
pub trait BackendProvider: Send + Sync {
    /// Native transports must explicitly carry the authoritative body and headers.
    fn supports_native_chat(&self) -> bool {
        false
    }

    /// Whether a translated (non-native) Chat route can retain every control in
    /// `request`. Routing calls this before dispatch so an unrepresentable
    /// candidate is skipped instead of failing with a client error.
    fn check_translated_chat(&self, _request: &ChatCompletionRequest) -> Result<(), BackendError> {
        Ok(())
    }

    /// Validate the candidate adapter's representation before dispatch, including
    /// backends that select a translated dialect internally. Native provider bodies
    /// remain authoritative and are validated upstream.
    fn check_native_chat(
        &self,
        request: &crate::native_chat::NativeChatRequest,
        upstream_model: &str,
    ) -> Result<(), BackendError> {
        if self.supports_native_chat() {
            return Ok(());
        }
        let mut compatible = request.compatibility_request()?;
        compatible.model = upstream_model.to_owned();
        self.check_translated_chat(&compatible)
    }

    async fn create_native_chat_completion(
        &self,
        request: crate::native_chat::NativeChatRequest,
    ) -> Result<BackendChatResponse, BackendError> {
        if self.supports_native_chat() {
            return Err(BackendError::ConfigError(
                "native Chat envelope forwarding is missing".into(),
            ));
        }
        BackendChatResponse::from_completion(
            self.create_chat_completion(request.compatibility_request()?)
                .await?,
        )
    }

    async fn create_native_chat_completion_stream(
        &self,
        request: crate::native_chat::NativeChatRequest,
    ) -> Result<BackendSseResponse, BackendError> {
        if self.supports_native_chat() {
            return Err(BackendError::ConfigError(
                "native Chat SSE envelope forwarding is missing".into(),
            ));
        }
        Ok(BackendSseResponse {
            stream: self
                .create_chat_completion_stream(request.compatibility_request()?)
                .await?,
            headers: Vec::new(),
            status: 200,
            content_type: "text/event-stream".into(),
        })
    }

    async fn create_native_audio(
        &self,
        request: crate::native_audio::NativeAudioRequest,
    ) -> Result<BackendBodyResponse, BackendError> {
        request.require_compatible()?;
        let file = request.file();
        let name = file.filename.clone().unwrap_or_else(|| "audio.wav".into());
        let temperature = request
            .text("temperature")
            .and_then(|value| value.parse().ok());
        let response = match request.operation {
            crate::native_audio::AudioOperation::Transcription => {
                self.create_transcription(
                    request.model().to_owned(),
                    file.body.clone(),
                    name,
                    request.text("language"),
                    request.text("prompt"),
                    request.text("response_format"),
                    temperature,
                )
                .await?
            }
            crate::native_audio::AudioOperation::Translation => {
                self.create_translation(
                    request.model().to_owned(),
                    file.body.clone(),
                    name,
                    request.text("prompt"),
                    request.text("response_format"),
                    temperature,
                )
                .await?
            }
        };
        BackendBodyResponse::from_json(&response)
    }

    async fn create_native_embedding(
        &self,
        request: crate::native_json::NativeJsonRequest<CreateEmbeddingRequest>,
    ) -> Result<BackendJsonView<CreateEmbeddingResponse>, BackendError> {
        let view = self
            .create_embedding(request.compatibility_request()?)
            .await?;
        Ok(BackendJsonView {
            response: BackendBodyResponse::from_json(&view)?,
            view,
        })
    }
    async fn create_native_image(
        &self,
        request: crate::native_json::NativeJsonRequest<CreateImageRequest>,
    ) -> Result<BackendJsonView<ImageResponse>, BackendError> {
        let view = self.create_image(request.compatibility_request()?).await?;
        Ok(BackendJsonView {
            response: BackendBodyResponse::from_json(&view)?,
            view,
        })
    }
    async fn create_native_speech(
        &self,
        request: crate::native_json::NativeJsonRequest<CreateSpeechRequest>,
    ) -> Result<BackendBodyResponse, BackendError> {
        let content_type = match request.response_format.as_deref().unwrap_or("mp3") {
            "opus" => "audio/ogg",
            "aac" => "audio/aac",
            "flac" => "audio/flac",
            "wav" => "audio/wav",
            "pcm" => "audio/pcm",
            _ => "audio/mpeg",
        };
        let body = self.create_speech(request.compatibility_request()?).await?;
        Ok(BackendBodyResponse::from_bytes(body, content_type))
    }

    /// Discard local route-resolution state after an admin route mutation.
    /// Leaf providers have no routing snapshot to invalidate.
    async fn invalidate_routing_cache(&self) {}

    /// Create a chat completion
    async fn create_chat_completion(
        &self,
        _request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        Err(unsupported_operation(self.name(), "chat completions"))
    }

    /// Send a normalized Decisions request through a provider's native or
    /// adapted decision endpoint. The raw response is retained so the handler
    /// can apply fallback policy before forwarding terminal upstream errors.
    async fn send_decisions_request(
        &self,
        _request: crate::decisions::DecisionsRequest,
    ) -> Result<reqwest::Response, BackendError> {
        Err(unsupported_operation(self.name(), "Decisions requests"))
    }

    /// Native/adapted Responses create transport primitive. The handler selects
    /// this or the conservative chat translation from the resolved profile.
    async fn create_response(
        &self,
        request: ResponsesRequest,
        _headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        create_response_via_chat(self, request).await
    }

    /// Native/adapted Responses streaming transport primitive. Compatibility
    /// synthesis is selected by the handler from the resolved profile.
    async fn create_response_stream(
        &self,
        request: ResponsesRequest,
        _headers: &ResponsesRequestHeaders,
    ) -> Result<BackendSseResponse, BackendError> {
        create_response_stream_via_chat(self, request).await
    }

    /// Compact a Responses conversation. There is no lossless chat-completions
    /// equivalent, so only native providers override this operation.
    async fn compact_response(
        &self,
        _request: crate::schema_json::SchemaJson,
        _headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        Err(unsupported_response_compaction())
    }

    async fn retrieve_response(
        &self,
        _response_id: &str,
        _headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        Err(BackendError::api_status(
            400,
            "response retrieval is unsupported by this backend",
        ))
    }

    async fn retrieve_response_stream(
        &self,
        _response_id: &str,
        _starting_after: Option<u64>,
        _headers: &ResponsesRequestHeaders,
    ) -> Result<BackendSseResponse, BackendError> {
        Err(BackendError::api_status(
            400,
            "response stream resumption is unsupported by this backend",
        ))
    }

    async fn cancel_response(
        &self,
        _response_id: &str,
        _headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        Err(BackendError::api_status(
            400,
            "response cancellation is unsupported by this backend",
        ))
    }

    async fn delete_response(
        &self,
        _response_id: &str,
        _headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        Err(BackendError::api_status(
            400,
            "response deletion is unsupported by this backend",
        ))
    }

    /// Send a provider-native Anthropic request through this resolved leaf.
    /// Only Anthropic-compatible leaf adapters override this operation.
    async fn send_anthropic_request(
        &self,
        _endpoint: AnthropicRequestEndpoint,
        _headers: &AnthropicRequestHeaders,
        _body: bytes::Bytes,
    ) -> Result<reqwest::Response, BackendError> {
        Err(BackendError::api_status(
            400,
            format!(
                "provider '{}' does not support the native Anthropic Messages API",
                self.name()
            ),
        ))
    }

    /// Send a native Gemini Interactions create request without translating its payload.
    async fn send_gemini_interaction(
        &self,
        _headers: &axum::http::HeaderMap,
        _body: bytes::Bytes,
        _stream: bool,
    ) -> Result<reqwest::Response, BackendError> {
        Err(BackendError::api_status(
            400,
            format!(
                "provider '{}' does not support native Gemini Interactions",
                self.name()
            ),
        ))
    }

    /// Send an Anthropic request through a provider-specific semantic adapter
    /// such as Vertex partner-model rawPredict or Bedrock Runtime.
    async fn send_anthropic_adapted_request(
        &self,
        _endpoint: AnthropicRequestEndpoint,
        _headers: &AnthropicRequestHeaders,
        _body: bytes::Bytes,
    ) -> Result<reqwest::Response, BackendError> {
        Err(BackendError::api_status(
            400,
            format!(
                "provider '{}' does not support adapted Anthropic Messages",
                self.name()
            ),
        ))
    }

    async fn count_anthropic_tokens(
        &self,
        _body: bytes::Bytes,
    ) -> Result<BackendJsonResponse, BackendError> {
        Err(BackendError::api_status(
            400,
            format!("provider '{}' has no adapted token counter", self.name()),
        ))
    }

    /// Create embeddings
    async fn create_embedding(
        &self,
        _request: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(unsupported_operation(self.name(), "embeddings"))
    }

    /// Generate images
    async fn create_image(
        &self,
        _request: CreateImageRequest,
    ) -> Result<ImageResponse, BackendError> {
        Err(unsupported_operation(self.name(), "image generation"))
    }

    /// Create speech from text
    async fn create_speech(
        &self,
        _request: CreateSpeechRequest,
    ) -> Result<bytes::Bytes, BackendError> {
        Err(unsupported_operation(self.name(), "speech"))
    }

    /// Create transcription from audio
    async fn create_transcription(
        &self,
        _model: String,
        _file_bytes: bytes::Bytes,
        _file_name: String,
        _language: Option<String>,
        _prompt: Option<String>,
        _response_format: Option<String>,
        _temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(unsupported_operation(self.name(), "transcription"))
    }

    /// Create translation from audio
    async fn create_translation(
        &self,
        _model: String,
        _file_bytes: bytes::Bytes,
        _file_name: String,
        _prompt: Option<String>,
        _response_format: Option<String>,
        _temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(unsupported_operation(self.name(), "translation"))
    }

    /// Create moderation
    async fn create_moderation(
        &self,
        _request: CreateModerationRequest,
    ) -> Result<CreateModerationResponse, BackendError> {
        Err(unsupported_operation(self.name(), "moderation"))
    }

    /// List available models
    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError>;

    /// List models reachable through one saved provider connection. This is a
    /// control-plane catalog operation; leaf backends do not have a connection
    /// registry, so only [`crate::backend::RouterBackend`] supports it.
    async fn list_provider_connection_models(
        &self,
        _connection_ids: &[String],
    ) -> Result<Vec<ConnectionModel>, BackendError> {
        Err(BackendError::Other(
            "listing models for a provider connection requires the router backend".to_string(),
        ))
    }

    /// Perform the bounded model request used by administrative connection
    /// tests. Providers whose model listing is paginated may override this to
    /// validate one page rather than requiring a complete catalog traversal.
    async fn probe_models(&self) -> Result<ListResponse<Model>, BackendError> {
        self.list_models().await
    }

    /// Get a specific model
    async fn get_model(&self, model_id: &str) -> Result<Model, BackendError>;

    /// Stream a chat completion as raw SSE bytes, passing through from the upstream provider.
    /// Default implementation falls back to non-streaming and synthesizes proper SSE delta chunks
    /// in the OpenAI streaming format (object: "chat.completion.chunk", delta.content).
    /// Backends that support native streaming (e.g. OpenAI) override this method.
    async fn create_chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<SseByteStream, BackendError> {
        let response = self.create_chat_completion(request).await?;
        let mut events: Vec<Result<bytes::Bytes, BackendError>> = Vec::new();

        for choice in &response.choices {
            let id = &response.id;
            let created = response.created;
            let model = &response.model;
            let index = choice.index;

            // Chunk 1: role delta
            let role_chunk = serde_json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{"index": index, "delta": {"role": choice.message.role}, "finish_reason": null}]
            });
            if let Ok(json) = serde_json::to_string(&role_chunk) {
                events.push(Ok(bytes::Bytes::from(format!("data: {json}\n\n"))));
            }

            // Chunk 2: content delta
            let content_str = match &choice.message.content {
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                Some(serde_json::Value::Array(parts)) => {
                    let text: String = parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("");
                    if text.is_empty() {
                        None
                    } else {
                        Some(text)
                    }
                }
                _ => None,
            };
            if let Some(content) = content_str {
                let content_chunk = serde_json::json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{"index": index, "delta": {"content": content}, "finish_reason": null}]
                });
                if let Ok(json) = serde_json::to_string(&content_chunk) {
                    events.push(Ok(bytes::Bytes::from(format!("data: {json}\n\n"))));
                }
            }

            // Chunk 2b: tool_calls delta (when the model issued function calls)
            if let Some(tool_calls) = &choice.message.tool_calls {
                let tc_deltas: Vec<serde_json::Value> = tool_calls
                    .iter()
                    .enumerate()
                    .map(|(i, tc)| {
                        serde_json::json!({
                            "index": i,
                            "id": tc.id,
                            "type": tc.tool_type,
                            "function": {
                                "name": tc.function.name,
                                "arguments": tc.function.arguments,
                            }
                        })
                    })
                    .collect();
                let tc_chunk = serde_json::json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{"index": index, "delta": {"tool_calls": tc_deltas}, "finish_reason": null}]
                });
                if let Ok(json) = serde_json::to_string(&tc_chunk) {
                    events.push(Ok(bytes::Bytes::from(format!("data: {json}\n\n"))));
                }
            }

            // Chunk 3: finish_reason + usage
            let mut finish_chunk = serde_json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{"index": index, "delta": {}, "finish_reason": choice.finish_reason}]
            });
            if let Some(usage) = &response.usage {
                if let Ok(usage_val) = serde_json::to_value(usage) {
                    finish_chunk["usage"] = usage_val;
                }
            }
            if let Ok(json) = serde_json::to_string(&finish_chunk) {
                events.push(Ok(bytes::Bytes::from(format!("data: {json}\n\n"))));
            }
        }

        events.push(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    /// Get the name/identifier of this backend
    fn name(&self) -> &str;

    /// Resolve a request `model` to the upstream connection that will serve it,
    /// for usage attribution and span tagging. The default impl (leaf adapters)
    /// reports the adapter itself with the model unchanged; `RouterBackend`
    /// overrides this to report the resolved stored connection and the
    /// prefix-stripped / route-mapped upstream model id.
    async fn resolve_route(&self, model: &str) -> Result<RouteResolution, BackendError> {
        Ok(RouteResolution {
            provider_kind: self.name().to_string(),
            connection_id: None,
            connection_name: self.name().to_string(),
            upstream_model_id: model.to_string(),
            connection_prefix: None,
            model_route_id: None,
        })
    }

    /// Strict variant of [`resolve_route`](Self::resolve_route) for fallback
    /// alternates: it must resolve to a published route or a recognized provider
    /// prefix, never silently falling through to the OpenAI catch-all. The
    /// default impl matches `resolve_route` (leaf adapters have no catch-all).
    async fn resolve_route_strict(&self, model: &str) -> Result<RouteResolution, BackendError> {
        self.resolve_route(model).await
    }

    /// Resolve a request `model` into a [`ResolvedRoute`] (leaf backend +
    /// stripped model + [`RouteResolution`]) in a single snapshot read, so a
    /// caller resolves the route exactly once and dispatches to
    /// [`ResolvedRoute::backend`] directly. Lenient: an unrecognized model falls
    /// through to the router's catch-all.
    ///
    /// Only [`crate::backend::RouterBackend`] can resolve a stored connection
    /// into a live sub-backend, so the default impl (leaf adapters) errors —
    /// leaf backends are already the terminal dispatch target and are never
    /// asked to resolve.
    async fn resolve(&self, _model: &str) -> Result<ResolvedRoute, BackendError> {
        Err(BackendError::Other("resolve not supported".to_string()))
    }

    /// Strict variant of [`resolve`](Self::resolve) for fallback alternates: an
    /// unrecognized model is an error rather than falling through to the
    /// catch-all. Default impl errors (see [`resolve`](Self::resolve)).
    async fn resolve_strict(&self, _model: &str) -> Result<ResolvedRoute, BackendError> {
        Err(BackendError::Other(
            "resolve_strict not supported".to_string(),
        ))
    }

    /// Resolve a concrete destination without entering another route's policy.
    async fn resolve_concrete(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        crate::route_grid::split_target(model).map_err(BackendError::ConfigError)?;
        self.resolve_strict(model).await
    }

    /// The public model id a caller would use to address a concrete target
    /// directly, or `None` when its connection is reachable only through a
    /// `chalk/*` route. Caller-chosen fallbacks are authorized and rate-limited
    /// under this id, exactly as if the caller had requested it.
    async fn direct_model_id(&self, concrete: &str) -> Option<String> {
        Some(concrete.to_string())
    }

    /// Canonicalize connection names in request fallback keys and alternatives.
    /// Stored route configuration continues to use connection IDs exclusively.
    async fn normalize_request_fallbacks(
        &self,
        fallbacks: &crate::route_grid::FallbackMap,
    ) -> Result<crate::route_grid::FallbackMap, BackendError> {
        Ok(fallbacks.clone())
    }

    /// Resolve the stored connection named by a verified response locator.
    async fn resolve_connection(&self, connection_id: &str) -> Result<ResolvedRoute, BackendError> {
        Err(BackendError::api_status(
            400,
            format!("backend cannot resolve response connection '{connection_id}'"),
        ))
    }

    /// Resolve a model for a provider-native Anthropic request. Qualified model
    /// ids and published routes use normal strict resolution; bare Anthropic ids
    /// retain the native endpoint's implicit-connection compatibility behavior.
    async fn resolve_anthropic(&self, _model: &str) -> Result<ResolvedRoute, BackendError> {
        Err(BackendError::Other(
            "resolve_anthropic not supported".to_string(),
        ))
    }

    /// Resolve the built-in default connection used when a request carries no
    /// model — currently the OpenAI catch-all — into a [`ResolvedRoute`]. Lets a
    /// no-model request (e.g. `/v1/images/generations` without a `model`) be
    /// authorized against the *real* resolved connection identity and dispatched
    /// to its leaf backend, rather than a fabricated `("openai", "openai")`
    /// identity that would misjudge a key scoped to the connection's actual name.
    /// Default impl errors (see [`resolve`](Self::resolve)).
    async fn resolve_default(&self) -> Result<ResolvedRoute, BackendError> {
        Err(BackendError::Other(
            "resolve_default not supported".to_string(),
        ))
    }

    /// Select the implicit stored connection for `provider_kind` from the same
    /// cached, stale-while-refresh resolution snapshot routing uses. Explicit
    /// connections are ineligible; the canonical prefix owner wins, otherwise a
    /// sole eligible connection is selected and multiple eligible candidates are
    /// an error. Returns `Ok(None)` when none is eligible.
    async fn provider_connection(
        &self,
        _provider_kind: &str,
    ) -> Result<Option<StoredConnection>, BackendError> {
        Err(BackendError::Other(
            "provider_connection not supported".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderMap;

    #[test]
    fn forwards_only_rate_limit_and_retry_headers() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", "12".parse().unwrap());
        h.insert("x-ratelimit-remaining-requests", "0".parse().unwrap());
        h.insert("anthropic-ratelimit-tokens-reset", "2024".parse().unwrap());
        h.insert("x-request-id", "req_123".parse().unwrap());
        h.insert("x-amzn-requestid", "aws_123".parse().unwrap());
        h.insert("content-type", "application/json".parse().unwrap());
        h.insert("set-cookie", "secret=1".parse().unwrap());

        let out = forwardable_upstream_headers(&h);
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();

        assert!(names.contains(&"retry-after"));
        assert!(names.contains(&"x-ratelimit-remaining-requests"));
        assert!(names.contains(&"anthropic-ratelimit-tokens-reset"));
        assert!(names.contains(&"x-request-id"));
        assert!(names.contains(&"x-amzn-requestid"));
        // Headers unrelated to rate limiting must not be forwarded.
        assert!(!names.contains(&"content-type"));
        assert!(!names.contains(&"set-cookie"));
    }

    #[test]
    fn native_anthropic_headers_exclude_caller_auth_and_unrelated_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        headers.append("anthropic-beta", "feature-one".parse().unwrap());
        headers.append("anthropic-beta", "feature-two".parse().unwrap());
        headers.insert("authorization", "Bearer caller-secret".parse().unwrap());
        headers.insert("x-api-key", "caller-secret".parse().unwrap());

        let forwarded = AnthropicRequestHeaders::from_header_map(&headers);
        let values = forwarded
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_str().unwrap()))
            .collect::<Vec<_>>();
        assert!(values.contains(&("anthropic-version", "2023-06-01")));
        assert!(values.contains(&("anthropic-beta", "feature-one")));
        assert!(values.contains(&("anthropic-beta", "feature-two")));
        assert!(!values.iter().any(|(name, _)| *name == "authorization"));
        assert!(!values.iter().any(|(name, _)| *name == "x-api-key"));
    }

    #[test]
    fn responses_request_headers_are_strictly_allowlisted() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer caller-secret".parse().unwrap());
        headers.insert("cookie", "session=secret".parse().unwrap());
        headers.insert("x-client-request-id", "client_123".parse().unwrap());
        headers.insert("x-codex-turn-state", "opaque".parse().unwrap());
        headers.insert("openai-project", "proj_123".parse().unwrap());
        headers.insert("openai-organization", "org_123".parse().unwrap());

        let sanitized = ResponsesRequestHeaders::from_header_map(&headers);
        let values: Vec<_> = sanitized.iter().collect();

        assert!(values.contains(&("x-client-request-id", "client_123")));
        assert!(values.contains(&("x-codex-turn-state", "opaque")));
        assert!(!values.iter().any(|(name, _)| *name == "authorization"));
        assert!(!values.iter().any(|(name, _)| *name == "cookie"));
        assert!(!values.iter().any(|(name, _)| *name == "openai-project"));
        assert!(!values
            .iter()
            .any(|(name, _)| *name == "openai-organization"));
    }
}
