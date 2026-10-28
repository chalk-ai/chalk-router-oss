//! Vertex AI (Google Agent Platform, fka Vertex) backend.
//!
//! The enterprise surface for Gemini, and a different product from the Gemini
//! Developer API that the `gemini/` connection kind serves: models live under a
//! GCP project and region, and requests authenticate with a short-lived OAuth2
//! token minted from a service account key rather than a standing API key. The
//! stored credential for a `vertex` connection is that key JSON; see
//! [`super::gcp_auth`] for the minting side.
//!
//! Chat completions use Vertex's OpenAI-compatible endpoint, so request and SSE
//! response shapes are the ones the rest of the router already speaks.
//! Embeddings have no OpenAI-compatible route on Vertex and are translated to
//! and from the native `:predict` call, except Gemini Embedding 2, which
//! rejects `:predict` and is served through `:embedContent` instead.

use std::sync::Arc;

use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use reqwest::Client;
use serde::Deserialize;

use crate::embedding_limits::gemini_embedding_2_limits;
use crate::types::{
    ChatCompletionRequest, ChatCompletionResponse, CreateEmbeddingRequest, CreateEmbeddingResponse,
    CreateImageRequest, CreateModerationRequest, CreateModerationResponse, CreateSpeechRequest,
    Embedding, EmbeddingData, EmbeddingInput, ImageResponse, ListResponse, Model,
    TranscriptionResponse, Usage,
};

use super::gcp_auth::{GcpCredentials, GcpTokenSource};
use super::google_embedding::prepare_embedding;
use super::provider::{
    forwardable_upstream_headers, AnthropicRequestEndpoint, AnthropicRequestHeaders, BackendError,
    BackendJsonResponse, BackendProvider, SseByteStream,
};
use crate::translation::gemini_embeddings::{
    embedding_response, google_error_message, EmbeddingResponse,
};

/// Region used when neither the credential nor a base-URL override names one.
/// Every Gemini model is served here.
pub const DEFAULT_LOCATION: &str = "us-central1";

/// The endpoint a connection defaults to, shown in the Providers UI. Overriding
/// it is how an operator moves the connection to another region.
pub const VERTEX_DEFAULT_BASE_URL: &str = "https://us-central1-aiplatform.googleapis.com";

/// Publisher applied to bare model names, so `gemini-2.5-pro` reaches Vertex as
/// `google/gemini-2.5-pro`. Partner models carry their own (`meta/llama-...`)
/// and are passed through untouched.
const DEFAULT_PUBLISHER: &str = "google";

/// Page size for the publisher catalog. 300 is the API's maximum — it rejects
/// anything larger — and the catalog is ~130 models today, so this is normally
/// one request.
const MODEL_PAGE_SIZE: u32 = 300;

/// Pages to follow before giving up, so a paging bug upstream cannot spin the
/// model listing forever. Far above the real catalog size.
const MAX_MODEL_PAGES: usize = 10;

/// Keep the hint to particularly constrained budgets; 512 tokens is a
/// diagnostic heuristic, not a provider limit.
const LOW_MAX_TOKENS_HINT_THRESHOLD: i32 = 512;

/// `:embedContent` takes one document per call, so a text batch fans out to
/// one upstream call per input. The router's rate limits count the batch as
/// one request, so the batch size is capped rather than left at OpenAI's 2,048,
/// and one failed call fails the whole batch.
const MAX_EMBED_CONTENT_BATCH: usize = 100;

/// Calls in flight for one batch. Per request, not per connection: concurrent
/// batches multiply it.
const MAX_CONCURRENT_EMBED_CONTENT: usize = 8;

/// A Vertex AI connection: credentials, project, and region.
pub struct VertexBackend {
    client: Client,
    tokens: Arc<GcpTokenSource>,
    project_id: String,
    location: String,
    /// API host, without a trailing slash.
    host: String,
}

/// Build a Vertex backend from a stored provider connection.
///
/// `credential` is the service account key JSON (raw or base64). The project
/// comes from the key itself — a key is always issued for one project — and the
/// region from, in order: an explicit `location` the operator added to the key
/// JSON, the region named by a `base_url` override, else [`DEFAULT_LOCATION`].
pub fn vertex_backend(
    http_client: Client,
    credential: &str,
    base_url_override: Option<&str>,
) -> Result<VertexBackend, BackendError> {
    let credentials = GcpCredentials::parse(credential)?;

    let project_id = credentials
        .project_id()
        .map(str::to_string)
        .ok_or_else(|| {
            BackendError::ConfigError(
                "Vertex credential names no project: add \"project_id\" to the service \
                 account key JSON"
                    .to_string(),
            )
        })?;

    let base_override = base_url_override
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(|url| url.trim_end_matches('/'));

    let location = credentials
        .location()
        .map(str::to_string)
        .or_else(|| base_override.and_then(location_from_host))
        .unwrap_or_else(|| DEFAULT_LOCATION.to_string());

    let host = base_override
        .map(str::to_string)
        .unwrap_or_else(|| host_for_location(&location));

    Ok(VertexBackend {
        client: http_client.clone(),
        tokens: Arc::new(GcpTokenSource::new(http_client, credentials)),
        project_id,
        location,
        host,
    })
}

/// The regional API host for `location`. The `global` endpoint alone has no
/// region prefix.
pub fn host_for_location(location: &str) -> String {
    if location == "global" {
        "https://aiplatform.googleapis.com".to_string()
    } else {
        format!("https://{location}-aiplatform.googleapis.com")
    }
}

/// Recover the region from an API host, so an operator who moves the connection
/// by editing its base URL does not also have to restate the region.
pub fn location_from_host(host: &str) -> Option<String> {
    let rest = host
        .trim()
        .trim_end_matches('/')
        .strip_prefix("https://")
        .or_else(|| host.trim().trim_end_matches('/').strip_prefix("http://"))?;
    let authority = rest.split('/').next()?;
    // Data-residency endpoints put the location in the middle:
    // `aiplatform.us.rep.googleapis.com` serves `locations/us`.
    let residency = authority
        .strip_prefix("aiplatform.")
        .and_then(|rest| rest.strip_suffix(".rep.googleapis.com"));
    let bare = residency.or_else(|| authority.strip_suffix("-aiplatform.googleapis.com"));
    match bare {
        Some(location) if !location.is_empty() && !location.contains('.') => {
            Some(location.to_string())
        }
        // The unprefixed host is the global endpoint.
        _ if authority == "aiplatform.googleapis.com" => Some("global".to_string()),
        _ => None,
    }
}

impl std::fmt::Debug for VertexBackend {
    /// Only the non-secret routing identity; the credential lives behind
    /// [`GcpTokenSource`]'s own redacting `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VertexBackend")
            .field("project_id", &self.project_id)
            .field("location", &self.location)
            .field("host", &self.host)
            .field("credentials", &self.tokens.credentials().kind())
            .finish()
    }
}

impl VertexBackend {
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn location(&self) -> &str {
        &self.location
    }

    /// Base for the OpenAI-compatible surface (chat completions).
    fn openai_base(&self) -> String {
        format!(
            "{}/v1beta1/projects/{}/locations/{}/endpoints/openapi",
            self.host, self.project_id, self.location
        )
    }

    /// Base for native publisher-model calls (`:predict`).
    fn publisher_base(&self, publisher: &str) -> String {
        format!(
            "{}/v1/projects/{}/locations/{}/publishers/{}",
            self.host, self.project_id, self.location, publisher
        )
    }

    /// `gemini-2.5-pro` → `google/gemini-2.5-pro`; `meta/llama-x` unchanged.
    pub fn qualify_model(model: &str) -> String {
        if model.contains('/') {
            model.to_string()
        } else {
            format!("{DEFAULT_PUBLISHER}/{model}")
        }
    }

    /// Split a possibly-qualified model into (publisher, bare model name).
    pub fn split_model(model: &str) -> (String, String) {
        match model.split_once('/') {
            Some((publisher, name)) => (publisher.to_string(), name.to_string()),
            None => (DEFAULT_PUBLISHER.to_string(), model.to_string()),
        }
    }

    /// Attach the minted bearer token, plus the quota project when the
    /// credential is a user credential (see [`GcpCredentials::quota_project`]).
    async fn authorize(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, BackendError> {
        let token = self.tokens.token().await?;
        let mut request = request.header("Authorization", format!("Bearer {token}"));
        if let Some(project) = self.tokens.credentials().quota_project() {
            request = request.header("x-goog-user-project", project);
        }
        Ok(request)
    }

    fn unsupported(op: &str) -> BackendError {
        BackendError::api_status(
            400,
            format!("Vertex AI connections do not serve {op}; route it to another provider"),
        )
    }

    /// Read `model` out of an Anthropic request body (removing it), or a 400
    /// if it is missing or not a string. Shared by both endpoint branches
    /// below, which each put the bare name somewhere different.
    fn require_model(
        object: &mut serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, BackendError> {
        object
            .remove("model")
            .and_then(|value| value.as_str().map(str::to_string))
            .ok_or_else(|| BackendError::api_status(400, "Anthropic request requires model"))
    }

    /// Adapt an Anthropic Messages- or count-tokens-shaped body to Vertex's
    /// `rawPredict` contract and compute the target URL. Messages force-sets
    /// `anthropic_version` to `vertex-2023-10-16` (Anthropic's Vertex guide)
    /// and moves the bare model name into the URL. Count-tokens isn't
    /// documented there, but both official Anthropic SDKs setdefault that
    /// same `anthropic_version` into count_tokens bodies too and leave the
    /// bare model name in the body rather than the URL; we match that.
    /// (Unverified: Vertex has been reported to reject an `@default` model
    /// suffix for count-tokens, so we only strip the publisher prefix here,
    /// not suffixes.)
    fn prepare_anthropic_request(
        &self,
        endpoint: AnthropicRequestEndpoint,
        body: &[u8],
    ) -> Result<(String, serde_json::Value), BackendError> {
        let mut body: serde_json::Value = serde_json::from_slice(body).map_err(|error| {
            BackendError::api_status(400, format!("invalid Anthropic request JSON: {error}"))
        })?;
        let object = body.as_object_mut().ok_or_else(|| {
            BackendError::api_status(400, "Anthropic request body must be a JSON object")
        })?;
        let url = match endpoint {
            AnthropicRequestEndpoint::Messages => {
                object.insert(
                    "anthropic_version".to_string(),
                    serde_json::Value::String("vertex-2023-10-16".to_string()),
                );
                let model = Self::require_model(object)?;
                let (_, model) = Self::split_model(&model);
                let operation =
                    if object.get("stream").and_then(serde_json::Value::as_bool) == Some(true) {
                        "streamRawPredict"
                    } else {
                        "rawPredict"
                    };
                format!(
                    "{}/models/{model}:{operation}",
                    self.publisher_base("anthropic")
                )
            }
            AnthropicRequestEndpoint::CountTokens => {
                object
                    .entry("anthropic_version")
                    .or_insert_with(|| serde_json::Value::String("vertex-2023-10-16".to_string()));
                let model = Self::require_model(object)?;
                let (_, model) = Self::split_model(&model);
                object.insert("model".to_string(), serde_json::Value::String(model));
                format!(
                    "{}/models/count-tokens:rawPredict",
                    self.publisher_base("anthropic")
                )
            }
        };
        Ok((url, body))
    }

    /// Turn a non-2xx response into a [`BackendError`].
    ///
    /// Vertex answers with a Google API error (`{"error": {"message", "status",
    /// "code"}}`) on both of its surfaces. `code` is a number there and a string
    /// in OpenAI's shape, so the message is read out of the JSON directly rather
    /// than through `types::ErrorResponse`.
    async fn upstream_error(response: reqwest::Response) -> BackendError {
        super::provider::upstream_error(response, |text| {
            serde_json::from_str::<serde_json::Value>(text)
                .ok()
                .and_then(|body| {
                    body.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                })
        })
        .await
    }

    /// Gemini Embedding 2 through `:embedContent`. The model answers `:predict`
    /// with `FAILED_PRECONDITION`, and only `:embedContent` accepts image parts
    /// or fuses a multimodal document into one vector.
    async fn create_gemini_embedding(
        &self,
        request: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        let prepared = prepare_embedding(&request).await?;
        let expected_count = prepared.contents.len();
        if expected_count > MAX_EMBED_CONTENT_BATCH {
            return Err(BackendError::api_status(
                400,
                format!(
                    "Vertex Gemini Embedding 2 accepts at most {MAX_EMBED_CONTENT_BATCH} inputs \
                     per request; split the batch"
                ),
            ));
        }
        let (publisher, model) = Self::split_model(&request.model);
        let url = format!(
            "{}/models/{model}:embedContent",
            self.publisher_base(&publisher)
        );
        let responses: Vec<EmbeddingResponse> =
            futures::stream::iter(prepared.contents.into_iter().map(|content| {
                let mut body = serde_json::json!({ "content": content });
                if let Some(dimensions) = request.dimensions {
                    body["outputDimensionality"] = serde_json::json!(dimensions);
                }
                self.embed_content(&url, body)
            }))
            .buffered(MAX_CONCURRENT_EMBED_CONTENT)
            .try_collect()
            .await?;

        let mut embeddings = Vec::with_capacity(responses.len());
        let mut usages = Vec::with_capacity(responses.len());
        for response in responses {
            usages.push(
                crate::translation::gemini_embeddings::observe_embedding_usage(
                    response.usage_metadata.as_ref(),
                    prepared.has_images,
                )
                .map_err(|message| BackendError::Other(message.into()))?,
            );
            embeddings.push(response.embedding.ok_or_else(|| {
                BackendError::Other("Vertex response omitted the embedding".into())
            })?);
        }
        let usage = if prepared.batch {
            sum_text_usage(usages)
        } else {
            usages.into_iter().next().flatten()
        };
        embedding_response(
            embeddings,
            expected_count,
            prepared.base64,
            request.model,
            usage,
        )
    }

    async fn embed_content(
        &self,
        url: &str,
        body: serde_json::Value,
    ) -> Result<EmbeddingResponse, BackendError> {
        let response = self
            .authorize(self.client.post(url))
            .await?
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(super::provider::upstream_error(response, google_error_message).await);
        }
        response
            .json()
            .await
            .map_err(|e| BackendError::Other(format!("Failed to parse Vertex embedding: {e}")))
    }

    /// Shared preparation for both chat paths: qualify the model and fold
    /// `max_completion_tokens` back down, since Vertex's compatibility layer
    /// takes OpenAI's original `max_tokens`.
    fn prepare_chat(request: &mut ChatCompletionRequest, stream: bool) {
        request.model = Self::qualify_model(&request.model);
        request.stream = Some(stream);
        if request.max_completion_tokens.is_some() && request.max_tokens.is_none() {
            request.max_tokens = request.max_completion_tokens.take();
        }
    }
}

/// One total across a fanned-out text batch, billed at the text rate. Usage
/// that is missing for any call is missing for the batch: a partial sum would
/// undercount.
fn sum_text_usage(usages: Vec<Option<Usage>>) -> Option<Usage> {
    let tokens = usages.iter().try_fold(0i32, |sum, usage| {
        sum.checked_add(usage.as_ref()?.prompt_tokens)
    })?;
    Some(Usage {
        prompt_tokens: tokens,
        total_tokens: tokens,
        input_token_attribution: crate::pricing::InputTokenAttribution::TextRate,
        ..Default::default()
    })
}

/// `:predict` response for the text-embedding publisher models.
#[derive(Deserialize)]
struct PredictEmbeddingResponse {
    #[serde(default)]
    predictions: Vec<EmbeddingPrediction>,
}

#[derive(Deserialize)]
struct EmbeddingPrediction {
    embeddings: PredictedEmbedding,
}

#[derive(Deserialize)]
struct PredictedEmbedding {
    values: Vec<f32>,
    #[serde(default)]
    statistics: Option<EmbeddingStatistics>,
}

#[derive(Deserialize)]
struct EmbeddingStatistics {
    #[serde(default)]
    token_count: Option<f64>,
}

/// `publishers/google/models` listing (Model Garden).
#[derive(Deserialize)]
struct ListPublisherModelsResponse {
    #[serde(default, rename = "publisherModels")]
    publisher_models: Vec<PublisherModel>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct PublisherModel {
    /// e.g. `publishers/google/models/gemini-2.5-pro`
    #[serde(default)]
    name: String,
}

#[async_trait]
impl BackendProvider for VertexBackend {
    async fn count_anthropic_tokens(
        &self,
        body: bytes::Bytes,
    ) -> Result<BackendJsonResponse, BackendError> {
        let (url, body) =
            self.prepare_anthropic_request(AnthropicRequestEndpoint::CountTokens, &body)?;
        let response = self
            .authorize(self.client.post(url))
            .await?
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Self::upstream_error(response).await);
        }
        let headers = forwardable_upstream_headers(response.headers());
        BackendJsonResponse::from_value(response.json().await?, headers)
    }

    async fn send_anthropic_adapted_request(
        &self,
        endpoint: AnthropicRequestEndpoint,
        _headers: &AnthropicRequestHeaders,
        body: bytes::Bytes,
    ) -> Result<reqwest::Response, BackendError> {
        let (url, body) = self.prepare_anthropic_request(endpoint, &body)?;

        Ok(self
            .authorize(self.client.post(url))
            .await?
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?)
    }

    async fn create_chat_completion(
        &self,
        mut request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        Self::prepare_chat(&mut request, false);
        let max_tokens = request.max_tokens;
        let url = format!("{}/chat/completions", self.openai_base());

        let response = self
            .authorize(self.client.post(&url))
            .await?
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::upstream_error(response).await);
        }

        response
            .json::<ChatCompletionResponse>()
            .await
            .map_err(|error| {
                let message = format!("Failed to parse Vertex response: {error}");
                let message = if max_tokens
                    .is_some_and(|tokens| tokens <= LOW_MAX_TOKENS_HINT_THRESHOLD)
                {
                    format!(
                        "{message}. A low max_tokens value may be involved if a thinking model exhausts the output budget on internal reasoning; consider increasing it if appropriate."
                    )
                } else {
                    message
                };
                BackendError::Other(message)
            })
    }

    async fn create_chat_completion_stream(
        &self,
        mut request: ChatCompletionRequest,
    ) -> Result<SseByteStream, BackendError> {
        Self::prepare_chat(&mut request, true);
        let url = format!("{}/chat/completions", self.openai_base());

        let response = self
            .authorize(self.client.post(&url))
            .await?
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::upstream_error(response).await);
        }

        Ok(Box::pin(
            response
                .bytes_stream()
                .map(|r| r.map_err(BackendError::from)),
        ))
    }

    async fn create_embedding(
        &self,
        request: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        if gemini_embedding_2_limits(&Self::split_model(&request.model).1).is_some() {
            return self.create_gemini_embedding(request).await;
        }
        let texts: Vec<String> = match &request.input {
            EmbeddingInput::Single(text) => vec![text.clone()],
            EmbeddingInput::Multiple(texts) => texts.clone(),
            EmbeddingInput::Tokens(_) | EmbeddingInput::TokenBatch(_) => {
                return Err(Self::unsupported("token-array embeddings"))
            }
            EmbeddingInput::Multimodal(_) => {
                return Err(Self::unsupported("multimodal embeddings"))
            }
        };

        let (publisher, model) = Self::split_model(&request.model);
        let url = format!("{}/models/{model}:predict", self.publisher_base(&publisher));

        let mut body = serde_json::json!({
            "instances": texts
                .iter()
                .map(|text| serde_json::json!({ "content": text }))
                .collect::<Vec<_>>(),
        });
        if let Some(dimensions) = request.dimensions {
            body["parameters"] = serde_json::json!({ "outputDimensionality": dimensions });
        }

        let response = self
            .authorize(self.client.post(&url))
            .await?
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::upstream_error(response).await);
        }

        let parsed: PredictEmbeddingResponse = response
            .json()
            .await
            .map_err(|e| BackendError::Other(format!("Failed to parse Vertex embeddings: {e}")))?;

        // Vertex reports tokens per prediction; OpenAI's shape carries one total.
        let mut total_tokens = 0i32;
        let data = parsed
            .predictions
            .into_iter()
            .enumerate()
            .map(|(index, prediction)| {
                if let Some(count) = prediction
                    .embeddings
                    .statistics
                    .as_ref()
                    .and_then(|stats| stats.token_count)
                {
                    total_tokens += count as i32;
                }
                Embedding {
                    object: "embedding".to_string(),
                    index: index as i32,
                    embedding: EmbeddingData::Float(prediction.embeddings.values),
                }
            })
            .collect();

        Ok(CreateEmbeddingResponse {
            object: "list".to_string(),
            data,
            model: request.model,
            usage: Some(Usage {
                prompt_tokens: total_tokens,
                completion_tokens: None,
                total_tokens,
                ..Default::default()
            }),
        })
    }

    async fn create_image(
        &self,
        _request: CreateImageRequest,
    ) -> Result<ImageResponse, BackendError> {
        Err(Self::unsupported("image generation"))
    }

    async fn create_speech(
        &self,
        _request: CreateSpeechRequest,
    ) -> Result<bytes::Bytes, BackendError> {
        Err(Self::unsupported("speech synthesis"))
    }

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
        Err(Self::unsupported("transcription"))
    }

    async fn create_translation(
        &self,
        _model: String,
        _file_bytes: bytes::Bytes,
        _file_name: String,
        _prompt: Option<String>,
        _response_format: Option<String>,
        _temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(Self::unsupported("translation"))
    }

    async fn create_moderation(
        &self,
        _request: CreateModerationRequest,
    ) -> Result<CreateModerationResponse, BackendError> {
        Err(Self::unsupported("moderation"))
    }

    /// List the publisher catalog for the connection's region.
    ///
    /// Vertex has no OpenAI-compatible `/models` route, so this reads Model
    /// Garden. Errors propagate: this call is also what the Providers "Test"
    /// button probes with, and a key that cannot list is a key that cannot
    /// serve.
    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        let base = format!(
            "{}/v1beta1/publishers/{DEFAULT_PUBLISHER}/models?pageSize={MODEL_PAGE_SIZE}",
            self.host
        );

        let mut data = Vec::new();
        let mut page_token: Option<String> = None;

        for page in 0..MAX_MODEL_PAGES {
            let url = match &page_token {
                Some(token) => format!("{base}&pageToken={token}"),
                None => base.clone(),
            };

            let response = self.authorize(self.client.get(&url)).await?.send().await?;
            if !response.status().is_success() {
                return Err(Self::upstream_error(response).await);
            }

            let parsed: ListPublisherModelsResponse = response.json().await.map_err(|e| {
                BackendError::Other(format!("Failed to parse Vertex model list: {e}"))
            })?;

            data.extend(parsed.publisher_models.into_iter().filter_map(|model| {
                // `publishers/google/models/gemini-2.5-pro` → the last segment.
                let id = model
                    .name
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                (!id.is_empty()).then(|| Model {
                    id,
                    object: "model".to_string(),
                    created: 0,
                    owned_by: DEFAULT_PUBLISHER.to_string(),
                    capabilities: None,
                    provider_metadata: Default::default(),
                })
            }));

            page_token = parsed.next_page_token.filter(|token| !token.is_empty());
            if page_token.is_none() {
                break;
            }
            if page + 1 == MAX_MODEL_PAGES {
                tracing::warn!(
                    models = data.len(),
                    "Vertex publisher catalog exceeded {MAX_MODEL_PAGES} pages; listing truncated"
                );
            }
        }

        Ok(ListResponse {
            object: "list".to_string(),
            data,
            first_id: None,
            last_id: None,
            has_more: false,
        })
    }

    async fn get_model(&self, model_id: &str) -> Result<Model, BackendError> {
        Ok(Model {
            id: model_id.to_string(),
            object: "model".to_string(),
            created: 0,
            owned_by: DEFAULT_PUBLISHER.to_string(),
            capabilities: None,
            provider_metadata: Default::default(),
        })
    }

    fn name(&self) -> &str {
        "vertex"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SA_JSON: &str = r#"{
        "type": "service_account",
        "project_id": "my-proj",
        "private_key_id": "abc123",
        "private_key": "-----BEGIN PRIVATE KEY-----\nMIIB\n-----END PRIVATE KEY-----\n",
        "client_email": "router@my-proj.iam.gserviceaccount.com"
    }"#;

    fn backend(base_url: Option<&str>) -> VertexBackend {
        vertex_backend(Client::new(), SA_JSON, base_url).expect("build backend")
    }

    #[test]
    fn project_comes_from_the_key_and_region_defaults() {
        let backend = backend(None);
        assert_eq!(backend.project_id(), "my-proj");
        assert_eq!(backend.location(), DEFAULT_LOCATION);
        assert_eq!(
            backend.openai_base(),
            "https://us-central1-aiplatform.googleapis.com/v1beta1/projects/my-proj/locations/us-central1/endpoints/openapi"
        );
    }

    #[test]
    fn a_location_in_the_key_moves_project_and_host_together() {
        let raw = SA_JSON.replace(
            r#""project_id": "my-proj","#,
            r#""project_id": "my-proj", "location": "europe-west4","#,
        );
        let backend = vertex_backend(Client::new(), &raw, None).expect("build backend");
        assert_eq!(backend.location(), "europe-west4");
        assert_eq!(
            backend.openai_base(),
            "https://europe-west4-aiplatform.googleapis.com/v1beta1/projects/my-proj/locations/europe-west4/endpoints/openapi"
        );
    }

    #[test]
    fn a_base_url_override_sets_the_region_too() {
        let backend = backend(Some("https://asia-northeast1-aiplatform.googleapis.com/"));
        assert_eq!(backend.location(), "asia-northeast1");
        assert!(backend
            .openai_base()
            .starts_with("https://asia-northeast1-aiplatform.googleapis.com/v1beta1/projects/my-proj/locations/asia-northeast1/"));
    }

    #[test]
    fn the_global_endpoint_has_no_region_prefix() {
        let backend = backend(Some("https://aiplatform.googleapis.com"));
        assert_eq!(backend.location(), "global");
        assert_eq!(
            host_for_location("global"),
            "https://aiplatform.googleapis.com"
        );
        assert_eq!(
            backend.openai_base(),
            "https://aiplatform.googleapis.com/v1beta1/projects/my-proj/locations/global/endpoints/openapi"
        );
    }

    #[test]
    fn a_data_residency_host_names_its_multi_region() {
        // gemini-embedding-2 is served only here; the path must say `us`, not
        // the us-central1 default, so errors and logs match where data went.
        let backend = backend(Some("https://aiplatform.us.rep.googleapis.com"));
        assert_eq!(backend.location(), "us");
        assert_eq!(
            backend.publisher_base("google"),
            "https://aiplatform.us.rep.googleapis.com/v1/projects/my-proj/locations/us/publishers/google"
        );
        assert_eq!(
            location_from_host("https://aiplatform.europe-west4.rep.googleapis.com/"),
            Some("europe-west4".to_string())
        );
        assert_eq!(
            location_from_host("https://aiplatform.rep.googleapis.com"),
            None
        );
    }

    #[test]
    fn a_fanned_out_batch_sums_usage_only_when_every_call_reports_it() {
        let usage = |tokens| {
            Some(Usage {
                prompt_tokens: tokens,
                total_tokens: tokens,
                ..Default::default()
            })
        };
        let summed = sum_text_usage(vec![usage(3), usage(4)]).unwrap();
        assert_eq!(summed.total_tokens, 7);
        assert_eq!(
            summed.input_token_attribution,
            crate::pricing::InputTokenAttribution::TextRate
        );
        assert!(sum_text_usage(vec![usage(3), None]).is_none());
    }

    #[test]
    fn an_unrecognized_host_keeps_the_default_region() {
        // A private endpoint or proxy: honor it, but there is no region to read
        // out of it, so fall back rather than guessing from the hostname.
        let backend = backend(Some("https://vertex.internal.example/"));
        assert_eq!(backend.location(), DEFAULT_LOCATION);
        assert_eq!(location_from_host("https://vertex.internal.example"), None);
        assert!(backend
            .openai_base()
            .starts_with("https://vertex.internal.example/v1beta1/"));
    }

    #[test]
    fn embeddings_use_the_native_publisher_route() {
        assert_eq!(
            backend(None).publisher_base("google"),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1/publishers/google"
        );
    }

    #[test]
    fn claude_messages_use_vertex_raw_predict_contracts() {
        let backend = backend(None);
        let (url, body) = backend
            .prepare_anthropic_request(
                AnthropicRequestEndpoint::Messages,
                br#"{"model":"anthropic/claude-sonnet-4@20250514","stream":true,"messages":[]}"#,
            )
            .unwrap();
        assert_eq!(
            url,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1/publishers/anthropic/models/claude-sonnet-4@20250514:streamRawPredict"
        );
        assert!(body.get("model").is_none());
        assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
    }

    #[test]
    fn claude_count_tokens_use_vertex_raw_predict_contracts() {
        let backend = backend(None);

        // Publisher-prefixed model: stripped to the bare name, same as messages.
        let (url, body) = backend
            .prepare_anthropic_request(
                AnthropicRequestEndpoint::CountTokens,
                br#"{"model":"anthropic/claude-sonnet-4@20250514","messages":[]}"#,
            )
            .unwrap();
        assert!(url.ends_with("/publishers/anthropic/models/count-tokens:rawPredict"));
        assert_eq!(body["model"], "claude-sonnet-4@20250514");
        assert_eq!(body["anthropic_version"], "vertex-2023-10-16");

        // Bare model name: left unchanged.
        let (_, body) = backend
            .prepare_anthropic_request(
                AnthropicRequestEndpoint::CountTokens,
                br#"{"model":"claude-sonnet-4@20250514","messages":[]}"#,
            )
            .unwrap();
        assert_eq!(body["model"], "claude-sonnet-4@20250514");

        // Caller-supplied anthropic_version is preserved (setdefault, not overwrite).
        let (_, body) = backend
            .prepare_anthropic_request(
                AnthropicRequestEndpoint::CountTokens,
                br#"{"model":"claude-sonnet-4@20250514","anthropic_version":"custom","messages":[]}"#,
            )
            .unwrap();
        assert_eq!(body["anthropic_version"], "custom");

        // Missing model is a 400, matching the messages branch.
        let error = backend
            .prepare_anthropic_request(AnthropicRequestEndpoint::CountTokens, br#"{"messages":[]}"#)
            .unwrap_err();
        match error {
            BackendError::ApiError { status, .. } => assert_eq!(status, 400),
            other => panic!("expected ApiError, got {other:?}"),
        }
    }

    #[test]
    fn bare_models_get_the_google_publisher() {
        assert_eq!(
            VertexBackend::qualify_model("gemini-2.5-pro"),
            "google/gemini-2.5-pro"
        );
    }

    #[test]
    fn already_qualified_models_pass_through() {
        assert_eq!(
            VertexBackend::qualify_model("meta/llama-3.1-405b-instruct-maas"),
            "meta/llama-3.1-405b-instruct-maas"
        );
        assert_eq!(
            VertexBackend::split_model("meta/llama-x"),
            ("meta".to_string(), "llama-x".to_string())
        );
        assert_eq!(
            VertexBackend::split_model("text-embedding-005"),
            ("google".to_string(), "text-embedding-005".to_string())
        );
    }

    #[test]
    fn max_completion_tokens_is_folded_into_max_tokens() {
        let mut request = ChatCompletionRequest {
            fallbacks: Default::default(),
            model: "gemini-2.5-flash".to_string(),
            messages: vec![],
            temperature: None,
            top_p: None,
            n: None,
            stream: None,
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
        };
        VertexBackend::prepare_chat(&mut request, false);
        assert_eq!(request.model, "google/gemini-2.5-flash");
        assert_eq!(request.max_tokens, Some(64));
        assert_eq!(request.max_completion_tokens, None);
    }

    #[test]
    fn a_credential_without_a_project_is_rejected() {
        let raw = SA_JSON.replace(r#""project_id": "my-proj","#, "");
        let err = vertex_backend(Client::new(), &raw, None).expect_err("must fail");
        assert!(err.to_string().contains("project"), "got {err}");
    }

    #[test]
    fn unsupported_operations_are_client_errors() {
        // Asking a Vertex connection for an operation it doesn't serve (image
        // generation, speech, ...) is a mismatch in the caller's request, not a
        // failure of the adapter itself, so it must surface as a 400 rather
        // than the 500 every other unmapped BackendError produces.
        let error = VertexBackend::unsupported("image generation");
        assert!(matches!(error, BackendError::ApiError { status: 400, .. }));
        assert!(error.to_string().contains("image generation"));
    }
}
