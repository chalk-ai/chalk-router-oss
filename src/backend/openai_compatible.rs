use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;

use super::provider::forwardable_upstream_headers;
use super::provider::{
    AnthropicRequestEndpoint, AnthropicRequestHeaders, BackendBodyResponse, BackendChatResponse,
    BackendError, BackendJsonResponse, BackendJsonView, BackendProvider, BackendSseResponse,
    OriginalUpstreamError, ResponsesRequestHeaders, SseByteStream, UpstreamErrorFormat,
};
use crate::responses::ResponsesRequest;
use crate::types::{
    ChatCompletionRequest, ChatCompletionResponse, CreateEmbeddingRequest, CreateEmbeddingResponse,
    CreateImageRequest, CreateModerationRequest, CreateModerationResponse, CreateSpeechRequest,
    ErrorResponse, ImageResponse, ListResponse, Model, StreamOptions, TranscriptionResponse,
};

/// HTTP backend shared by OpenAI and providers with OpenAI-compatible APIs.
pub struct OpenAICompatibleBackend {
    client: Client,
    api_key: String,
    base_url: String,
    provider_name: String,
    normalize_gemini_image_models: bool,
    anthropic_base_url: Option<String>,
    anthropic_uses_x_api_key: bool,
    bedrock_sigv4: Option<std::sync::Arc<super::aws_auth::BedrockSigv4>>,
}

/// OpenAI-compatible providers use either OpenAI's `{ "data": [...] }`
/// envelope or return the model array directly.
#[derive(Deserialize)]
#[serde(untagged)]
enum CompatibleModelList {
    Wrapped(ListResponse<Model>),
    Bare(Vec<Model>),
}

/// Drop catalog entries the provider itself reports as unservable. Bedrock
/// Mantle lists models with `"status": "unavailable"` and a `status_reason`
/// (for example a model unavailable under the account's data-retention
/// mode); listing them advertises models every call to which fails.
fn without_unavailable_models(mut listing: serde_json::Value) -> serde_json::Value {
    let data = match &mut listing {
        serde_json::Value::Object(object) => object.get_mut("data"),
        array @ serde_json::Value::Array(_) => Some(array),
        _ => None,
    };
    if let Some(serde_json::Value::Array(models)) = data {
        models.retain(|model| {
            model.get("status").and_then(serde_json::Value::as_str) != Some("unavailable")
        });
    }
    listing
}

/// The upstream format a provider's Decisions endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecisionsWire {
    /// OpenRouter's Decisions contract (System One plus routing fields).
    OpenRouter,
    /// OpenAI's `POST /v1/decisions`.
    OpenAi,
    /// Perplexity's System One contract: no extra fields, images inline.
    Perplexity,
}

/// The provider's own reason a catalog entry cannot be served, when its
/// `status` is `"unavailable"` (the same marker [`without_unavailable_models`]
/// filters on).
fn unavailable_reason(model: &Model) -> Option<String> {
    let status = model.provider_metadata.get("status")?.as_str()?;
    if status != "unavailable" {
        return None;
    }
    Some(
        model
            .provider_metadata
            .get("status_reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("the provider reports it as unavailable")
            .to_string(),
    )
}

impl CompatibleModelList {
    fn into_list_response(self) -> ListResponse<Model> {
        match self {
            Self::Wrapped(response) => response,
            Self::Bare(data) => ListResponse {
                object: "list".to_string(),
                data,
                first_id: None,
                last_id: None,
                has_more: false,
            },
        }
    }
}

/// When enabled via `OPENAI_STRICT_SCHEMA_NORMALIZE`, rewrite a
/// `response_format` json_schema whose `strict` flag is set so it satisfies
/// OpenAI's strict-mode requirements (`additionalProperties: false` everywhere,
/// every property `required`). No-op otherwise, so a caller's schema is left
/// untouched by default.
fn normalize_strict_response_format(
    request: &mut ChatCompletionRequest,
) -> Result<(), BackendError> {
    if request.response_format.is_none() {
        return Ok(());
    }
    if let Some(format) = normalized_strict_response_format(
        request.response_format.as_ref(),
        crate::structured_output::openai_strict_normalize_enabled(),
    )? {
        request.response_format = Some(format);
    }
    Ok(())
}

/// Clone only the schema handle, after establishing that a rewrite is needed.
fn normalized_strict_response_format(
    format: Option<&crate::schema_json::SchemaJson>,
    enabled: bool,
) -> Result<Option<crate::schema_json::SchemaJson>, BackendError> {
    use crate::structured_output::ResponseFormat;
    if !enabled {
        return Ok(None);
    }
    let Some(format) = format else {
        return Ok(None);
    };
    if !matches!(
        ResponseFormat::parse(format).map_err(|error| {
            BackendError::api_status(400, format!("invalid response_format schema: {error}"))
        })?,
        Some(ResponseFormat::JsonSchema { strict: true, .. })
    ) {
        return Ok(None);
    }
    let mut normalized = format.clone();
    normalized.normalize_response_format().map_err(|error| {
        BackendError::api_status(400, format!("invalid response_format schema: {error}"))
    })?;
    Ok(Some(normalized))
}

fn normalize_native_response_format(
    request: &mut crate::native_chat::NativeChatRequest,
) -> Result<(), BackendError> {
    if request.response_format.is_none() {
        return Ok(());
    }
    if let Some(format) = normalized_strict_response_format(
        request.response_format.as_ref(),
        crate::structured_output::openai_strict_normalize_enabled(),
    )? {
        request.set_field("response_format", &format)?;
    }
    Ok(())
}

/// Build a `BackendError::ApiError` from a non-2xx upstream response: capture
/// rate-limit / retry headers before consuming the body, try the OpenAI error
/// envelope, and fall back to the raw response text. Shared by every
/// OpenAI-compatible call site whose success path isn't decoded through
/// [`OpenAICompatibleBackend::handle_response`] (streaming, binary, and other
/// non-JSON-success responses).
async fn extract_upstream_error(response: reqwest::Response) -> BackendError {
    let status = response.status();
    let headers = forwardable_upstream_headers(response.headers());
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    let body = response.bytes().await.unwrap_or_default();
    let message = serde_json::from_slice::<ErrorResponse>(&body)
        .map(|r| r.error.message)
        .unwrap_or_else(|_| String::from_utf8_lossy(&body).into_owned());
    BackendError::ApiError {
        message,
        status: status.as_u16(),
        headers,
        reasoning_incompatible: false,
        original: Some(Box::new(OriginalUpstreamError {
            body,
            content_type,
            format: UpstreamErrorFormat::Openai,
        })),
    }
}

impl OpenAICompatibleBackend {
    pub(crate) async fn read_native_body(
        &self,
        response: reqwest::Response,
    ) -> Result<BackendBodyResponse, BackendError> {
        if !response.status().is_success() {
            return Err(extract_upstream_error(response).await);
        }
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        let headers = forwardable_upstream_headers(response.headers());
        Ok(BackendBodyResponse {
            body: response.bytes().await?,
            status,
            content_type,
            headers,
        })
    }

    pub(crate) fn with_bedrock_sigv4(
        mut self,
        signer: std::sync::Arc<super::aws_auth::BedrockSigv4>,
    ) -> Self {
        self.bedrock_sigv4 = Some(signer);
        self
    }

    pub(crate) async fn send_request(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, BackendError> {
        let mut request = builder.build()?;
        if let Some(signer) = &self.bedrock_sigv4 {
            request.headers_mut().remove(reqwest::header::AUTHORIZATION);
            signer.sign(&mut request).await?;
        }
        self.client
            .execute(request)
            .await
            .map_err(BackendError::from)
    }

    /// Create a new OpenAI backend with the given API key
    pub fn new(client: Client, api_key: String) -> Self {
        Self {
            client,
            api_key,
            base_url: super::openai::OPENAI_API_BASE_URL.to_string(),
            provider_name: "openai".to_string(),
            normalize_gemini_image_models: false,
            anthropic_base_url: None,
            anthropic_uses_x_api_key: false,
            bedrock_sigv4: None,
        }
    }

    /// Create a new OpenAI backend with a custom base URL
    pub fn with_base_url(client: Client, api_key: String, base_url: String) -> Self {
        Self {
            client,
            api_key,
            base_url,
            provider_name: "openai".to_string(),
            normalize_gemini_image_models: false,
            anthropic_base_url: None,
            anthropic_uses_x_api_key: false,
            bedrock_sigv4: None,
        }
    }

    /// Create an OpenAI-compatible backend with a provider-specific identity.
    pub fn for_provider(
        client: Client,
        provider_name: impl Into<String>,
        api_key: String,
        base_url: String,
    ) -> Self {
        Self {
            client,
            api_key,
            base_url,
            provider_name: provider_name.into(),
            normalize_gemini_image_models: false,
            anthropic_base_url: None,
            anthropic_uses_x_api_key: false,
            bedrock_sigv4: None,
        }
    }

    /// Construct the Gemini adapter with its image model-ID contract. Keep this
    /// independent of display names, which custom compatible providers can reuse.
    pub fn for_gemini(client: Client, api_key: String, base_url: String) -> Self {
        Self {
            normalize_gemini_image_models: true,
            ..Self::for_provider(client, "gemini", api_key, base_url)
        }
    }

    /// Configure an officially supported Anthropic-compatible API root for a
    /// provider that also exposes OpenAI-compatible endpoints.
    pub fn with_anthropic_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.anthropic_base_url = Some(base_url.into().trim_end_matches('/').to_string());
        self
    }

    pub fn with_anthropic_x_api_key_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.anthropic_base_url = Some(base_url.into().trim_end_matches('/').to_string());
        self.anthropic_uses_x_api_key = true;
        self
    }

    async fn fetch_model_list(&self, query: &str) -> Result<ListResponse<Model>, BackendError> {
        let url = format!("{}/models{query}", self.base_url);
        let response = self
            .send_request(
                self.client
                    .get(&url)
                    .header("Authorization", format!("Bearer {}", self.api_key)),
            )
            .await?;

        let listing: serde_json::Value = self.handle_response(response).await?;
        serde_json::from_value::<CompatibleModelList>(without_unavailable_models(listing))
            .map(CompatibleModelList::into_list_response)
            .map_err(|e| BackendError::InvalidResponse(e.to_string()))
    }

    /// Where this provider takes Decisions requests and in which format.
    fn decisions_endpoint(
        &self,
        ingress: crate::decisions::DecisionsIngress,
    ) -> Result<(String, DecisionsWire), BackendError> {
        use crate::decisions::DecisionsIngress;
        let base_url = self.base_url.trim_end_matches('/');
        match self.provider_name.as_str() {
            "openrouter" => {
                let origin = base_url.strip_suffix("/api/v1").ok_or_else(|| {
                    BackendError::ConfigError(
                        "OpenRouter base URL must end in `/api/v1` to derive the Decisions endpoint"
                            .to_string(),
                    )
                })?;
                let url = match ingress {
                    DecisionsIngress::SystemOne => format!("{origin}/api/v1/systemone"),
                    DecisionsIngress::Alpha | DecisionsIngress::OpenAi => {
                        format!("{origin}/api/alpha/decisions")
                    }
                };
                Ok((url, DecisionsWire::OpenRouter))
            }
            "openai" => Ok((format!("{base_url}/decisions"), DecisionsWire::OpenAi)),
            "perplexity" => {
                let origin = base_url.strip_suffix("/v1").unwrap_or(base_url);
                Ok((format!("{origin}/v1/decisions"), DecisionsWire::Perplexity))
            }
            provider => Err(BackendError::api_status(
                400,
                format!("provider '{provider}' does not support Decisions requests"),
            )),
        }
    }

    /// Render the canonical request in `wire`'s format. Translation failures
    /// are the caller's to fix.
    fn decisions_body(
        request: &crate::decisions::DecisionsRequest,
        wire: DecisionsWire,
    ) -> Result<serde_json::Value, String> {
        use crate::decisions::DecisionsIngress;
        match wire {
            DecisionsWire::OpenAi => {
                crate::translation::decisions_openai::canonical_to_upstream(request)
            }
            DecisionsWire::OpenRouter => {
                let mut body = serde_json::json!({
                    "model": request.model,
                    "state": request.state,
                    "questions": request.questions,
                });
                let assets = request.system_one_assets()?;
                if !assets.is_empty() {
                    body["assets"] = serde_json::to_value(assets).map_err(|e| e.to_string())?;
                }
                if let Some(session_id) = &request.session_id {
                    body["session_id"] = serde_json::Value::String(session_id.clone());
                }
                if request.forwards_extra_to(DecisionsIngress::Alpha) {
                    for (name, value) in &request.extra {
                        if body.get(name).is_none() {
                            body[name] = value.clone();
                        }
                    }
                }
                Ok(body)
            }
            DecisionsWire::Perplexity => {
                if request.has_box_questions() {
                    return Err("Perplexity does not support box questions".to_string());
                }
                if crate::decisions::questions_reference_assets(&request.questions, &request.assets)
                {
                    return Err(
                        "Perplexity takes images in `state` only, not in questions".to_string()
                    );
                }
                if request
                    .assets
                    .values()
                    .any(|asset| asset.image().url.is_some())
                {
                    return Err("Perplexity takes base64 images only, not image URLs".to_string());
                }
                let state = crate::decisions::state_with_inline_images(
                    &request.state,
                    &request.assets,
                    |image| {
                        serde_json::json!({
                            "type": "image_url",
                            "image_url": {"url": image.data_url().unwrap_or_default()},
                        })
                    },
                );
                Ok(serde_json::json!({
                    "model": request.model,
                    "state": state,
                    "questions": request.questions,
                }))
            }
        }
    }

    fn anthropic_url(
        &self,
        endpoint: AnthropicRequestEndpoint,
    ) -> Result<reqwest::Url, BackendError> {
        let base = self.anthropic_base_url.as_deref().ok_or_else(|| {
            BackendError::ConfigError(format!(
                "provider '{}' has no native Anthropic endpoint configured",
                self.provider_name
            ))
        })?;
        let mut url = reqwest::Url::parse(base).map_err(|error| {
            BackendError::ConfigError(format!("invalid Anthropic-compatible base URL: {error}"))
        })?;
        let mut path = url.path_segments_mut().map_err(|_| {
            BackendError::ConfigError("Anthropic base URL cannot be a base".to_string())
        })?;
        path.pop_if_empty();
        path.push("messages");
        if endpoint == AnthropicRequestEndpoint::CountTokens {
            path.push("count_tokens");
        }
        drop(path);
        Ok(url)
    }

    fn anthropic_request(
        &self,
        endpoint: AnthropicRequestEndpoint,
        headers: &AnthropicRequestHeaders,
    ) -> Result<reqwest::RequestBuilder, BackendError> {
        let mut request = self
            .client
            .post(self.anthropic_url(endpoint)?)
            .header("content-type", "application/json");
        request = if self.anthropic_uses_x_api_key {
            request.header("x-api-key", &self.api_key)
        } else {
            request.header("Authorization", format!("Bearer {}", self.api_key))
        };
        for (name, value) in headers.iter() {
            request = request.header(name, value);
        }
        Ok(request)
    }

    /// Helper method to handle OpenAI API responses
    async fn handle_response<T: for<'de> Deserialize<'de>>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, BackendError> {
        if response.status().is_success() {
            response.json::<T>().await.map_err(BackendError::from)
        } else {
            Err(extract_upstream_error(response).await)
        }
    }

    fn responses_request(
        &self,
        method: reqwest::Method,
        url: &str,
        headers: &ResponsesRequestHeaders,
    ) -> Result<reqwest::RequestBuilder, BackendError> {
        let mut request = self
            .client
            .request(method, url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json");

        for (name, value) in headers.iter() {
            let name =
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                    BackendError::Other(format!("invalid forwarded header name: {error}"))
                })?;
            let value = reqwest::header::HeaderValue::from_str(value).map_err(|error| {
                BackendError::Other(format!("invalid forwarded header value: {error}"))
            })?;
            request = request.header(name, value);
        }
        Ok(request)
    }

    fn response_item_url(&self, response_id: &str) -> Result<reqwest::Url, BackendError> {
        let mut url = reqwest::Url::parse(&self.base_url).map_err(|error| {
            BackendError::ConfigError(format!("invalid OpenAI-compatible base URL: {error}"))
        })?;
        url.path_segments_mut()
            .map_err(|_| BackendError::ConfigError("base URL cannot be a base".to_string()))?
            .extend(["responses", response_id]);
        Ok(url)
    }

    fn response_stream_url(
        &self,
        response_id: &str,
        starting_after: Option<u64>,
    ) -> Result<reqwest::Url, BackendError> {
        let mut url = self.response_item_url(response_id)?;
        let mut query = url.query_pairs_mut();
        query.append_pair("stream", "true");
        if let Some(starting_after) = starting_after {
            query.append_pair("starting_after", &starting_after.to_string());
        }
        drop(query);
        Ok(url)
    }

    fn response_action_url(
        &self,
        response_id: &str,
        action: &str,
    ) -> Result<reqwest::Url, BackendError> {
        let mut url = self.response_item_url(response_id)?;
        url.path_segments_mut()
            .map_err(|_| BackendError::ConfigError("base URL cannot be a base".to_string()))?
            .push(action);
        Ok(url)
    }

    async fn handle_responses_json(
        &self,
        response: reqwest::Response,
    ) -> Result<BackendJsonResponse, BackendError> {
        BackendJsonResponse::from_response(self.read_native_body(response).await?)
    }

    async fn handle_responses_sse(
        &self,
        response: reqwest::Response,
    ) -> Result<BackendSseResponse, BackendError> {
        if !response.status().is_success() {
            return Err(extract_upstream_error(response).await);
        }
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if !content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case("text/event-stream")
        {
            return Err(BackendError::InvalidResponse(
                "upstream returned an unexpected Responses SSE content type".into(),
            ));
        }
        let headers = forwardable_upstream_headers(response.headers());
        Ok(BackendSseResponse {
            stream: Box::pin(
                response
                    .bytes_stream()
                    .map(|result| result.map_err(BackendError::from)),
            ),
            headers,
            status,
            content_type,
        })
    }

    fn requests_stream_usage(&self) -> bool {
        matches!(
            self.provider_name.as_str(),
            "openai"
                | "gemini"
                | "vllm"
                | "openrouter"
                | "xai"
                | "qwen"
                | "fireworks"
                | "together"
                | "groq"
                | "mistral"
                | "bedrock"
        )
    }

    fn native_chat_http_request(
        &self,
        request: &crate::native_chat::NativeChatRequest,
    ) -> Result<reqwest::RequestBuilder, BackendError> {
        let mut builder = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/json");
        for (name, value) in request.headers() {
            builder = builder.header(name, value);
        }
        Ok(builder.body(
            serde_json::to_vec(request).map_err(|error| BackendError::Other(error.to_string()))?,
        ))
    }

    /// Serialize a streaming request for the upstream API.
    ///
    /// OpenAI only sends usage in a streamed chat-completion response when
    /// `stream_options.include_usage` is enabled. The terminal chunk is what
    /// the chat handler uses for trace and per-key token accounting, so enable
    /// it for built-in OpenAI-compatible providers whose APIs support the
    /// standard flag. Arbitrary compatible endpoints remain untouched.
    fn stream_request_payload(
        &self,
        mut request: ChatCompletionRequest,
    ) -> Result<Vec<u8>, BackendError> {
        request.stream_options = self.requests_stream_usage().then_some(StreamOptions {
            include_usage: true,
        });

        serde_json::to_vec(&request)
            .map_err(|e| BackendError::Other(format!("Failed to serialize streaming request: {e}")))
    }
}

fn normalize_missing_embedding_indexes(
    response: &mut BackendBodyResponse,
) -> Result<(), BackendError> {
    use serde_json::value::RawValue;
    use std::collections::BTreeMap;
    let mut root: BTreeMap<String, Box<RawValue>> = serde_json::from_slice(&response.body)
        .map_err(|error| BackendError::Other(error.to_string()))?;
    let Some(data) = root.get("data") else {
        return Ok(());
    };
    let mut entries: Vec<BTreeMap<String, Box<RawValue>>> =
        serde_json::from_str(data.get()).map_err(|error| BackendError::Other(error.to_string()))?;
    let mut changed = false;
    for (index, entry) in entries.iter_mut().enumerate() {
        if entry
            .get("index")
            .is_none_or(|raw| raw.get().trim() == "null")
        {
            entry.insert(
                "index".into(),
                serde_json::value::to_raw_value(&index).expect("index serializes"),
            );
            changed = true;
        }
    }
    if changed {
        root.insert(
            "data".into(),
            serde_json::value::to_raw_value(&entries)
                .map_err(|error| BackendError::Other(error.to_string()))?,
        );
        response.body = serde_json::to_vec(&root)
            .map_err(|error| BackendError::Other(error.to_string()))?
            .into();
    }
    Ok(())
}

fn normalize_missing_image_created(response: &mut BackendBodyResponse) -> Result<(), BackendError> {
    use serde_json::value::RawValue;
    use std::collections::BTreeMap;
    let mut root: BTreeMap<String, Box<RawValue>> = serde_json::from_slice(&response.body)
        .map_err(|error| BackendError::Other(error.to_string()))?;
    if !root.contains_key("created") {
        root.insert(
            "created".into(),
            serde_json::value::to_raw_value(&0).expect("timestamp serializes"),
        );
        response.body = serde_json::to_vec(&root)
            .map_err(|error| BackendError::Other(error.to_string()))?
            .into();
    }
    Ok(())
}

#[async_trait]
impl BackendProvider for OpenAICompatibleBackend {
    async fn send_decisions_request(
        &self,
        request: crate::decisions::DecisionsRequest,
    ) -> Result<reqwest::Response, BackendError> {
        let (url, wire) = self.decisions_endpoint(request.ingress)?;
        let body = Self::decisions_body(&request, wire)
            .map_err(|message| BackendError::api_status(400, message))?;
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        if wire != DecisionsWire::OpenAi || !response.status().is_success() {
            return Ok(response);
        }
        // OpenAI answers in its own format; hand the pipeline canonical
        // answers so usage, pricing, and every ingress renderer see one shape.
        let status = response.status();
        let headers = response.headers().clone();
        let upstream: serde_json::Value = response.json().await.map_err(|error| {
            BackendError::InvalidResponse(format!("invalid OpenAI Decisions response: {error}"))
        })?;
        let canonical =
            crate::translation::decisions_openai::upstream_to_canonical(&request, upstream)
                .map_err(BackendError::InvalidResponse)?;
        let bytes = serde_json::to_vec(&canonical)
            .map_err(|error| BackendError::InvalidResponse(error.to_string()))?;
        let mut translated = axum::http::Response::builder().status(status.as_u16());
        for (name, value) in &headers {
            if name != reqwest::header::CONTENT_LENGTH && name != reqwest::header::CONTENT_ENCODING
            {
                translated = translated.header(name, value);
            }
        }
        translated
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .map(reqwest::Response::from)
            .map_err(|error| BackendError::InvalidResponse(error.to_string()))
    }

    async fn send_anthropic_request(
        &self,
        endpoint: AnthropicRequestEndpoint,
        headers: &AnthropicRequestHeaders,
        body: bytes::Bytes,
    ) -> Result<reqwest::Response, BackendError> {
        Ok(self
            .anthropic_request(endpoint, headers)?
            .body(body)
            .send()
            .await?)
    }

    async fn create_response(
        &self,
        mut request: ResponsesRequest,
        headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        request.set_stream(false);
        let url = format!("{}/responses", self.base_url);
        let response = self
            .send_request(
                self.responses_request(reqwest::Method::POST, &url, headers)?
                    .json(&request),
            )
            .await?;
        self.handle_responses_json(response).await
    }

    async fn create_response_stream(
        &self,
        mut request: ResponsesRequest,
        headers: &ResponsesRequestHeaders,
    ) -> Result<BackendSseResponse, BackendError> {
        request.set_stream(true);
        let url = format!("{}/responses", self.base_url);
        let response = self
            .send_request(
                self.responses_request(reqwest::Method::POST, &url, headers)?
                    .header("Accept", "text/event-stream")
                    .json(&request),
            )
            .await?;
        self.handle_responses_sse(response).await
    }

    async fn compact_response(
        &self,
        request: crate::schema_json::SchemaJson,
        headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        let url = format!("{}/responses/compact", self.base_url);
        let response = self
            .send_request(
                self.responses_request(reqwest::Method::POST, &url, headers)?
                    .json(&request),
            )
            .await?;
        self.handle_responses_json(response).await
    }

    async fn retrieve_response(
        &self,
        response_id: &str,
        headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        let url = self.response_item_url(response_id)?;
        let response = self
            .send_request(self.responses_request(reqwest::Method::GET, url.as_str(), headers)?)
            .await?;
        self.handle_responses_json(response).await
    }

    async fn retrieve_response_stream(
        &self,
        response_id: &str,
        starting_after: Option<u64>,
        headers: &ResponsesRequestHeaders,
    ) -> Result<BackendSseResponse, BackendError> {
        let url = self.response_stream_url(response_id, starting_after)?;
        let response = self
            .send_request(
                self.responses_request(reqwest::Method::GET, url.as_str(), headers)?
                    .header("Accept", "text/event-stream"),
            )
            .await?;
        self.handle_responses_sse(response).await
    }

    async fn cancel_response(
        &self,
        response_id: &str,
        headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        let url = self.response_action_url(response_id, "cancel")?;
        let response = self
            .send_request(self.responses_request(reqwest::Method::POST, url.as_str(), headers)?)
            .await?;
        self.handle_responses_json(response).await
    }

    async fn delete_response(
        &self,
        response_id: &str,
        headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        let url = self.response_item_url(response_id)?;
        let response = self
            .send_request(self.responses_request(reqwest::Method::DELETE, url.as_str(), headers)?)
            .await?;
        self.handle_responses_json(response).await
    }

    async fn create_native_audio(
        &self,
        request: crate::native_audio::NativeAudioRequest,
    ) -> Result<BackendBodyResponse, BackendError> {
        let endpoint = match request.operation {
            crate::native_audio::AudioOperation::Transcription => "transcriptions",
            crate::native_audio::AudioOperation::Translation => "translations",
        };
        let response = self
            .send_request(
                self.client
                    .post(format!("{}/audio/{endpoint}", self.base_url))
                    .bearer_auth(&self.api_key)
                    .multipart(request.multipart()?),
            )
            .await?;
        self.read_native_body(response).await
    }

    async fn create_native_embedding(
        &self,
        request: crate::native_json::NativeJsonRequest<CreateEmbeddingRequest>,
    ) -> Result<BackendJsonView<CreateEmbeddingResponse>, BackendError> {
        let response = self
            .send_request(
                self.client
                    .post(format!("{}/embeddings", self.base_url))
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .header("Content-Type", "application/json")
                    .body(
                        serde_json::to_vec(&request)
                            .map_err(|e| BackendError::Other(e.to_string()))?,
                    ),
            )
            .await?;
        let mut response = self.read_native_body(response).await?;
        normalize_missing_embedding_indexes(&mut response)?;
        let view = serde_json::from_slice(&response.body)
            .map_err(|e| BackendError::Other(e.to_string()))?;
        Ok(BackendJsonView { response, view })
    }
    async fn create_native_image(
        &self,
        mut request: crate::native_json::NativeJsonRequest<CreateImageRequest>,
    ) -> Result<BackendJsonView<ImageResponse>, BackendError> {
        if self.normalize_gemini_image_models {
            if let Some(model) = &request.model {
                request.set_model(model.strip_prefix("models/").unwrap_or(model).to_owned());
            }
        }
        let response = self
            .send_request(
                self.client
                    .post(format!("{}/images/generations", self.base_url))
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .header("Content-Type", "application/json")
                    .body(
                        serde_json::to_vec(&request)
                            .map_err(|e| BackendError::Other(e.to_string()))?,
                    ),
            )
            .await?;
        let mut response = self.read_native_body(response).await?;
        normalize_missing_image_created(&mut response)?;
        let view = serde_json::from_slice(&response.body)
            .map_err(|e| BackendError::Other(e.to_string()))?;
        Ok(BackendJsonView { response, view })
    }
    async fn create_native_speech(
        &self,
        request: crate::native_json::NativeJsonRequest<CreateSpeechRequest>,
    ) -> Result<BackendBodyResponse, BackendError> {
        let response = self
            .send_request(
                self.client
                    .post(format!("{}/audio/speech", self.base_url))
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .header("Content-Type", "application/json")
                    .body(
                        serde_json::to_vec(&request)
                            .map_err(|e| BackendError::Other(e.to_string()))?,
                    ),
            )
            .await?;
        self.read_native_body(response).await
    }

    fn supports_native_chat(&self) -> bool {
        true
    }

    async fn create_native_chat_completion(
        &self,
        mut request: crate::native_chat::NativeChatRequest,
    ) -> Result<BackendChatResponse, BackendError> {
        request.set_stream(false);
        request.translate_max_tokens();
        normalize_native_response_format(&mut request)?;
        let response = self
            .send_request(self.native_chat_http_request(&request)?)
            .await?;
        if !response.status().is_success() {
            return Err(extract_upstream_error(response).await);
        }
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/json")
            .to_owned();
        let headers = forwardable_upstream_headers(response.headers());
        let text = response.text().await?;
        let completion = serde_json::from_str(&text)
            .map_err(|e| BackendError::Other(format!("invalid Chat completion: {e}")))?;
        let body = serde_json::value::RawValue::from_string(text)
            .map_err(|e| BackendError::Other(e.to_string()))?;
        Ok(BackendChatResponse {
            body,
            completion,
            status,
            content_type,
            headers,
        })
    }

    async fn create_native_chat_completion_stream(
        &self,
        mut request: crate::native_chat::NativeChatRequest,
    ) -> Result<BackendSseResponse, BackendError> {
        request.set_stream(true);
        request.translate_max_tokens();
        normalize_native_response_format(&mut request)?;
        if self.requests_stream_usage() {
            request.request_usage()?;
        }
        let response = self
            .send_request(self.native_chat_http_request(&request)?)
            .await?;
        if !response.status().is_success() {
            return Err(extract_upstream_error(response).await);
        }
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("text/event-stream")
            .to_owned();
        let headers = forwardable_upstream_headers(response.headers());
        Ok(BackendSseResponse {
            stream: Box::pin(
                response
                    .bytes_stream()
                    .map(|result| result.map_err(BackendError::from)),
            ),
            headers,
            status,
            content_type,
        })
    }

    async fn create_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        let url = format!("{}/chat/completions", self.base_url);

        // Always use non-streaming mode when calling OpenAI since we convert to SSE on our end
        let mut request = request;
        request.stream = Some(false);
        normalize_strict_response_format(&mut request)?;

        // Newer OpenAI models reject max_tokens and require max_completion_tokens.
        // Translate automatically so callers don't have to care.
        if request.max_tokens.is_some() && request.max_completion_tokens.is_none() {
            request.max_completion_tokens = request.max_tokens.take();
        }

        tracing::debug!("Sending request to OpenAI");

        let response = self
            .send_request(
                self.client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .header("Content-Type", "application/json")
                    .json(&request),
            )
            .await?;

        let status = response.status();
        tracing::debug!(status = %status, "Received response from OpenAI");

        self.handle_response(response).await
    }

    async fn create_chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<SseByteStream, BackendError> {
        let url = format!("{}/chat/completions", self.base_url);

        let mut request = request;
        request.stream = Some(true);
        normalize_strict_response_format(&mut request)?;

        // Newer OpenAI models reject max_tokens and require max_completion_tokens.
        if request.max_tokens.is_some() && request.max_completion_tokens.is_none() {
            request.max_completion_tokens = request.max_tokens.take();
        }

        let payload = self.stream_request_payload(request)?;

        tracing::debug!("Sending streaming request to OpenAI");

        let response = self
            .send_request(
                self.client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .header("Content-Type", "application/json")
                    .body(payload),
            )
            .await?;

        let status = response.status();
        if !status.is_success() {
            return Err(extract_upstream_error(response).await);
        }

        // Stream the raw bytes from the upstream response
        let byte_stream = response
            .bytes_stream()
            .map(|result| result.map_err(BackendError::from));

        Ok(Box::pin(byte_stream))
    }

    async fn create_embedding(
        &self,
        request: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        let url = format!("{}/embeddings", self.base_url);

        let response = self
            .send_request(
                self.client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .header("Content-Type", "application/json")
                    .json(&request),
            )
            .await?;

        self.handle_response(response).await
    }

    /// Send an image request, converting Gemini catalog resource names to the
    /// bare model IDs its images endpoint requires without changing public IDs.
    async fn create_image(
        &self,
        mut request: CreateImageRequest,
    ) -> Result<ImageResponse, BackendError> {
        if self.normalize_gemini_image_models {
            request.model = request
                .model
                .map(|model| model.strip_prefix("models/").unwrap_or(&model).to_owned());
        }
        let url = format!("{}/images/generations", self.base_url);

        let response = self
            .send_request(
                self.client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .header("Content-Type", "application/json")
                    .json(&request),
            )
            .await?;

        self.handle_response(response).await
    }

    async fn create_speech(
        &self,
        request: CreateSpeechRequest,
    ) -> Result<bytes::Bytes, BackendError> {
        let url = format!("{}/audio/speech", self.base_url);

        let response = self
            .send_request(
                self.client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .header("Content-Type", "application/json")
                    .json(&request),
            )
            .await?;

        if response.status().is_success() {
            response.bytes().await.map_err(BackendError::from)
        } else {
            Err(extract_upstream_error(response).await)
        }
    }

    async fn create_transcription(
        &self,
        model: String,
        file_bytes: bytes::Bytes,
        file_name: String,
        language: Option<String>,
        prompt: Option<String>,
        response_format: Option<String>,
        temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        let url = format!("{}/audio/transcriptions", self.base_url);

        let file_part = reqwest::multipart::Part::bytes(file_bytes.to_vec())
            .file_name(file_name)
            .mime_str("application/octet-stream")
            .map_err(|e| BackendError::Other(format!("Failed to create multipart part: {}", e)))?;

        let mut form = reqwest::multipart::Form::new()
            .text("model", model)
            .part("file", file_part);

        if let Some(lang) = language {
            form = form.text("language", lang);
        }
        if let Some(p) = prompt {
            form = form.text("prompt", p);
        }
        if let Some(rf) = response_format {
            form = form.text("response_format", rf);
        }
        if let Some(t) = temperature {
            form = form.text("temperature", t.to_string());
        }

        let response = self
            .send_request(
                self.client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .multipart(form),
            )
            .await?;

        self.handle_response(response).await
    }

    async fn create_translation(
        &self,
        model: String,
        file_bytes: bytes::Bytes,
        file_name: String,
        prompt: Option<String>,
        response_format: Option<String>,
        temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        let url = format!("{}/audio/translations", self.base_url);

        let file_part = reqwest::multipart::Part::bytes(file_bytes.to_vec())
            .file_name(file_name)
            .mime_str("application/octet-stream")
            .map_err(|e| BackendError::Other(format!("Failed to create multipart part: {}", e)))?;

        let mut form = reqwest::multipart::Form::new()
            .text("model", model)
            .part("file", file_part);

        if let Some(p) = prompt {
            form = form.text("prompt", p);
        }
        if let Some(rf) = response_format {
            form = form.text("response_format", rf);
        }
        if let Some(t) = temperature {
            form = form.text("temperature", t.to_string());
        }

        let response = self
            .send_request(
                self.client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .multipart(form),
            )
            .await?;

        self.handle_response(response).await
    }

    async fn create_moderation(
        &self,
        request: CreateModerationRequest,
    ) -> Result<CreateModerationResponse, BackendError> {
        let url = format!("{}/moderations", self.base_url);

        let response = self
            .send_request(
                self.client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .header("Content-Type", "application/json")
                    .json(&request),
            )
            .await?;

        self.handle_response(response).await
    }

    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        let mut listing = self.fetch_model_list("").await?;
        // OpenRouter's default catalog omits decision models; they are listed
        // only on request. A failed decisions listing must not hide the rest.
        if self.provider_name == "openrouter" {
            match self.fetch_model_list("?output_modalities=decisions").await {
                Ok(decisions) => {
                    let mut seen: std::collections::HashSet<String> =
                        listing.data.iter().map(|model| model.id.clone()).collect();
                    listing.data.extend(
                        decisions
                            .data
                            .into_iter()
                            .filter(|model| seen.insert(model.id.clone())),
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, "OpenRouter decision-model listing failed");
                }
            }
        }
        Ok(listing)
    }

    async fn get_model(&self, model_id: &str) -> Result<Model, BackendError> {
        let url = format!("{}/models/{}", self.base_url, model_id);

        let response = self
            .send_request(
                self.client
                    .get(&url)
                    .header("Authorization", format!("Bearer {}", self.api_key)),
            )
            .await?;

        let model: Model = self.handle_response(response).await?;
        if let Some(reason) = unavailable_reason(&model) {
            // Keep the per-id endpoint consistent with the listing, which
            // omits entries the provider reports as unservable.
            return Err(BackendError::api_status(
                404,
                format!("model '{model_id}' is not available: {reason}"),
            ));
        }
        Ok(model)
    }

    fn name(&self) -> &str {
        &self.provider_name
    }
}

#[cfg(test)]
mod tests {
    use super::{CompatibleModelList, OpenAICompatibleBackend};

    #[test]
    fn strict_normalization_only_changes_applicable_response_format() {
        let mut request: crate::native_chat::NativeChatRequest =
            serde_json::from_value(serde_json::json!({
                "model": "test", "messages": [{"role": "user", "content": "prompt"}],
                "future_control": {"preserved": true},
                "response_format": {"type": "json_schema", "json_schema": {
                    "name": "answer", "strict": true,
                    "schema": {"type": "object", "properties": {"answer": {"type": "string"}}}
                }}
            }))
            .unwrap();
        let original = serde_json::to_value(&request).unwrap();
        assert!(
            super::normalized_strict_response_format(request.response_format.as_ref(), false)
                .unwrap()
                .is_none()
        );
        assert!(super::normalized_strict_response_format(None, true)
            .unwrap()
            .is_none());
        let ordinary: crate::schema_json::SchemaJson =
            serde_json::from_value(serde_json::json!({"type":"json_object"})).unwrap();
        assert!(
            super::normalized_strict_response_format(Some(&ordinary), true)
                .unwrap()
                .is_none()
        );
        let normalized =
            super::normalized_strict_response_format(request.response_format.as_ref(), true)
                .unwrap()
                .unwrap();
        request.set_field("response_format", &normalized).unwrap();
        let changed = serde_json::to_value(&request).unwrap();
        for (name, value) in original.as_object().unwrap() {
            if name != "response_format" {
                assert_eq!(&changed[name], value, "{name}");
            }
        }
        let schema = &changed["response_format"]["json_schema"]["schema"];
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], serde_json::json!(["answer"]));
    }

    #[test]
    fn unavailable_catalog_entries_are_dropped_in_both_envelopes() {
        let entries = serde_json::json!([
            {"id": "served", "object": "model"},
            {"id": "no-status", "object": "model", "status": null},
            {"id": "blocked", "object": "model", "status": "unavailable", "status_reason": "retention"}
        ]);
        for listing in [
            serde_json::json!({"object": "list", "data": entries.clone()}),
            entries,
        ] {
            let ids: Vec<_> = serde_json::from_value::<super::CompatibleModelList>(
                super::without_unavailable_models(listing),
            )
            .unwrap()
            .into_list_response()
            .data
            .into_iter()
            .map(|model| model.id)
            .collect();
            assert_eq!(ids, ["served", "no-status"]);
        }
    }

    #[test]
    fn unavailable_reason_reads_the_provider_status_marker() {
        let model = |value: serde_json::Value| -> crate::types::Model {
            serde_json::from_value(value).unwrap()
        };
        assert_eq!(
            super::unavailable_reason(&model(serde_json::json!({
                "id": "blocked", "object": "model",
                "status": "unavailable", "status_reason": "retention mode"
            }))),
            Some("retention mode".to_string())
        );
        assert_eq!(
            super::unavailable_reason(&model(serde_json::json!({
                "id": "blocked", "object": "model", "status": "unavailable"
            }))),
            Some("the provider reports it as unavailable".to_string())
        );
        for value in [
            serde_json::json!({"id": "ok", "object": "model"}),
            serde_json::json!({"id": "ok", "object": "model", "status": "available"}),
            serde_json::json!({"id": "ok", "object": "model", "status": null}),
        ] {
            assert_eq!(super::unavailable_reason(&model(value)), None);
        }
    }
    use crate::backend::provider::{
        AnthropicRequestEndpoint, AnthropicRequestHeaders, ResponsesRequestHeaders,
    };
    use crate::responses::ResponsesRequest;
    use crate::types::ChatCompletionRequest;

    #[test]
    fn normalizes_wrapped_and_bare_model_lists() {
        for (json, expected_id) in [
            (
                r#"{"object":"list","data":[{"id":"wrapped","object":"model"}]}"#,
                "wrapped",
            ),
            (r#"[{"id":"bare","object":"model"}]"#, "bare"),
        ] {
            let response = serde_json::from_str::<CompatibleModelList>(json)
                .unwrap()
                .into_list_response();

            assert_eq!(response.object, "list");
            assert_eq!(response.data.len(), 1);
            assert_eq!(response.data[0].id, expected_id);
            assert_eq!(response.data[0].created, 0);
            assert!(response.data[0].owned_by.is_empty());
        }
    }

    fn streaming_request() -> ChatCompletionRequest {
        serde_json::from_value(serde_json::json!({
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true,
        }))
        .unwrap()
    }

    #[test]
    fn native_openai_streams_request_usage() {
        let backend = OpenAICompatibleBackend::new(reqwest::Client::new(), "test-key".to_string());

        let payload = backend.stream_request_payload(streaming_request()).unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();

        assert_eq!(
            payload["stream_options"]["include_usage"],
            serde_json::json!(true),
        );
    }

    #[test]
    fn owned_stream_payload_preserves_request_fields_and_provider_usage_rules() {
        let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "model", "stream": true, "max_completion_tokens": 17, "n": 2,
            "messages": [{"role": "user", "content": "雪🦀\n"}],
            "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {
                "type": "object", "properties": {"city": {"type": "string"}}
            }}}],
            "tool_choice": "auto", "parallel_tool_calls": false,
            "stream_options": {"include_usage": false}
        }))
        .unwrap();
        for (kind, include_usage) in [("openai", true), ("openai-compatible", false)] {
            let backend = OpenAICompatibleBackend::for_provider(
                reqwest::Client::new(),
                kind,
                "key".into(),
                "https://example.com/v1".into(),
            );
            let mut expected = serde_json::to_value(&request).unwrap();
            if include_usage {
                expected["stream_options"] = serde_json::json!({"include_usage": true});
            } else {
                expected.as_object_mut().unwrap().remove("stream_options");
            }
            let actual: serde_json::Value =
                serde_json::from_slice(&backend.stream_request_payload(request.clone()).unwrap())
                    .unwrap();
            assert_eq!(actual, expected, "{kind}");
        }
    }

    #[test]
    fn streaming_serialization_preserves_schema_after_opt_in_normalization() {
        let backend = OpenAICompatibleBackend::new(reqwest::Client::new(), "key".to_string());
        let mut request: ChatCompletionRequest = serde_json::from_str(r#"{"model":"gpt-4o","messages":[],"response_format":{"type":"json_schema","json_schema":{"strict":true,"schema":{"properties":{"z":{"type":"object","properties":{"yy":{},"bb":{}}},"a":{"type":"array","items":{"properties":{"q":{},"c":{}}}}}}}}}"#).unwrap();
        request
            .response_format
            .as_mut()
            .unwrap()
            .normalize_response_format()
            .unwrap();
        let wire = String::from_utf8(backend.stream_request_payload(request).unwrap()).unwrap();
        assert!(wire.contains(r#""required":["z","a"]"#));
        assert!(wire.contains(r#""properties":{"yy":{},"bb":{}}"#));
        assert!(wire.contains(r#""properties":{"q":{},"c":{}}"#));
        assert!(wire.contains(r#""required":["q","c"]"#));
    }

    #[test]
    fn arbitrary_compatible_provider_does_not_receive_openai_stream_options() {
        let backend = OpenAICompatibleBackend::for_provider(
            reqwest::Client::new(),
            "compatible",
            "test-key".to_string(),
            "https://example.com/v1".to_string(),
        );

        let payload = backend.stream_request_payload(streaming_request()).unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();

        assert!(payload.get("stream_options").is_none());
    }

    #[test]
    fn supported_builtin_compatible_provider_requests_stream_usage() {
        let backend = OpenAICompatibleBackend::for_provider(
            reqwest::Client::new(),
            "openrouter",
            "key".to_string(),
            "https://example.com/v1".to_string(),
        );
        let payload = backend.stream_request_payload(streaming_request()).unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(
            payload["stream_options"]["include_usage"],
            serde_json::Value::Bool(true)
        );
    }

    fn responses_request(value: serde_json::Value) -> ResponsesRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn responses_requests_preserve_fields_for_capability_selected_dispatch() {
        let request = responses_request(serde_json::json!({
            "model": "gpt-5-codex",
            "input": {"future_native_shape": true},
            "reasoning": {"effort": "high"},
            "future_native_field": {"preserved": true},
        }));

        let serialized = serde_json::to_value(request).unwrap();
        assert_eq!(serialized["future_native_field"]["preserved"], true);
    }

    #[test]
    fn response_item_ids_are_encoded_as_one_path_segment() {
        let backend = OpenAICompatibleBackend::with_base_url(
            reqwest::Client::new(),
            "test-key".to_string(),
            "https://example.com/v1".to_string(),
        );
        assert_eq!(
            backend
                .response_item_url("resp/with spaces")
                .unwrap()
                .as_str(),
            "https://example.com/v1/responses/resp%2Fwith%20spaces"
        );
    }

    #[test]
    fn response_lifecycle_urls_preserve_id_encoding_and_resume_cursor() {
        let backend = OpenAICompatibleBackend::with_base_url(
            reqwest::Client::new(),
            "test-key".to_string(),
            "https://example.com/v1".to_string(),
        );
        assert_eq!(
            backend
                .response_stream_url("resp/with spaces", Some(42))
                .unwrap()
                .as_str(),
            "https://example.com/v1/responses/resp%2Fwith%20spaces?stream=true&starting_after=42"
        );
        assert_eq!(
            backend
                .response_action_url("resp/with spaces", "cancel")
                .unwrap()
                .as_str(),
            "https://example.com/v1/responses/resp%2Fwith%20spaces/cancel"
        );
    }

    #[test]
    fn anthropic_compatible_endpoints_use_configured_auth_and_paths() {
        let bearer = OpenAICompatibleBackend::for_provider(
            reqwest::Client::new(),
            "openrouter",
            "bearer-key".to_string(),
            "https://openrouter.ai/api/v1".to_string(),
        )
        .with_anthropic_base_url("https://openrouter.ai/api/v1");
        let request = bearer
            .anthropic_request(
                AnthropicRequestEndpoint::Messages,
                &AnthropicRequestHeaders::default(),
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://openrouter.ai/api/v1/messages"
        );
        assert_eq!(request.headers()["authorization"], "Bearer bearer-key");

        let bedrock = OpenAICompatibleBackend::for_provider(
            reqwest::Client::new(),
            "bedrock",
            "bedrock-key".to_string(),
            "https://bedrock-mantle.us-east-1.api.aws/v1".to_string(),
        )
        .with_anthropic_x_api_key_base_url("https://bedrock-mantle.us-east-1.api.aws/anthropic/v1");
        let request = bedrock
            .anthropic_request(
                AnthropicRequestEndpoint::CountTokens,
                &AnthropicRequestHeaders::default(),
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages/count_tokens"
        );
        assert_eq!(request.headers()["x-api-key"], "bedrock-key");
        assert!(!request.headers().contains_key("authorization"));
    }

    #[test]
    fn decisions_endpoints_follow_each_provider_contract() {
        use super::DecisionsWire;
        use crate::decisions::DecisionsIngress;
        let backend = |provider: &str, base: &str| {
            OpenAICompatibleBackend::for_provider(
                reqwest::Client::new(),
                provider,
                "key".to_string(),
                base.to_string(),
            )
        };
        let openrouter = backend("openrouter", "https://openrouter.ai/api/v1/");
        for (ingress, url) in [
            (
                DecisionsIngress::Alpha,
                "https://openrouter.ai/api/alpha/decisions",
            ),
            (
                DecisionsIngress::OpenAi,
                "https://openrouter.ai/api/alpha/decisions",
            ),
            (
                DecisionsIngress::SystemOne,
                "https://openrouter.ai/api/v1/systemone",
            ),
        ] {
            assert_eq!(
                openrouter.decisions_endpoint(ingress).unwrap(),
                (url.to_string(), DecisionsWire::OpenRouter)
            );
        }
        assert_eq!(
            backend("openai", "https://api.openai.com/v1")
                .decisions_endpoint(DecisionsIngress::Alpha)
                .unwrap(),
            (
                "https://api.openai.com/v1/decisions".to_string(),
                DecisionsWire::OpenAi
            )
        );
        for base in ["https://api.perplexity.ai", "https://api.perplexity.ai/v1/"] {
            assert_eq!(
                backend("perplexity", base)
                    .decisions_endpoint(DecisionsIngress::Alpha)
                    .unwrap(),
                (
                    "https://api.perplexity.ai/v1/decisions".to_string(),
                    DecisionsWire::Perplexity
                )
            );
        }
        assert!(matches!(
            backend("openrouter", "https://openrouter.example/v1")
                .decisions_endpoint(DecisionsIngress::Alpha),
            Err(crate::backend::BackendError::ConfigError(_))
        ));
        assert!(backend("groq", "https://api.groq.com/openai/v1")
            .decisions_endpoint(DecisionsIngress::Alpha)
            .is_err());
    }

    #[test]
    fn decisions_bodies_keep_extras_on_their_own_wire_and_inline_perplexity_images() {
        use super::DecisionsWire;
        let mut request: crate::decisions::DecisionsRequest =
            serde_json::from_value(serde_json::json!({
                "model": "pplx-decider-v1.2",
                "state": {"camera": "img-1", "note": "front door"},
                "assets": {"img-1": {"type": "image", "format": "png", "data_base64": "AAAA"}},
                "questions": {"person": {"type": "noul", "instructions": "Person?"}},
                "session_id": "s-1",
                "provider": {"order": ["TypeSafe"]}
            }))
            .unwrap();
        let openrouter =
            OpenAICompatibleBackend::decisions_body(&request, DecisionsWire::OpenRouter).unwrap();
        assert_eq!(openrouter["provider"]["order"][0], "TypeSafe");
        assert_eq!(openrouter["session_id"], "s-1");
        assert_eq!(openrouter["assets"]["img-1"]["format"], "png");

        let perplexity =
            OpenAICompatibleBackend::decisions_body(&request, DecisionsWire::Perplexity).unwrap();
        assert_eq!(
            perplexity,
            serde_json::json!({
                "model": "pplx-decider-v1.2",
                "state": {
                    "camera": {"type": "image_url",
                               "image_url": {"url": "data:image/png;base64,AAAA"}},
                    "note": "front door"
                },
                "questions": {"person": {"type": "noul", "instructions": "Person?"}}
            })
        );

        request.ingress = crate::decisions::DecisionsIngress::OpenAi;
        let openrouter =
            OpenAICompatibleBackend::decisions_body(&request, DecisionsWire::OpenRouter).unwrap();
        assert!(openrouter.get("provider").is_none());
    }

    #[test]
    fn responses_request_uses_backend_auth_and_only_sanitized_caller_headers() {
        let backend =
            OpenAICompatibleBackend::new(reqwest::Client::new(), "backend-secret".to_string());
        let mut caller = http::HeaderMap::new();
        caller.insert("authorization", "Bearer caller-secret".parse().unwrap());
        caller.insert("cookie", "session=secret".parse().unwrap());
        caller.insert("x-client-request-id", "client_123".parse().unwrap());
        let sanitized = ResponsesRequestHeaders::from_header_map(&caller);

        let request = backend
            .responses_request(
                reqwest::Method::POST,
                "https://api.openai.com/v1/responses",
                &sanitized,
            )
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(request.headers()["authorization"], "Bearer backend-secret");
        assert_eq!(request.headers()["x-client-request-id"], "client_123");
        assert!(request.headers().get("cookie").is_none());
    }
}
