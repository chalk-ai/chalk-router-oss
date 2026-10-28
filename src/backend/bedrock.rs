use super::anthropic::AnthropicBackend;
use super::aws_auth::BedrockSigv4;
use super::openai_compatible::OpenAICompatibleBackend;
use super::provider::{
    forwardable_upstream_headers, AnthropicRequestEndpoint, AnthropicRequestHeaders, BackendError,
    BackendJsonResponse, BackendProvider, BackendSseResponse, ResponsesRequestHeaders,
    SseByteStream,
};
use crate::responses::ResponsesRequest;
use crate::types::{
    ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, CreateEmbeddingRequest,
    CreateEmbeddingResponse, CreateImageRequest, CreateModerationRequest, CreateModerationResponse,
    CreateSpeechRequest, FunctionCall, ImageResponse, ListResponse, Model, ToolCall,
    TranscriptionResponse, Usage,
};
use async_trait::async_trait;
use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use aws_smithy_eventstream::smithy::parse_response_headers;
use aws_smithy_types::event_stream::Message as EventStreamMessage;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;

pub const DEFAULT_BEDROCK_REGION: &str = "us-east-1";

fn region_from_runtime_host(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    let region = url
        .host_str()?
        .strip_prefix("bedrock-runtime.")?
        .strip_suffix(".amazonaws.com")?;
    if region.is_empty()
        || !region
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return None;
    }
    Some(region.to_string())
}

/// Runtime has two distinct wire protocols; a regional hostname alone does
/// not determine the transport. Custom proxy URLs retain their own semantics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeEndpoint {
    Converse { region: String },
    OpenAi { region: String },
}

pub(crate) fn runtime_endpoint(
    base_url: Option<&str>,
) -> Result<Option<RuntimeEndpoint>, BackendError> {
    let Some(base_url) = base_url else {
        return Ok(None);
    };
    let parsed = reqwest::Url::parse(base_url)
        .map_err(|error| BackendError::ConfigError(format!("invalid Bedrock base URL: {error}")))?;
    if parsed.host_str().is_some_and(|host| {
        host.starts_with("bedrock-runtime.") && host.ends_with(".amazonaws.com")
    }) && parsed.scheme() != "https"
    {
        return Err(BackendError::ConfigError(
            "Bedrock Runtime requires HTTPS".into(),
        ));
    }
    let Some(region) = region_from_runtime_host(base_url) else {
        return Ok(None);
    };
    let url = reqwest::Url::parse(base_url)
        .map_err(|error| BackendError::ConfigError(error.to_string()))?;
    if url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(BackendError::ConfigError(
            "Bedrock Runtime base URL must not contain credentials, a query or fragment"
                .to_string(),
        ));
    }
    match url.path().trim_end_matches('/') {
        "" => Ok(Some(RuntimeEndpoint::Converse { region })),
        "/openai/v1" => Ok(Some(RuntimeEndpoint::OpenAi { region })),
        _ => Err(BackendError::ConfigError(
            "Bedrock Runtime base URL must be the regional root or /openai/v1".to_string(),
        )),
    }
}

pub fn is_runtime_connection(base_url: Option<&str>) -> bool {
    matches!(runtime_endpoint(base_url), Ok(Some(_)))
}

pub(crate) fn is_runtime_openai_connection(base_url: Option<&str>) -> bool {
    matches!(
        runtime_endpoint(base_url),
        Ok(Some(RuntimeEndpoint::OpenAi { .. }))
    )
}

/// Resolve the OpenAI-compatible Bedrock Mantle endpoint.
///
/// A custom base URL takes precedence over the regional endpoint. Trailing
/// slashes are removed because [`OpenAICompatibleBackend`] appends endpoint paths with a
/// leading slash.
pub fn resolve_bedrock_base_url(region: Option<&str>, base_url_override: Option<&str>) -> String {
    if let Some(base_url) = base_url_override
        .map(str::trim)
        .filter(|base_url| !base_url.is_empty())
    {
        return base_url.trim_end_matches('/').to_string();
    }

    let region = region
        .map(str::trim)
        .filter(|region| !region.is_empty())
        .unwrap_or(DEFAULT_BEDROCK_REGION);

    format!("https://bedrock-mantle.{region}.api.aws/v1")
}

/// Bedrock Mantle exposes Anthropic models through its Anthropic Messages
/// endpoint, rather than its OpenAI-compatible Chat Completions endpoint.
///
/// Bedrock inference profiles prefix the provider with a region (for example
/// `us.anthropic.claude-sonnet-4-5`), while direct model IDs begin with
/// `anthropic.`. Both forms identify Anthropic-family models.
pub(crate) fn is_bedrock_anthropic_model(model: &str) -> bool {
    model.split('.').any(|segment| segment == "anthropic")
}

/// Closed GPT models use Mantle's `/openai/v1` generation dialect. GPT OSS
/// and other providers keep the original compatibility endpoint. Keep this
/// list scoped to documented models, rather than assuming every OpenAI ID
/// uses the same transport. Mantle also lists dated snapshots of these models
/// (for example `openai.gpt-5.5-2026-04-23`); they share the base model's
/// endpoint.
///
/// See the endpoint tables in the AWS GPT-5.6 and GPT-6 model cards:
/// https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-openai-gpt-56-sol.html
/// https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-openai-gpt-6-luna.html
pub(crate) fn is_mantle_closed_gpt_model(model: &str) -> bool {
    matches!(
        strip_snapshot_date(model),
        "openai.gpt-5.4"
            | "openai.gpt-5.5"
            | "openai.gpt-5.6-sol"
            | "openai.gpt-5.6-terra"
            | "openai.gpt-5.6-luna"
            | "openai.gpt-6-sol"
            | "openai.gpt-6-luna"
            | "openai.gpt-6-astra"
            | "openai.gpt-6.1-sol"
    )
}

/// Models whose generation Mantle serves only on `/openai/v1`. Its `/v1`
/// endpoint answers these with `400 model ... isn't supported on this route`
/// (and its `/v1/responses` does not answer at all), while `/v1/models`
/// lists them alongside everything else and carries no endpoint hint.
/// Observed against bedrock-mantle.us-east-1 on 2026-10-13: closed GPT
/// (including dated snapshots), Gemma 4 and Grok.
fn uses_mantle_openai_dialect(model: &str) -> bool {
    is_mantle_closed_gpt_model(model)
        || model.starts_with("google.gemma-4-")
        || model.starts_with("xai.grok-")
}

/// `openai.gpt-5.5-2026-04-23` -> `openai.gpt-5.5`; other ids are unchanged.
///
/// Model ids come from request bodies and may be any UTF-8, so the split
/// point is checked for a char boundary before slicing.
fn strip_snapshot_date(model: &str) -> &str {
    let Some(head_len) = model.len().checked_sub(11).filter(|&len| len > 0) else {
        return model;
    };
    if !model.is_char_boundary(head_len) {
        return model;
    }
    let (head, tail) = model.split_at(head_len);
    let t = tail.as_bytes();
    let dated = t[0] == b'-'
        && t[5] == b'-'
        && t[8] == b'-'
        && [1, 2, 3, 4, 6, 7, 9, 10]
            .iter()
            .all(|&i| t[i].is_ascii_digit());
    if dated {
        head
    } else {
        model
    }
}

/// Derive the documented dialect only for the standard AWS Mantle endpoint.
/// Explicit `/openai/v1` and custom proxy paths remain authoritative.
fn mantle_closed_gpt_base_url(base_url: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(base_url).ok()?;
    let region = url
        .host_str()?
        .strip_prefix("bedrock-mantle.")?
        .strip_suffix(".api.aws")?;
    if url.scheme() != "https"
        || region.is_empty()
        || !region
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        || url.path() != "/v1"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    url.set_path("/openai/v1");
    Some(url.to_string())
}

/// Create an AWS Bedrock Mantle backend. Anthropic-family models are sent to
/// Mantle's Anthropic Messages endpoint and translated to/from OpenAI Chat
/// Completions. Documented closed GPT models use `/openai/v1`; other models
/// retain the original `/v1` OpenAI-compatible transport.
///
/// OpenAI-compatible calls use bearer auth; the Mantle Anthropic surface uses
/// the documented `x-api-key` header.
pub fn bedrock_backend(
    http_client: reqwest::Client,
    api_key: String,
    region: Option<&str>,
    base_url_override: Option<&str>,
) -> BedrockMantleBackend {
    let openai_base_url = resolve_bedrock_base_url(region, base_url_override);
    let closed_gpt_base_url = mantle_closed_gpt_base_url(&openai_base_url);
    mantle_backend_with_urls(http_client, api_key, openai_base_url, closed_gpt_base_url)
}

fn mantle_backend_with_urls(
    http_client: reqwest::Client,
    api_key: String,
    openai_base_url: String,
    closed_gpt_base_url: Option<String>,
) -> BedrockMantleBackend {
    let anthropic_base_url = resolve_bedrock_anthropic_base_url(&openai_base_url);
    BedrockMantleBackend {
        closed_gpt: closed_gpt_base_url.map(|base_url| {
            OpenAICompatibleBackend::for_provider(
                http_client.clone(),
                "bedrock",
                api_key.clone(),
                base_url,
            )
        }),
        openai: OpenAICompatibleBackend::for_provider(
            http_client.clone(),
            "bedrock",
            api_key.clone(),
            openai_base_url,
        )
        .with_anthropic_x_api_key_base_url(anthropic_base_url.clone()),
        anthropic: AnthropicBackend::with_base_url(http_client, api_key, anthropic_base_url),
    }
}

pub fn resolve_bedrock_anthropic_base_url(openai_base_url: &str) -> String {
    openai_base_url
        .strip_suffix("/v1")
        .map(|root| format!("{root}/anthropic/v1"))
        .unwrap_or_else(|| openai_base_url.to_string())
}

/// Mantle selects a generation dialect by model. Anthropic-family models
/// use its Messages API even when the caller uses Chat Completions, and
/// documented closed GPT models use a separate OpenAI-compatible endpoint.
pub struct BedrockMantleBackend {
    openai: OpenAICompatibleBackend,
    closed_gpt: Option<OpenAICompatibleBackend>,
    anthropic: AnthropicBackend,
}

impl BedrockMantleBackend {
    fn openai_for_model(&self, model: &str) -> &OpenAICompatibleBackend {
        if uses_mantle_openai_dialect(model) {
            if let Some(backend) = &self.closed_gpt {
                return backend;
            }
        }
        &self.openai
    }
}

#[async_trait]
impl BackendProvider for BedrockMantleBackend {
    fn check_native_chat(
        &self,
        request: &crate::native_chat::NativeChatRequest,
        upstream_model: &str,
    ) -> Result<(), BackendError> {
        if is_bedrock_anthropic_model(upstream_model) {
            self.anthropic.check_native_chat(request, upstream_model)
        } else {
            self.openai_for_model(upstream_model)
                .check_native_chat(request, upstream_model)
        }
    }

    async fn create_native_embedding(
        &self,
        request: crate::native_json::NativeJsonRequest<CreateEmbeddingRequest>,
    ) -> Result<super::provider::BackendJsonView<CreateEmbeddingResponse>, BackendError> {
        self.openai.create_native_embedding(request).await
    }
    async fn create_native_image(
        &self,
        request: crate::native_json::NativeJsonRequest<CreateImageRequest>,
    ) -> Result<super::provider::BackendJsonView<ImageResponse>, BackendError> {
        self.openai.create_native_image(request).await
    }
    async fn create_native_speech(
        &self,
        request: crate::native_json::NativeJsonRequest<CreateSpeechRequest>,
    ) -> Result<super::provider::BackendBodyResponse, BackendError> {
        self.openai.create_native_speech(request).await
    }
    async fn create_native_audio(
        &self,
        request: crate::native_audio::NativeAudioRequest,
    ) -> Result<super::provider::BackendBodyResponse, BackendError> {
        self.openai.create_native_audio(request).await
    }

    fn supports_native_chat(&self) -> bool {
        true
    }

    async fn create_native_chat_completion(
        &self,
        request: crate::native_chat::NativeChatRequest,
    ) -> Result<super::provider::BackendChatResponse, BackendError> {
        if is_bedrock_anthropic_model(&request.model) {
            let completion = self
                .anthropic
                .create_chat_completion(request.compatibility_request()?)
                .await?;
            return super::provider::BackendChatResponse::from_completion(completion);
        }
        self.openai_for_model(&request.model)
            .create_native_chat_completion(request)
            .await
    }

    async fn create_native_chat_completion_stream(
        &self,
        request: crate::native_chat::NativeChatRequest,
    ) -> Result<BackendSseResponse, BackendError> {
        if is_bedrock_anthropic_model(&request.model) {
            return Ok(BackendSseResponse {
                stream: self
                    .anthropic
                    .create_chat_completion_stream(request.compatibility_request()?)
                    .await?,
                headers: Vec::new(),
                status: 200,
                content_type: "text/event-stream".into(),
            });
        }
        self.openai_for_model(&request.model)
            .create_native_chat_completion_stream(request)
            .await
    }

    async fn create_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        if is_bedrock_anthropic_model(&request.model) {
            self.anthropic.create_chat_completion(request).await
        } else {
            self.openai_for_model(&request.model)
                .create_chat_completion(request)
                .await
        }
    }

    async fn create_response(
        &self,
        request: ResponsesRequest,
        headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        // Only closed GPT models advertise native Responses (see
        // `capabilities::bedrock_capabilities`); they live on `/openai/v1`.
        self.openai_for_model(request.model())
            .create_response(request, headers)
            .await
    }

    async fn create_response_stream(
        &self,
        request: ResponsesRequest,
        headers: &ResponsesRequestHeaders,
    ) -> Result<BackendSseResponse, BackendError> {
        self.openai_for_model(request.model())
            .create_response_stream(request, headers)
            .await
    }

    async fn compact_response(
        &self,
        request: crate::schema_json::SchemaJson,
        headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        self.openai.compact_response(request, headers).await
    }

    async fn retrieve_response(
        &self,
        response_id: &str,
        headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        self.openai.retrieve_response(response_id, headers).await
    }

    async fn retrieve_response_stream(
        &self,
        response_id: &str,
        starting_after: Option<u64>,
        headers: &ResponsesRequestHeaders,
    ) -> Result<BackendSseResponse, BackendError> {
        self.openai
            .retrieve_response_stream(response_id, starting_after, headers)
            .await
    }

    async fn cancel_response(
        &self,
        response_id: &str,
        headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        self.openai.cancel_response(response_id, headers).await
    }

    async fn delete_response(
        &self,
        response_id: &str,
        headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        self.openai.delete_response(response_id, headers).await
    }

    async fn send_anthropic_request(
        &self,
        endpoint: AnthropicRequestEndpoint,
        headers: &AnthropicRequestHeaders,
        body: Bytes,
    ) -> Result<reqwest::Response, BackendError> {
        self.openai
            .send_anthropic_request(endpoint, headers, body)
            .await
    }

    async fn create_chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<SseByteStream, BackendError> {
        if is_bedrock_anthropic_model(&request.model) {
            self.anthropic.create_chat_completion_stream(request).await
        } else {
            self.openai_for_model(&request.model)
                .create_chat_completion_stream(request)
                .await
        }
    }

    async fn create_embedding(
        &self,
        request: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        self.openai.create_embedding(request).await
    }

    async fn create_image(
        &self,
        request: CreateImageRequest,
    ) -> Result<ImageResponse, BackendError> {
        self.openai.create_image(request).await
    }

    async fn create_speech(&self, request: CreateSpeechRequest) -> Result<Bytes, BackendError> {
        self.openai.create_speech(request).await
    }

    async fn create_transcription(
        &self,
        model: String,
        file_bytes: Bytes,
        file_name: String,
        language: Option<String>,
        prompt: Option<String>,
        response_format: Option<String>,
        temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        self.openai
            .create_transcription(
                model,
                file_bytes,
                file_name,
                language,
                prompt,
                response_format,
                temperature,
            )
            .await
    }

    async fn create_translation(
        &self,
        model: String,
        file_bytes: Bytes,
        file_name: String,
        prompt: Option<String>,
        response_format: Option<String>,
        temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        self.openai
            .create_translation(
                model,
                file_bytes,
                file_name,
                prompt,
                response_format,
                temperature,
            )
            .await
    }

    async fn create_moderation(
        &self,
        request: CreateModerationRequest,
    ) -> Result<CreateModerationResponse, BackendError> {
        self.openai.create_moderation(request).await
    }

    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        self.openai.list_models().await
    }

    async fn get_model(&self, model_id: &str) -> Result<Model, BackendError> {
        self.openai.get_model(model_id).await
    }

    fn name(&self) -> &str {
        "bedrock"
    }
}

pub struct BedrockRuntimeBackend {
    client: reqwest::Client,
    base_url: String,
    auth: RuntimeAuth,
    openai: Option<OpenAICompatibleBackend>,
}

enum RuntimeAuth {
    Bearer(String),
    Sigv4(Arc<BedrockSigv4>),
}

#[derive(Debug, Default)]
struct BedrockEventDecoder {
    decoder: MessageFrameDecoder,
    buffer: BytesMut,
    incomplete_frame: bool,
}

impl BedrockEventDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<EventStreamMessage>, BackendError> {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        self.buffer.extend_from_slice(bytes);
        let mut messages = Vec::new();
        loop {
            match self
                .decoder
                .decode_frame(&mut self.buffer)
                .map_err(|error| {
                    BackendError::Other(format!(
                        "invalid Bedrock ConverseStream event frame: {error}"
                    ))
                })? {
                DecodedFrame::Complete(message) => {
                    self.incomplete_frame = false;
                    messages.push(message);
                    if self.buffer.is_empty() {
                        break;
                    }
                }
                DecodedFrame::Incomplete => {
                    self.incomplete_frame = true;
                    break;
                }
            }
        }
        Ok(messages)
    }

    fn finish(&self) -> Result<(), BackendError> {
        if self.incomplete_frame {
            Err(BackendError::Other(
                "Bedrock ConverseStream ended with an incomplete event frame".to_string(),
            ))
        } else {
            Ok(())
        }
    }
}

struct ConverseStreamTranslator {
    id: String,
    model: String,
    created: i64,
    saw_message_stop: bool,
    tool_indexes: BTreeMap<u64, u64>,
}

impl ConverseStreamTranslator {
    fn new(model: String) -> Self {
        Self {
            id: format!("chatcmpl-bedrock-{}", uuid::Uuid::new_v4()),
            model,
            created: chrono::Utc::now().timestamp(),
            saw_message_stop: false,
            tool_indexes: BTreeMap::new(),
        }
    }

    fn sse_chunk(&self, choices: Value, usage: Option<Value>) -> Result<Bytes, BackendError> {
        let mut chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": choices,
        });
        if let Some(usage) = usage {
            chunk["usage"] = usage;
        }
        let encoded = serde_json::to_string(&chunk).map_err(|error| {
            BackendError::Other(format!("failed to serialize Bedrock SSE chunk: {error}"))
        })?;
        Ok(Bytes::from(format!("data: {encoded}\n\n")))
    }

    fn translate(&mut self, message: EventStreamMessage) -> Result<Option<Bytes>, BackendError> {
        let headers = parse_response_headers(&message).map_err(|error| {
            BackendError::Other(format!(
                "invalid Bedrock ConverseStream event headers: {error}"
            ))
        })?;
        let message_type = headers.message_type.as_str();
        let event_type = headers.smithy_type.as_str();
        let payload: Value = serde_json::from_slice(message.payload()).map_err(|error| {
            BackendError::Other(format!(
                "invalid Bedrock ConverseStream {event_type} payload: {error}"
            ))
        })?;

        if message_type == "exception" {
            return Err(converse_stream_exception(event_type, &payload));
        }

        match event_type {
            "messageStart" => {
                let role = payload
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("assistant");
                self.sse_chunk(
                    json!([{
                        "index": 0,
                        "delta": {"role": role, "content": ""},
                        "finish_reason": null,
                    }]),
                    None,
                )
                .map(Some)
            }
            "contentBlockStart" => {
                let Some(tool) = payload.pointer("/start/toolUse") else {
                    return Ok(None);
                };
                let content_block_index = payload
                    .get("contentBlockIndex")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        BackendError::Other(
                            "Bedrock tool-use start is missing contentBlockIndex".to_string(),
                        )
                    })?;
                if self.tool_indexes.contains_key(&content_block_index) {
                    return Err(BackendError::Other(format!(
                        "Bedrock tool-use block {content_block_index} started more than once"
                    )));
                }
                let tool_index = self.tool_indexes.len() as u64;
                self.tool_indexes.insert(content_block_index, tool_index);
                let id = tool
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        BackendError::Other(
                            "Bedrock tool-use start is missing toolUseId".to_string(),
                        )
                    })?;
                let name = tool.get("name").and_then(Value::as_str).ok_or_else(|| {
                    BackendError::Other("Bedrock tool-use start is missing name".to_string())
                })?;
                self.sse_chunk(
                    json!([{
                        "index": 0,
                        "delta": {"tool_calls": [{
                            "index": tool_index,
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": ""},
                        }]},
                        "finish_reason": null,
                    }]),
                    None,
                )
                .map(Some)
            }
            "contentBlockDelta" => {
                let delta = payload.get("delta").ok_or_else(|| {
                    BackendError::Other("Bedrock content-block delta is missing delta".to_string())
                })?;
                let openai_delta = if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    json!({"content": text})
                } else if let Some(input) = delta.pointer("/toolUse/input").and_then(Value::as_str)
                {
                    let content_block_index = payload
                        .get("contentBlockIndex")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| {
                            BackendError::Other(
                                "Bedrock tool-use delta is missing contentBlockIndex".to_string(),
                            )
                        })?;
                    let tool_index = self
                        .tool_indexes
                        .get(&content_block_index)
                        .copied()
                        .ok_or_else(|| {
                            BackendError::Other(format!(
                                "Bedrock tool-use delta arrived before block {content_block_index} started"
                            ))
                        })?;
                    json!({"tool_calls": [{
                        "index": tool_index,
                        "function": {"arguments": input},
                    }]})
                } else if let Some(reasoning) = delta
                    .pointer("/reasoningContent/text")
                    .and_then(Value::as_str)
                {
                    json!({"reasoning_content": reasoning})
                } else {
                    return Err(BackendError::Other(format!(
                        "unsupported Bedrock ConverseStream content delta: {delta}"
                    )));
                };
                self.sse_chunk(
                    json!([{
                        "index": 0,
                        "delta": openai_delta,
                        "finish_reason": null,
                    }]),
                    None,
                )
                .map(Some)
            }
            "contentBlockStop" => Ok(None),
            "messageStop" => {
                self.saw_message_stop = true;
                let reason = payload
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .map(openai_finish_reason)
                    .unwrap_or("stop");
                self.sse_chunk(
                    json!([{
                        "index": 0,
                        "delta": {},
                        "finish_reason": reason,
                    }]),
                    None,
                )
                .map(Some)
            }
            "metadata" => {
                let usage = payload.get("usage").map(|usage| {
                    let prompt_tokens = usage
                        .get("inputTokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    let completion_tokens = usage
                        .get("outputTokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    let total_tokens = usage
                        .get("totalTokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(prompt_tokens + completion_tokens);
                    let cached_tokens = usage
                        .get("cacheReadInputTokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    let cache_write_tokens = usage
                        .get("cacheWriteInputTokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    json!({
                        "prompt_tokens": prompt_tokens,
                        "completion_tokens": completion_tokens,
                        "total_tokens": total_tokens,
                        "prompt_tokens_details": {
                            "cached_tokens": cached_tokens,
                            "cache_write_tokens": cache_write_tokens,
                        },
                    })
                });
                match usage {
                    Some(usage) => self.sse_chunk(json!([]), Some(usage)).map(Some),
                    None => Ok(None),
                }
            }
            other => Err(BackendError::Other(format!(
                "unsupported Bedrock ConverseStream event '{other}'"
            ))),
        }
    }
}

fn openai_finish_reason(reason: &str) -> &'static str {
    match reason {
        "max_tokens" | "model_context_window_exceeded" => "length",
        "tool_use" => "tool_calls",
        "guardrail_intervened" | "content_filtered" => "content_filter",
        _ => "stop",
    }
}

/// Bedrock error bodies carry the client-visible message under `message`
/// (most operations) or `Message` (a few, notably Smithy exception frames).
/// Shared by the top-level HTTP error path and mid-stream exception frames.
fn bedrock_error_message(payload: &Value) -> Option<String> {
    payload
        .get("message")
        .or_else(|| payload.get("Message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn converse_stream_exception(kind: &str, payload: &Value) -> BackendError {
    let status = match kind {
        "validationException" => 400,
        "throttlingException" => 429,
        "modelStreamErrorException" => 424,
        "serviceUnavailableException" => 503,
        _ => 500,
    };
    let message = bedrock_error_message(payload).unwrap_or_else(|| payload.to_string());
    BackendError::api_status(status, format!("Bedrock {kind}: {message}"))
}

impl BedrockRuntimeBackend {
    pub fn new(client: reqwest::Client, base_url: Option<&str>) -> Result<Self, BackendError> {
        Self::with_api_key(
            client,
            crate::provider_connections::KEYLESS_PLACEHOLDER,
            base_url,
        )
    }

    /// Use the stored connection credential as a bearer token. Empty keys and
    /// the repository's keyless placeholder explicitly select SigV4 instead.
    /// A rejected bearer credential never falls back to the AWS chain.
    pub fn with_api_key(
        client: reqwest::Client,
        api_key: &str,
        base_url: Option<&str>,
    ) -> Result<Self, BackendError> {
        let endpoint = runtime_endpoint(base_url)?.ok_or_else(|| {
            BackendError::ConfigError(
                "Bedrock Runtime requires a bedrock-runtime regional base URL".to_string(),
            )
        })?;
        let region = match &endpoint {
            RuntimeEndpoint::Converse { region } | RuntimeEndpoint::OpenAi { region } => {
                region.clone()
            }
        };
        let base_url = base_url.unwrap().trim_end_matches('/').to_string();
        let keyless =
            api_key.is_empty() || api_key == crate::provider_connections::KEYLESS_PLACEHOLDER;
        let auth = if keyless {
            RuntimeAuth::Sigv4(Arc::new(BedrockSigv4::new(client.clone(), region.clone())))
        } else {
            RuntimeAuth::Bearer(api_key.to_string())
        };
        let openai = if matches!(endpoint, RuntimeEndpoint::OpenAi { .. }) {
            let mut backend = OpenAICompatibleBackend::for_provider(
                client.clone(),
                "bedrock",
                api_key.to_string(),
                base_url.clone(),
            );
            if let RuntimeAuth::Sigv4(signer) = &auth {
                backend = backend.with_bedrock_sigv4(signer.clone());
            }
            Some(backend)
        } else {
            None
        };
        Ok(Self {
            client,
            base_url,
            auth,
            openai,
        })
    }

    fn model_url(&self, model: &str, operation: &str) -> Result<reqwest::Url, BackendError> {
        let mut url = reqwest::Url::parse(&self.base_url)
            .map_err(|error| BackendError::ConfigError(format!("invalid Bedrock URL: {error}")))?;
        url.path_segments_mut()
            .map_err(|_| BackendError::ConfigError("Bedrock URL cannot be a base".to_string()))?
            .extend(["model", model, operation]);
        Ok(url)
    }

    async fn post_with_accept(
        &self,
        url: reqwest::Url,
        body: &(impl serde::Serialize + Sync),
        accept: Option<&str>,
    ) -> Result<reqwest::Response, BackendError> {
        let mut builder = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .json(body);
        if let Some(accept) = accept {
            builder = builder.header("accept", accept);
        }
        let mut request = builder.build()?;
        match &self.auth {
            RuntimeAuth::Bearer(key) => {
                let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
                    .map_err(|_| {
                        BackendError::ConfigError("invalid Bedrock bearer credential".to_string())
                    })?;
                value.set_sensitive(true);
                request
                    .headers_mut()
                    .insert(reqwest::header::AUTHORIZATION, value);
            }
            RuntimeAuth::Sigv4(signer) => signer.sign(&mut request).await?,
        }
        Ok(self.client.execute(request).await?)
    }

    async fn post(
        &self,
        url: reqwest::Url,
        body: &(impl serde::Serialize + Sync),
    ) -> Result<reqwest::Response, BackendError> {
        self.post_with_accept(url, body, None).await
    }

    async fn converse_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<SseByteStream, BackendError> {
        Ok(self.converse_stream_response(request).await?.stream)
    }

    async fn converse_stream_response(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<BackendSseResponse, BackendError> {
        let model = request.model.clone();
        let response = self
            .post_with_accept(
                self.model_url(&model, "converse-stream")?,
                &Self::converse_body(&request)?,
                Some("application/vnd.amazon.eventstream"),
            )
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(super::provider::upstream_error(response, |text| {
                serde_json::from_str::<Value>(text)
                    .ok()
                    .and_then(|value| bedrock_error_message(&value))
            })
            .await);
        }

        if !response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .eq_ignore_ascii_case("application/vnd.amazon.eventstream")
            })
        {
            return Err(BackendError::api_status(
                502,
                "Bedrock ConverseStream returned an unexpected content type",
            ));
        }

        let headers = forwardable_upstream_headers(response.headers());
        let mut upstream = response.bytes_stream();
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        tokio::spawn(async move {
            let mut decoder = BedrockEventDecoder::default();
            let mut translator = ConverseStreamTranslator::new(model);
            while let Some(item) = upstream.next().await {
                let bytes = match item {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        let _ = sender.send(Err(BackendError::from(error))).await;
                        return;
                    }
                };
                let messages = match decoder.push(&bytes) {
                    Ok(messages) => messages,
                    Err(error) => {
                        let _ = sender.send(Err(error)).await;
                        return;
                    }
                };
                for message in messages {
                    match translator.translate(message) {
                        Ok(Some(chunk)) => {
                            if sender.send(Ok(chunk)).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let _ = sender.send(Err(error)).await;
                            return;
                        }
                    }
                }
            }

            if let Err(error) = decoder.finish() {
                let _ = sender.send(Err(error)).await;
                return;
            }
            if !translator.saw_message_stop {
                let _ = sender
                    .send(Err(BackendError::Other(
                        "Bedrock ConverseStream ended before messageStop".to_string(),
                    )))
                    .await;
                return;
            }
            let _ = sender
                .send(Ok(Bytes::from_static(b"data: [DONE]\n\n")))
                .await;
        });
        Ok(BackendSseResponse {
            stream: Box::pin(ReceiverStream::new(receiver)),
            headers,
            status: 200,
            content_type: "text/event-stream".into(),
        })
    }

    fn converse_body(request: &ChatCompletionRequest) -> Result<ConverseBody, BackendError> {
        use crate::schema_json::SchemaJson;
        crate::translation::chat_controls::validate_converse_chat_request(request)?;
        let mut system = Vec::new();
        let mut messages = Vec::new();
        for message in &request.messages {
            if matches!(message.role.as_str(), "system" | "developer") {
                if let Some(text) = message.content.as_ref().and_then(Value::as_str) {
                    system.push(json!({"text":text}));
                }
                continue;
            }
            if message.role == "tool" {
                messages.push(json!({"role":"user","content":[{"toolResult":{
                    "toolUseId":message.tool_call_id,
                    "content":[{"text":message.content.as_ref().and_then(Value::as_str).unwrap_or("")}]
                }}]}));
                continue;
            }
            let mut content = Vec::new();
            if let Some(value) = &message.content {
                match value {
                    Value::String(text) => content.push(json!({"text":text})),
                    Value::Array(parts) => {
                        for part in parts {
                            match part.get("type").and_then(Value::as_str) {
                                Some("text") => content.push(json!({"text":part["text"]})),
                                Some("image_url") => {
                                    let url =
                                        part["image_url"]["url"].as_str().ok_or_else(|| {
                                            BackendError::api_status(400, "image_url requires url")
                                        })?;
                                    let encoded = url
                                        .strip_prefix("data:image/")
                                        .and_then(|rest| rest.split_once(";base64,"));
                                    let (format, bytes) = encoded.ok_or_else(|| {
                                        BackendError::api_status(
                                            400,
                                            "Bedrock Converse requires base64 data URL images",
                                        )
                                    })?;
                                    content.push(
                                        json!({"image":{"format":format,"source":{"bytes":bytes}}}),
                                    );
                                }
                                Some(kind) => {
                                    return Err(BackendError::api_status(
                                        400,
                                        format!("unsupported Converse content part '{kind}'"),
                                    ))
                                }
                                None => {
                                    return Err(BackendError::api_status(
                                        400,
                                        "Converse content part requires type",
                                    ))
                                }
                            }
                        }
                    }
                    _ => {
                        return Err(BackendError::api_status(
                            400,
                            "unsupported Chat content for Converse",
                        ))
                    }
                }
            }
            if let Some(calls) = &message.tool_calls {
                for call in calls {
                    let input = crate::translation::chat_controls::tool_call_input(
                        &call.function.arguments,
                    );
                    content.push(json!({"toolUse":{"toolUseId":call.id,"name":call.function.name,"input":input}}));
                }
            }
            messages.push(json!({"role":message.role,"content":content}));
        }
        let inference_config = json!({
            "maxTokens": request.max_completion_tokens.or(request.max_tokens),
            "temperature": request.temperature,
            "topP": request.top_p,
            "stopSequences": request.stop
        });
        let mut tool_config = None;
        if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
            let tools: Vec<_> = tools
                .iter()
                .map(|tool| {
                    SchemaJson::object([(
                        "toolSpec",
                        SchemaJson::object([
                            ("name", json!(tool.function.name).into()),
                            ("description", json!(tool.function.description).into()),
                            (
                                "inputSchema",
                                SchemaJson::object([(
                                    "json",
                                    tool.function
                                        .parameters
                                        .clone()
                                        .unwrap_or_else(|| Value::Null.into()),
                                )]),
                            ),
                        ]),
                    )])
                })
                .collect();
            tool_config = Some(SchemaJson::object([("tools", SchemaJson::array(tools))]));
        }
        Ok(ConverseBody {
            messages,
            system: (!system.is_empty()).then_some(system),
            inference_config: Some(inference_config),
            tool_config,
        })
    }

    fn convert_response(
        model: String,
        value: Value,
    ) -> Result<ChatCompletionResponse, BackendError> {
        let malformed =
            || BackendError::api_status(502, "Bedrock returned an invalid Converse response");
        let stop = value
            .get("stopReason")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(malformed)?;
        let content = match value
            .pointer("/output/message/content")
            .and_then(Value::as_array)
        {
            Some(content)
                if value
                    .pointer("/output/message/role")
                    .and_then(Value::as_str)
                    == Some("assistant") =>
            {
                content.as_slice()
            }
            None if matches!(stop, "guardrail_intervened" | "content_filtered") => &[],
            _ => return Err(malformed()),
        };
        let token_count = |name: &str, required: bool| -> Result<i32, BackendError> {
            match value.get("usage").and_then(|u| u.get(name)) {
                Some(number) => number
                    .as_u64()
                    .and_then(|n| i32::try_from(n).ok())
                    .ok_or_else(malformed),
                None if !required => Ok(0),
                None => Err(malformed()),
            }
        };
        let input = token_count("inputTokens", true)?;
        let output = token_count("outputTokens", true)?;
        let total = input.checked_add(output).ok_or_else(malformed)?;
        let cached = token_count("cacheReadInputTokens", false)?;
        let cache_write = token_count("cacheWriteInputTokens", false)?;
        let mut text = Vec::new();
        let mut calls = Vec::new();
        for block in content {
            if !block.is_object() || block.as_object().is_none_or(|object| object.is_empty()) {
                return Err(malformed());
            }
            if let Some(text_value) = block.get("text") {
                text.push(text_value.as_str().ok_or_else(malformed)?.to_string());
            }
            if let Some(tool) = block.get("toolUse") {
                calls.push(ToolCall {
                    id: tool
                        .get("toolUseId")
                        .and_then(Value::as_str)
                        .filter(|v| !v.is_empty())
                        .ok_or_else(malformed)?
                        .to_string(),
                    tool_type: "function".to_string(),
                    function: FunctionCall {
                        name: tool
                            .get("name")
                            .and_then(Value::as_str)
                            .filter(|v| !v.is_empty())
                            .ok_or_else(malformed)?
                            .to_string(),
                        arguments: tool
                            .get("input")
                            .filter(|v| v.is_object())
                            .ok_or_else(malformed)?
                            .to_string(),
                    },
                });
            }
        }
        let finish = match stop {
            "max_tokens" => "length",
            "tool_use" => "tool_calls",
            "guardrail_intervened" | "content_filtered" => "content_filter",
            _ => "stop",
        };
        Ok(ChatCompletionResponse {
            id: format!("chatcmpl-bedrock-{}", uuid::Uuid::new_v4()),
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp(),
            model,
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: (!text.is_empty()).then(|| Value::String(text.join(""))),
                    name: None,
                    tool_calls: (!calls.is_empty()).then_some(calls),
                    tool_call_id: None,
                },
                finish_reason: Some(finish.to_string()),
            }],
            usage: Some(Usage {
                prompt_tokens: input,
                completion_tokens: Some(output),
                total_tokens: total,
                prompt_tokens_details: Some(crate::types::PromptTokenDetails {
                    cached_tokens: cached,
                    cache_write_tokens: cache_write,
                }),
                completion_tokens_details: None,
                ..Default::default()
            }),
        })
    }

    async fn converse(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        Ok(self.converse_response(request).await?.completion)
    }

    async fn converse_response(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<super::provider::BackendChatResponse, BackendError> {
        let model = request.model.clone();
        let response = self
            .post(
                self.model_url(&model, "converse")?,
                &Self::converse_body(&request)?,
            )
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(super::provider::upstream_error(response, |text| {
                serde_json::from_str::<Value>(text)
                    .ok()
                    .and_then(|value| bedrock_error_message(&value))
            })
            .await);
        }
        let headers = forwardable_upstream_headers(response.headers());
        let body: Value = response
            .json()
            .await
            .map_err(|_| BackendError::api_status(502, "Bedrock returned invalid Converse JSON"))?;
        let mut reply = super::provider::BackendChatResponse::from_completion(
            Self::convert_response(model, body)?,
        )?;
        reply.headers = headers;
        Ok(reply)
    }
}

fn unsupported(operation: &str) -> BackendError {
    BackendError::api_status(
        400,
        format!("Bedrock Runtime does not support {operation} through this adapter"),
    )
}

#[async_trait]
impl BackendProvider for BedrockRuntimeBackend {
    fn supports_native_chat(&self) -> bool {
        self.openai.is_some()
    }

    fn check_translated_chat(&self, request: &ChatCompletionRequest) -> Result<(), BackendError> {
        crate::translation::chat_controls::validate_converse_chat_request(request)
    }

    async fn create_native_chat_completion(
        &self,
        request: crate::native_chat::NativeChatRequest,
    ) -> Result<super::provider::BackendChatResponse, BackendError> {
        if let Some(openai) = &self.openai {
            return openai.create_native_chat_completion(request).await;
        }
        self.converse_response(request.compatibility_request()?)
            .await
    }

    async fn create_native_chat_completion_stream(
        &self,
        request: crate::native_chat::NativeChatRequest,
    ) -> Result<BackendSseResponse, BackendError> {
        if let Some(openai) = &self.openai {
            return openai.create_native_chat_completion_stream(request).await;
        }
        self.converse_stream_response(request.compatibility_request()?)
            .await
    }

    async fn count_anthropic_tokens(
        &self,
        body: Bytes,
    ) -> Result<BackendJsonResponse, BackendError> {
        if self.openai.is_some() {
            return Err(unsupported("count_tokens on the OpenAI endpoint"));
        }
        let mut raw: crate::schema_json::SchemaJson =
            serde_json::from_slice(&body).map_err(|error| {
                BackendError::api_status(400, format!("invalid Anthropic request JSON: {error}"))
            })?;
        let model = raw
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError::api_status(400, "count_tokens requires model"))?
            .to_string();
        if raw.get("max_tokens").is_none() {
            raw.set_member("max_tokens", json!(1).into())
                .map_err(|error| BackendError::Other(error.to_string()))?;
        }
        let chat = crate::translation::anthropic_chat::translate_request(
            &serde_json::to_vec(&raw).map_err(|error| BackendError::Other(error.to_string()))?,
        )?;
        let converse = Self::converse_body(&chat)?.without_inference_config();
        let response = self
            .post(
                self.model_url(&model, "count-tokens")?,
                &CountTokensBody {
                    input: CountTokensInput { converse },
                },
            )
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(super::provider::upstream_error(response, |text| {
                serde_json::from_str::<Value>(text)
                    .ok()
                    .and_then(|value| bedrock_error_message(&value))
            })
            .await);
        }
        let headers = forwardable_upstream_headers(response.headers());
        let value: Value = response.json().await?;
        BackendJsonResponse::from_value(
            json!({"input_tokens":value.get("inputTokens").cloned().unwrap_or(json!(0))}),
            headers,
        )
    }

    async fn create_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        if let Some(openai) = &self.openai {
            return openai.create_chat_completion(request).await;
        }
        self.converse(request).await
    }
    async fn create_chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<SseByteStream, BackendError> {
        if let Some(openai) = &self.openai {
            return openai.create_chat_completion_stream(request).await;
        }
        self.converse_stream(request).await
    }
    async fn create_embedding(
        &self,
        _request: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(unsupported("embeddings"))
    }
    async fn create_image(
        &self,
        _request: CreateImageRequest,
    ) -> Result<ImageResponse, BackendError> {
        Err(unsupported("images"))
    }
    async fn create_speech(&self, _request: CreateSpeechRequest) -> Result<Bytes, BackendError> {
        Err(unsupported("speech"))
    }
    async fn create_transcription(
        &self,
        _model: String,
        _file_bytes: Bytes,
        _file_name: String,
        _language: Option<String>,
        _prompt: Option<String>,
        _response_format: Option<String>,
        _temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(unsupported("transcription"))
    }
    async fn create_translation(
        &self,
        _model: String,
        _file_bytes: Bytes,
        _file_name: String,
        _prompt: Option<String>,
        _response_format: Option<String>,
        _temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(unsupported("translation"))
    }
    async fn create_moderation(
        &self,
        _request: CreateModerationRequest,
    ) -> Result<CreateModerationResponse, BackendError> {
        Err(unsupported("moderation"))
    }
    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        Err(unsupported("model listing"))
    }
    async fn get_model(&self, model_id: &str) -> Result<Model, BackendError> {
        Ok(Model {
            id: model_id.to_string(),
            object: "model".to_string(),
            created: 0,
            owned_by: "bedrock".to_string(),
            capabilities: None,
            provider_metadata: Default::default(),
        })
    }
    fn name(&self) -> &str {
        "bedrock"
    }
}

/// Converse request body. Only tool schemas need key-order retention, so
/// only `toolConfig` is a `SchemaJson`; everything else stays plain `Value`.
struct ConverseBody {
    messages: Vec<Value>,
    system: Option<Vec<Value>>,
    inference_config: Option<Value>,
    tool_config: Option<crate::schema_json::SchemaJson>,
}

impl ConverseBody {
    fn without_inference_config(mut self) -> Self {
        self.inference_config = None;
        self
    }
}

impl serde::Serialize for ConverseBody {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        if let Some(system) = &self.system {
            map.serialize_entry("system", system)?;
        }
        map.serialize_entry("messages", &self.messages)?;
        if let Some(config) = &self.inference_config {
            map.serialize_entry("inferenceConfig", config)?;
        }
        if let Some(tools) = &self.tool_config {
            map.serialize_entry("toolConfig", tools)?;
        }
        map.end()
    }
}

#[derive(serde::Serialize)]
struct CountTokensBody {
    input: CountTokensInput,
}

#[derive(serde::Serialize)]
struct CountTokensInput {
    converse: ConverseBody,
}

#[cfg(test)]
#[path = "bedrock/mantle_tests.rs"]
mod mantle_tests;

#[cfg(test)]
#[path = "bedrock/mantle_native_tests.rs"]
mod mantle_native_tests;

#[cfg(test)]
mod tests {
    use super::{
        is_runtime_connection, resolve_bedrock_anthropic_base_url, resolve_bedrock_base_url,
        unsupported, BackendError, BedrockEventDecoder, BedrockRuntimeBackend,
        ConverseStreamTranslator, CountTokensBody, CountTokensInput, DEFAULT_BEDROCK_REGION,
    };
    use crate::types::{ChatCompletionRequest, ChatMessage};
    use aws_smithy_eventstream::frame::write_message_to;
    use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
    use serde_json::{json, Value};

    fn stream_message(message_type: &str, smithy_type: &str, payload: Value) -> Message {
        let type_header = if message_type == "event" {
            ":event-type"
        } else {
            ":exception-type"
        };
        Message::new(serde_json::to_vec(&payload).unwrap())
            .add_header(Header::new(
                ":message-type",
                HeaderValue::String(message_type.to_string().into()),
            ))
            .add_header(Header::new(
                type_header,
                HeaderValue::String(smithy_type.to_string().into()),
            ))
            .add_header(Header::new(
                ":content-type",
                HeaderValue::String("application/json".into()),
            ))
    }

    fn event(smithy_type: &str, payload: Value) -> Message {
        stream_message("event", smithy_type, payload)
    }

    fn event_bytes(message: &Message) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_message_to(message, &mut bytes).unwrap();
        bytes
    }

    fn sse_json(bytes: &[u8]) -> Value {
        let text = std::str::from_utf8(bytes).unwrap();
        serde_json::from_str(text.strip_prefix("data: ").unwrap().trim()).unwrap()
    }

    #[test]
    fn resolves_default_region() {
        assert_eq!(DEFAULT_BEDROCK_REGION, "us-east-1");
        assert_eq!(
            resolve_bedrock_base_url(None, None),
            "https://bedrock-mantle.us-east-1.api.aws/v1"
        );
        assert_eq!(
            resolve_bedrock_base_url(Some("  "), None),
            "https://bedrock-mantle.us-east-1.api.aws/v1"
        );
    }

    #[test]
    fn resolves_custom_region() {
        assert_eq!(
            resolve_bedrock_base_url(Some(" eu-west-1 "), None),
            "https://bedrock-mantle.eu-west-1.api.aws/v1"
        );
    }

    #[test]
    fn normalizes_base_url_override() {
        assert_eq!(
            resolve_bedrock_base_url(
                Some("ignored-region"),
                Some("  https://bedrock.example.com/openai/v1///  "),
            ),
            "https://bedrock.example.com/openai/v1"
        );
    }

    #[test]
    fn derives_the_mantle_anthropic_protocol_root() {
        assert_eq!(
            resolve_bedrock_anthropic_base_url("https://bedrock-mantle.us-east-1.api.aws/v1"),
            "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1"
        );
    }

    #[test]
    fn identifies_direct_and_inference_profile_anthropic_model_ids() {
        for model in [
            "anthropic.claude-sonnet-5",
            "us.anthropic.claude-sonnet-4-5-v1:0",
            "global.anthropic.claude-opus-4-5-v1:0",
        ] {
            assert!(super::is_bedrock_anthropic_model(model), "{model}");
        }
        assert!(!super::is_bedrock_anthropic_model("amazon.nova-pro-v1:0"));
    }

    #[test]
    fn runtime_transport_and_region_are_derived_from_the_endpoint() {
        let base_url = Some("https://bedrock-runtime.eu-west-1.amazonaws.com/");
        assert!(is_runtime_connection(base_url));
        let backend = BedrockRuntimeBackend::new(reqwest::Client::new(), base_url).unwrap();
        assert_eq!(
            backend.base_url,
            "https://bedrock-runtime.eu-west-1.amazonaws.com"
        );
        assert_eq!(
            super::runtime_endpoint(base_url).unwrap(),
            Some(super::RuntimeEndpoint::Converse {
                region: "eu-west-1".to_string()
            })
        );
        assert!(!is_runtime_connection(Some(
            "https://bedrock-mantle.eu-west-1.api.aws/v1"
        )));
        assert!(!is_runtime_connection(Some(
            "https://bedrock-runtime.eu-west-1.example.com"
        )));
        assert!(!is_runtime_connection(Some(
            "http://bedrock-runtime.eu-west-1.amazonaws.com"
        )));
    }

    #[test]
    fn converse_treats_developer_as_system_and_defaults_unusable_tool_arguments() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "anthropic.claude-sonnet-4-6-v1",
            "user": "tracking-id",
            "messages": [
                {"role":"developer","content":"be brief"},
                {"role":"user","content":"hi"},
                {"role":"assistant","tool_calls":[
                    {"id":"a","type":"function","function":{"name":"f","arguments":""}},
                    {"id":"b","type":"function","function":{"name":"f","arguments":"not-json"}},
                    {"id":"c","type":"function","function":{"name":"f","arguments":"[1]"}}
                ]},
                {"role":"tool","tool_call_id":"a","content":"ok"}
            ]
        }))
        .unwrap();
        let body =
            serde_json::to_value(BedrockRuntimeBackend::converse_body(&request).unwrap()).unwrap();
        assert_eq!(body["system"][0]["text"], "be brief");
        let messages = body["messages"].as_array().unwrap();
        assert!(messages
            .iter()
            .all(|message| message["role"] == "user" || message["role"] == "assistant"));
        let tool_uses = messages[1]["content"].as_array().unwrap();
        assert_eq!(tool_uses.len(), 3);
        for tool_use in tool_uses {
            assert_eq!(tool_use["toolUse"]["input"], json!({}));
        }
    }

    #[test]
    fn converse_maps_text_images_and_tools() {
        let request = ChatCompletionRequest {
            fallbacks: Default::default(),
            model: "anthropic.claude-sonnet-4-6-v1".to_string(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: Some(Value::String("safe".to_string())),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: Some(
                        json!([{"type":"text","text":"look"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]),
                    ),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            temperature: Some(0.2),
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            max_tokens: Some(32),
            max_completion_tokens: None,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
            user: None,
            tools: Some(vec![crate::types::Tool {
                tool_type: "function".to_string(),
                function: crate::types::ToolFunction {
                    name: "weather".to_string(),
                    description: None,
                    parameters: Some(json!({"type":"object"}).into()),
                },
            }]),
            tool_choice: None,
            response_format: None,
            stream_options: None,
            parallel_tool_calls: None,
            reasoning_effort: None,
        };
        let body =
            serde_json::to_value(BedrockRuntimeBackend::converse_body(&request).unwrap()).unwrap();
        assert_eq!(body["system"][0]["text"], "safe");
        assert_eq!(body["messages"][0]["content"][1]["image"]["format"], "png");
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["name"],
            "weather"
        );
    }

    #[test]
    fn converse_retains_tool_property_order_in_generation_and_count_body() {
        let request: ChatCompletionRequest = serde_json::from_str(r#"{"model":"test","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"lookup","parameters":{"properties":{"z":{"minimum":1.2300e-4,"items":{"properties":{"yy":{},"bb":{}}}},"a":{}}}}},{"type":"function","function":{"name":"second","parameters":{"properties":{"q":{},"b":{}}}}}]}"#).unwrap();
        let body = BedrockRuntimeBackend::converse_body(&request).unwrap();
        let schema = r#""properties":{"z":{"minimum":1.2300e-4,"items":{"properties":{"yy":{},"bb":{}}}},"a":{}}"#;
        assert!(serde_json::to_string(&body).unwrap().contains(schema));
        let wire = serde_json::to_string(&body).unwrap();
        assert!(wire.contains(r#""properties":{"q":{},"b":{}}"#));
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["toolConfig"]["tools"].as_array().unwrap().len(), 2);
        let count = CountTokensBody {
            input: CountTokensInput {
                converse: body.without_inference_config(),
            },
        };
        assert!(serde_json::to_string(&count).unwrap().contains(schema));
        let count = serde_json::to_value(&count).unwrap();
        assert!(count["input"]["converse"].get("inferenceConfig").is_none());
    }

    #[test]
    fn converse_body_serializes_like_the_json_literal() {
        let with_tools: ChatCompletionRequest = serde_json::from_str(r#"{"model":"t","max_tokens":5,"stop":["x"],"messages":[{"role":"system","content":"safe"},{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"w","description":"d","parameters":{"type":"object"}}}]}"#).unwrap();
        let body = serde_json::to_value(BedrockRuntimeBackend::converse_body(&with_tools).unwrap())
            .unwrap();
        assert_eq!(
            body,
            json!({
                "system":[{"text":"safe"}],
                "messages":[{"role":"user","content":[{"text":"hi"}]}],
                "inferenceConfig":{"maxTokens":5,"temperature":null,"topP":null,"stopSequences":["x"]},
                "toolConfig":{"tools":[{"toolSpec":{"name":"w","description":"d","inputSchema":{"json":{"type":"object"}}}}]}
            })
        );
        let bare: ChatCompletionRequest =
            serde_json::from_str(r#"{"model":"t","messages":[{"role":"user","content":"hi"}]}"#)
                .unwrap();
        let body =
            serde_json::to_value(BedrockRuntimeBackend::converse_body(&bare).unwrap()).unwrap();
        assert_eq!(
            body,
            json!({
                "messages":[{"role":"user","content":[{"text":"hi"}]}],
                "inferenceConfig":{"maxTokens":null,"temperature":null,"topP":null,"stopSequences":null}
            })
        );
    }

    #[test]
    fn event_decoder_handles_fragmented_and_coalesced_frames() {
        let first = event_bytes(&event("messageStart", json!({"role":"assistant"})));
        let second = event_bytes(&event(
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"text":"hello"}}),
        ));
        let combined = [first, second].concat();
        let mut decoder = BedrockEventDecoder::default();
        let mut decoded = Vec::new();

        for chunk in combined.chunks(3) {
            decoded.extend(decoder.push(chunk).unwrap());
        }

        decoder.finish().unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(
            serde_json::from_slice::<Value>(decoded[1].payload()).unwrap()["delta"]["text"],
            "hello"
        );
    }

    #[test]
    fn event_decoder_rejects_corrupt_and_truncated_frames() {
        let mut corrupt = event_bytes(&event("messageStart", json!({"role":"assistant"})));
        *corrupt.last_mut().unwrap() ^= 0xff;
        let error = BedrockEventDecoder::default().push(&corrupt).unwrap_err();
        assert!(error.to_string().contains("invalid Bedrock ConverseStream"));

        let valid = event_bytes(&event("messageStart", json!({"role":"assistant"})));
        let mut decoder = BedrockEventDecoder::default();
        decoder.push(&valid[..valid.len() - 1]).unwrap();
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn converse_stream_translates_text_tools_finish_and_usage() {
        let mut translator = ConverseStreamTranslator::new("bedrock/model".to_string());

        let start = sse_json(
            &translator
                .translate(event("messageStart", json!({"role":"assistant"})))
                .unwrap()
                .unwrap(),
        );
        assert_eq!(start["choices"][0]["delta"]["role"], "assistant");

        let text = sse_json(
            &translator
                .translate(event(
                    "contentBlockDelta",
                    json!({"contentBlockIndex":0,"delta":{"text":"hello"}}),
                ))
                .unwrap()
                .unwrap(),
        );
        assert_eq!(text["choices"][0]["delta"]["content"], "hello");

        let tool_start = sse_json(
            &translator
                .translate(event(
                    "contentBlockStart",
                    json!({
                        "contentBlockIndex":1,
                        "start":{"toolUse":{"toolUseId":"call-1","name":"weather"}}
                    }),
                ))
                .unwrap()
                .unwrap(),
        );
        assert_eq!(
            tool_start["choices"][0]["delta"]["tool_calls"][0]["id"],
            "call-1"
        );
        assert_eq!(
            tool_start["choices"][0]["delta"]["tool_calls"][0]["index"],
            0
        );
        assert_eq!(
            tool_start["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "weather"
        );

        let tool_delta = sse_json(
            &translator
                .translate(event(
                    "contentBlockDelta",
                    json!({"contentBlockIndex":1,"delta":{"toolUse":{"input":"{\"city\":"}}}),
                ))
                .unwrap()
                .unwrap(),
        );
        assert_eq!(
            tool_delta["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":"
        );

        let stop = sse_json(
            &translator
                .translate(event("messageStop", json!({"stopReason":"tool_use"})))
                .unwrap()
                .unwrap(),
        );
        assert!(translator.saw_message_stop);
        assert_eq!(stop["choices"][0]["finish_reason"], "tool_calls");

        let usage = sse_json(
            &translator
                .translate(event(
                    "metadata",
                    json!({"usage":{"inputTokens":7,"outputTokens":3,"totalTokens":10}}),
                ))
                .unwrap()
                .unwrap(),
        );
        assert_eq!(usage["choices"], json!([]));
        assert_eq!(usage["usage"]["prompt_tokens"], 7);
        assert_eq!(usage["usage"]["completion_tokens"], 3);
        assert_eq!(usage["usage"]["total_tokens"], 10);
    }

    #[test]
    fn converse_stream_surfaces_modeled_exceptions() {
        let mut translator = ConverseStreamTranslator::new("bedrock/model".to_string());
        let error = translator
            .translate(stream_message(
                "exception",
                "throttlingException",
                json!({"message":"slow down"}),
            ))
            .unwrap_err();
        assert!(error.to_string().contains("429"));
        assert!(error.to_string().contains("slow down"));
    }

    #[test]
    fn unsupported_operations_are_client_errors() {
        // Asking Bedrock's Converse-backed adapter for an operation it does not
        // implement (embeddings, images, ...) is a mismatch in the caller's
        // request, not a failure of the adapter itself, so it must surface as
        // a 400 rather than the 500 every other unmapped BackendError produces.
        let error = unsupported("embeddings");
        assert!(matches!(error, BackendError::ApiError { status: 400, .. }));
        assert!(error.to_string().contains("embeddings"));
    }
}

#[cfg(test)]
#[path = "bedrock/runtime_tests.rs"]
mod runtime_tests;
