use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::provider::{
    AnthropicRequestEndpoint, AnthropicRequestHeaders, BackendError, BackendProvider, SseByteStream,
};
use crate::structured_output::{self, ResponseFormat};
use crate::types::{
    ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, CreateEmbeddingRequest,
    CreateEmbeddingResponse, CreateImageRequest, CreateModerationRequest, CreateModerationResponse,
    CreateSpeechRequest, FunctionCall, ImageResponse, ListResponse, Model, Tool, ToolCall,
    TranscriptionResponse, Usage,
};

const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com/v1";
/// Anthropic requires `max_tokens`; translated OpenAI requests that omit both
/// token-limit fields use this adapter default.
pub(crate) const ANTHROPIC_DEFAULT_MAX_TOKENS: u64 = 4_096;
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_MODEL_PAGE_SIZE: usize = 1000;
const ANTHROPIC_MAX_MODEL_PAGES: usize = 32;
const ANTHROPIC_MAX_MODEL_RECORDS: usize = 10_000;
const ANTHROPIC_MODEL_PAGE_MAX_BYTES: usize = 4 * 1024 * 1024;
const ANTHROPIC_PROBE_DNS_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const ANTHROPIC_MODEL_PAGE_TIMEOUT: Duration = Duration::from_secs(10);
const ANTHROPIC_MODEL_DETAIL_TIMEOUT: Duration = Duration::from_secs(25);
pub(crate) const ANTHROPIC_MODEL_CATALOG_TIMEOUT: Duration = Duration::from_secs(25);
/// Beta header value opting into Anthropic's native structured-output
/// (`output_config`) feature. Sent whenever a request carries `response_format`.
///
/// NOTE: verify against a live Anthropic key. We pin `anthropic-version`
/// 2023-06-01, on which structured output is gated behind this beta header; if
/// an account/model has the feature GA without the header, sending it is still
/// accepted, so this is the safe default.
const ANTHROPIC_STRUCTURED_OUTPUT_BETA: &str = "structured-outputs-2025-11-13";

/// Anthropic API request/response types
#[derive(Debug, Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    /// Either a string or an array of content blocks (text, tool_use, tool_result).
    content: Value,
}

#[derive(Debug, Serialize)]
struct AnthropicRequest<'a> {
    model: String,
    messages: Vec<AnthropicMessage>,
    max_tokens: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    /// Native output configuration for structured format and reasoning effort.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<crate::schema_json::SchemaJson>,
    /// Thinking controls derived from Chat `reasoning_effort`.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Thinking<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Thinking<'a> {
    Disabled,
    Adaptive {
        #[serde(skip)]
        effort: &'a str,
    },
}

impl<'a> Thinking<'a> {
    fn effort(self) -> Option<&'a str> {
        match self {
            Self::Disabled => None,
            Self::Adaptive { effort } => Some(effort),
        }
    }
}

/// Map Chat effort to the controls sent to Anthropic.
fn reasoning_controls(effort: Option<&str>) -> Option<Thinking<'_>> {
    effort.map(|effort| match effort {
        "none" => Thinking::Disabled,
        "minimal" => Thinking::Adaptive { effort: "low" },
        effort => Thinking::Adaptive { effort },
    })
}

fn anthropic_max_tokens(request: &ChatCompletionRequest) -> i32 {
    request
        .max_completion_tokens
        .or(request.max_tokens)
        .unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS as i32)
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: crate::schema_json::SchemaJson,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AnthropicResponse {
    id: String,
    #[serde(rename = "type")]
    response_type: String,
    role: String,
    content: Vec<AnthropicContent>,
    model: String,
    stop_reason: Option<String>,
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContent {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: i32,
    output_tokens: i32,
    #[serde(default)]
    cache_read_input_tokens: i32,
    #[serde(default)]
    cache_creation_input_tokens: i32,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorResponse {
    #[serde(rename = "type")]
    error_type: String,
    error: AnthropicErrorDetail,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    error_type: String,
    message: String,
}

const DISABLED_REJECTIONS: [&str; 3] = [
    r#""thinking.type.disabled" is not supported for this model. Use "thinking.type.adaptive" and "output_config.effort" to control thinking behavior."#,
    r#""thinking.type.disabled" is not supported for this model. Thinking defaults to adaptive mode when not specified; use "thinking.type.enabled" with "budget_tokens" for extended thinking."#,
    r#"To turn thinking off on this model, send "thinking": {"type": "between_tools"} instead of {"type": "disabled"}. The model does not think before responding. The short updates it writes between tool calls come back as thinking blocks."#,
];

/// Match complete diagnostics for the controls sent, not loose substrings.
/// Thinking diagnostics: https://platform.claude.com/docs/en/api/errors
fn reasoning_support_rejection(
    status: u16,
    error: &AnthropicErrorResponse,
    thinking: Option<Thinking<'_>>,
) -> bool {
    if status != 400
        || error.error_type != "error"
        || error.error.error_type != "invalid_request_error"
    {
        return false;
    }
    let message = error.error.message.trim();
    match thinking {
        None => false,
        Some(Thinking::Disabled) => DISABLED_REJECTIONS.contains(&message),
        Some(Thinking::Adaptive { effort }) => {
            message == "adaptive thinking is not supported on this model"
                || effort_unsupported(message, effort)
        }
    }
}

fn effort_unsupported(message: &str, effort: &str) -> bool {
    if message == "This model does not support the effort parameter." {
        return true;
    }
    if !is_effort_name(effort) {
        return false;
    }
    let prefix = format!("This model does not support effort level '{effort}'. Supported levels: ");
    let Some(levels) = message
        .strip_prefix(&prefix)
        .and_then(|s| s.strip_suffix('.'))
    else {
        return false;
    };
    let levels: Vec<_> = levels.split(',').map(str::trim).collect();
    !levels.is_empty()
        && levels.iter().all(|level| is_effort_name(level))
        && !levels.contains(&effort)
}

fn is_effort_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[derive(Debug, Deserialize)]
struct AnthropicModelInfo {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    created_at: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreatedAtIssue {
    Missing,
    Invalid,
}

#[derive(Default)]
struct ModelMappingDiagnostics {
    skipped_invalid_id: usize,
    missing_created_at: usize,
    invalid_created_at: usize,
    created_at_sample: Option<String>,
}

impl ModelMappingDiagnostics {
    fn log(&self, operation: &'static str) {
        if self.skipped_invalid_id == 0
            && self.missing_created_at == 0
            && self.invalid_created_at == 0
        {
            return;
        }
        tracing::warn!(
            operation,
            skipped_invalid_id = self.skipped_invalid_id,
            missing_created_at = self.missing_created_at,
            invalid_created_at = self.invalid_created_at,
            sample_model = self.created_at_sample.as_deref().unwrap_or(""),
            "Anthropic model response contained malformed records; skipping unusable ids and defaulting invalid release dates to zero"
        );
    }
}

impl AnthropicModelInfo {
    fn into_model(self, diagnostics: &mut ModelMappingDiagnostics) -> Option<Model> {
        let id = match self.id {
            Some(Value::String(id)) if !id.is_empty() => id,
            _ => {
                diagnostics.skipped_invalid_id += 1;
                return None;
            }
        };
        let (created, issue) = match self.created_at.as_ref() {
            Some(Value::String(created_at)) => {
                match chrono::DateTime::parse_from_rfc3339(created_at) {
                    Ok(created_at) => (created_at.timestamp(), None),
                    Err(_) => (0, Some(CreatedAtIssue::Invalid)),
                }
            }
            Some(_) => (0, Some(CreatedAtIssue::Invalid)),
            None => (0, Some(CreatedAtIssue::Missing)),
        };
        match issue {
            Some(CreatedAtIssue::Missing) => diagnostics.missing_created_at += 1,
            Some(CreatedAtIssue::Invalid) => diagnostics.invalid_created_at += 1,
            None => {}
        }
        if issue.is_some() && diagnostics.created_at_sample.is_none() {
            diagnostics.created_at_sample = Some(id.clone());
        }
        Some(Model {
            id,
            object: "model".to_string(),
            created,
            owned_by: "anthropic".to_string(),
            capabilities: None,
            provider_metadata: Default::default(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicModelsPage {
    data: Vec<Value>,
    last_id: Option<String>,
    has_more: bool,
}

/// Parsed Anthropic API root shared by translated requests and the native
/// Messages passthrough. A stored override is a trusted API root (for example
/// `https://gateway.example/anthropic/v1`); endpoint paths are always appended
/// as encoded URL segments.
#[derive(Clone, Debug)]
pub(crate) struct AnthropicEndpoints {
    base: Url,
}

/// Allow loopback Anthropic upstreams directly from the connection's base URL.
/// Other targets retain the existing public-endpoint checks.
pub(crate) fn validate_anthropic_base_url(base_url: Option<&str>) -> Result<(), BackendError> {
    let endpoints = AnthropicEndpoints::from_base_url(base_url)?;
    if is_local_mock_host(&endpoints.base) {
        Ok(())
    } else {
        AnthropicEndpoints::from_public_base_url(base_url)
            .map(|_| ())
            .map_err(|_| {
                BackendError::ConfigError(format!(
                    "Anthropic base URL '{}' must use a public host or a loopback address",
                    endpoints.base
                ))
            })
    }
}

fn is_local_mock_host(url: &Url) -> bool {
    url.host_str() == Some("localhost")
        || url
            .host_str()
            .and_then(|host| parse_ip_host(host).ok())
            .is_some_and(|address| address.is_loopback())
}

/// Local mock traffic must stay local even when the shell configures a proxy
/// or the mock responds with a redirect to another service. Pin localhost to
/// loopback addresses so system DNS/hosts configuration cannot redirect it.
fn local_anthropic_client() -> Result<Client, BackendError> {
    Client::builder()
        .no_proxy()
        .resolve_to_addrs(
            "localhost",
            &[
                SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                SocketAddr::from((Ipv6Addr::LOCALHOST, 0)),
            ],
        )
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            BackendError::ConfigError(format!("could not build local Anthropic client: {error}"))
        })
}

impl AnthropicEndpoints {
    pub(crate) fn from_base_url(base_url: Option<&str>) -> Result<Self, BackendError> {
        let raw = base_url
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .unwrap_or(ANTHROPIC_API_BASE);
        let url = Url::parse(raw).map_err(|error| {
            BackendError::ConfigError(format!("invalid Anthropic base URL '{raw}': {error}"))
        })?;
        if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
            return Err(BackendError::ConfigError(format!(
                "Anthropic base URL '{raw}' must be an absolute HTTP(S) URL"
            )));
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(BackendError::ConfigError(format!(
                "Anthropic base URL '{raw}' must not contain credentials, a query, or a fragment"
            )));
        }
        Ok(Self { base: url })
    }

    /// Parse an operator-supplied endpoint that will receive an Anthropic API
    /// key. Unlike [`Self::from_base_url`], this rejects local-network targets;
    /// provider connection validation uses it before persisting or probing an
    /// override.
    pub(crate) fn from_public_base_url(base_url: Option<&str>) -> Result<Self, BackendError> {
        let endpoints = Self::from_base_url(base_url)?;
        let is_non_public = match endpoints.base.host_str() {
            Some(host) => match parse_ip_host(host) {
                Ok(address) => ip_is_non_public(address),
                Err(_) => host == "localhost" || host.ends_with(".localhost"),
            },
            None => true,
        };
        if is_non_public {
            return Err(BackendError::ConfigError(format!(
                "Anthropic base URL '{}' must use a public host",
                endpoints.base
            )));
        }
        Ok(endpoints)
    }

    fn with_segments(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        let mut path = url
            .path_segments_mut()
            .expect("validated HTTP(S) Anthropic URL supports path segments");
        path.pop_if_empty();
        for segment in segments {
            path.push(segment);
        }
        drop(path);
        url
    }

    pub(crate) fn messages_url(&self) -> Url {
        self.with_segments(&["messages"])
    }

    pub(crate) fn count_tokens_url(&self) -> Url {
        self.with_segments(&["messages", "count_tokens"])
    }

    fn models_url(&self) -> Url {
        self.with_segments(&["models"])
    }

    fn model_url(&self, model_id: &str) -> Result<Url, BackendError> {
        if model_id.is_empty() || matches!(model_id, "." | "..") {
            return Err(BackendError::api_status(
                400,
                "Anthropic model id must not be empty, '.' or '..'",
            ));
        }
        Ok(self.with_segments(&["models", model_id]))
    }
}

/// Build a probe-only client for a loopback upstream or a resolved
/// and pinned public Anthropic endpoint. The probe does not follow redirects,
/// so a public endpoint cannot redirect an authenticated request to a
/// local-network target.
pub(crate) async fn anthropic_probe_client(base_url: Option<&str>) -> Result<Client, BackendError> {
    let endpoints = AnthropicEndpoints::from_base_url(base_url)?;
    if is_local_mock_host(&endpoints.base) {
        return local_anthropic_client();
    }
    let endpoints = AnthropicEndpoints::from_public_base_url(base_url)?;
    let mut builder = Client::builder().redirect(reqwest::redirect::Policy::none());

    let domain = endpoints
        .base
        .host_str()
        .ok_or_else(|| BackendError::ConfigError("Anthropic base URL has no host".to_string()))?;
    if parse_ip_host(domain).is_err() {
        let port = endpoints.base.port_or_known_default().ok_or_else(|| {
            BackendError::ConfigError("Anthropic base URL has no usable port".to_string())
        })?;
        let addresses = tokio::time::timeout(
            ANTHROPIC_PROBE_DNS_TIMEOUT,
            tokio::net::lookup_host((domain, port)),
        )
        .await
        .map_err(|_| {
            BackendError::ConfigError(format!(
                "timed out resolving Anthropic base URL host '{domain}'"
            ))
        })?
        .map_err(|error| {
            BackendError::ConfigError(format!(
                "could not resolve Anthropic base URL host '{domain}': {error}"
            ))
        })?
        .collect::<Vec<_>>();

        if addresses.is_empty() {
            return Err(BackendError::ConfigError(format!(
                "Anthropic base URL host '{domain}' resolved to no addresses"
            )));
        }
        if addresses
            .iter()
            .any(|address| ip_is_non_public(address.ip()))
        {
            return Err(BackendError::ConfigError(format!(
                "Anthropic base URL host '{domain}' must resolve only to public addresses"
            )));
        }
        builder = builder.resolve_to_addrs(domain, &addresses);
    }

    builder.build().map_err(|error| {
        BackendError::ConfigError(format!("could not build Anthropic probe client: {error}"))
    })
}

fn parse_ip_host(host: &str) -> Result<std::net::IpAddr, std::net::AddrParseError> {
    host.trim_start_matches('[').trim_end_matches(']').parse()
}

fn ip_is_non_public(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(address) => ipv4_is_non_public(address),
        std::net::IpAddr::V6(address) => {
            address.is_unspecified()
                || address.is_loopback()
                || address.is_multicast()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || address.to_ipv4().is_some_and(ipv4_is_non_public)
        }
    }
}

fn ipv4_is_non_public(address: std::net::Ipv4Addr) -> bool {
    address.is_unspecified()
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_multicast()
        || address.is_broadcast()
}

/// Anthropic backend implementation
pub struct AnthropicBackend {
    client: Client,
    api_key: String,
    endpoints: Result<AnthropicEndpoints, BackendError>,
    model_page_timeout: Duration,
    model_detail_timeout: Duration,
    model_catalog_timeout: Duration,
}

/// Build an Anthropic adapter from a stored connection, honoring an optional
/// endpoint override. Routing and administrative probes both use this helper so
/// they cannot drift on where an operator-supplied credential is sent.
pub fn anthropic_backend(
    client: Client,
    api_key: String,
    base_url: Option<&str>,
) -> AnthropicBackend {
    AnthropicBackend::with_optional_base_url(client, api_key, base_url)
}

impl AnthropicBackend {
    /// Create a new Anthropic backend with the given API key
    pub fn new(client: Client, api_key: String) -> Self {
        Self::with_optional_base_url(client, api_key, None)
    }

    /// Create a new Anthropic backend with a custom base URL
    pub fn with_base_url(client: Client, api_key: String, base_url: String) -> Self {
        Self::with_optional_base_url(client, api_key, Some(&base_url))
    }

    fn with_optional_base_url(client: Client, api_key: String, base_url: Option<&str>) -> Self {
        Self::with_model_timeouts(
            client,
            api_key,
            base_url,
            ANTHROPIC_MODEL_PAGE_TIMEOUT,
            ANTHROPIC_MODEL_DETAIL_TIMEOUT,
            ANTHROPIC_MODEL_CATALOG_TIMEOUT,
        )
    }

    #[cfg(test)]
    fn with_request_timeout(
        client: Client,
        api_key: String,
        base_url: String,
        model_request_timeout: Duration,
    ) -> Self {
        Self::with_model_timeouts(
            client,
            api_key,
            Some(&base_url),
            model_request_timeout,
            model_request_timeout,
            ANTHROPIC_MODEL_CATALOG_TIMEOUT,
        )
    }

    fn with_model_timeouts(
        client: Client,
        api_key: String,
        base_url: Option<&str>,
        model_page_timeout: Duration,
        model_detail_timeout: Duration,
        model_catalog_timeout: Duration,
    ) -> Self {
        let mut endpoints = AnthropicEndpoints::from_base_url(base_url);
        let client = if endpoints
            .as_ref()
            .is_ok_and(|value| is_local_mock_host(&value.base))
        {
            match local_anthropic_client() {
                Ok(local) => local,
                Err(error) => {
                    endpoints = Err(error);
                    client
                }
            }
        } else {
            client
        };
        Self {
            client,
            api_key,
            endpoints,
            model_page_timeout,
            model_detail_timeout,
            model_catalog_timeout,
        }
    }

    fn endpoints(&self) -> Result<&AnthropicEndpoints, BackendError> {
        self.endpoints.as_ref().map_err(Clone::clone)
    }

    fn messages_url(&self) -> Result<Url, BackendError> {
        Ok(self.endpoints()?.messages_url())
    }

    fn count_tokens_url(&self) -> Result<Url, BackendError> {
        Ok(self.endpoints()?.count_tokens_url())
    }

    fn models_url(&self) -> Result<Url, BackendError> {
        Ok(self.endpoints()?.models_url())
    }

    fn model_url(&self, model_id: &str) -> Result<Url, BackendError> {
        self.endpoints()?.model_url(model_id)
    }

    fn authenticated_get(&self, url: Url, timeout: Duration) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .timeout(timeout)
    }

    async fn fetch_models_page(
        &self,
        after_id: Option<&str>,
    ) -> Result<AnthropicModelsPage, BackendError> {
        let mut request = self
            .authenticated_get(self.models_url()?, self.model_page_timeout)
            .query(&[("limit", ANTHROPIC_MODEL_PAGE_SIZE)]);
        if let Some(cursor) = after_id {
            request = request.query(&[("after_id", cursor)]);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response).await);
        }
        if response
            .content_length()
            .is_some_and(|length| length > ANTHROPIC_MODEL_PAGE_MAX_BYTES as u64)
        {
            return Err(BackendError::api_status(
                502,
                format!(
                    "Anthropic model page exceeded the {} byte response limit",
                    ANTHROPIC_MODEL_PAGE_MAX_BYTES
                ),
            ));
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if body.len().saturating_add(chunk.len()) > ANTHROPIC_MODEL_PAGE_MAX_BYTES {
                return Err(BackendError::api_status(
                    502,
                    format!(
                        "Anthropic model page exceeded the {} byte response limit",
                        ANTHROPIC_MODEL_PAGE_MAX_BYTES
                    ),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<AnthropicModelsPage>(&body).map_err(|error| {
            BackendError::api_status(
                502,
                format!("Invalid Anthropic model page response: {error}"),
            )
        })
    }

    /// Walk the paginated model catalog under one overall deadline
    /// (`model_catalog_timeout`), collecting pages until the catalog ends or a
    /// limit trips. `fetch_page` is injected so the deadline behaviour can be
    /// tested on tokio's paused clock without a socket; production passes
    /// `fetch_models_page`.
    async fn walk_model_catalog<F, Fut>(
        &self,
        mut fetch_page: F,
    ) -> Result<ListResponse<Model>, BackendError>
    where
        F: FnMut(Option<String>) -> Fut,
        Fut: Future<Output = Result<AnthropicModelsPage, BackendError>>,
    {
        let mut data = Vec::new();
        let mut after_id: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        let mut seen_model_ids = HashSet::new();
        let mut records_seen = 0usize;
        let mut diagnostics = ModelMappingDiagnostics::default();
        let mut truncation_reason: Option<&'static str> = None;
        let mut truncation_error: Option<String> = None;
        let catalog_deadline = tokio::time::Instant::now() + self.model_catalog_timeout;

        for page_number in 0..ANTHROPIC_MAX_MODEL_PAGES {
            let page = match tokio::time::timeout_at(catalog_deadline, fetch_page(after_id.clone()))
                .await
            {
                Ok(Ok(page)) => page,
                Ok(Err(error)) if data.is_empty() => return Err(error),
                Ok(Err(error)) => {
                    truncation_reason = Some("page_error");
                    truncation_error = Some(error.to_string());
                    break;
                }
                Err(_) if data.is_empty() => {
                    return Err(BackendError::api_status(
                        504,
                        "Timed out retrieving the Anthropic model catalog",
                    ));
                }
                Err(_) => {
                    truncation_reason = Some("catalog_timeout");
                    break;
                }
            };
            let AnthropicModelsPage {
                data: page_models,
                last_id,
                has_more,
            } = page;
            let remaining_records = ANTHROPIC_MAX_MODEL_RECORDS - records_seen;
            let page_record_count = page_models.len();
            for record in page_models.into_iter().take(remaining_records) {
                records_seen += 1;
                let model_info = match serde_json::from_value::<AnthropicModelInfo>(record) {
                    Ok(model_info) => model_info,
                    Err(_) => {
                        diagnostics.skipped_invalid_id += 1;
                        continue;
                    }
                };
                if let Some(model) = model_info.into_model(&mut diagnostics) {
                    if seen_model_ids.insert(model.id.clone()) {
                        data.push(model);
                    }
                }
            }

            if page_record_count > remaining_records
                || (records_seen == ANTHROPIC_MAX_MODEL_RECORDS && has_more)
            {
                truncation_reason = Some("record_limit");
                break;
            }

            if !has_more {
                break;
            }

            if page_number + 1 == ANTHROPIC_MAX_MODEL_PAGES {
                truncation_reason = Some("page_limit");
                break;
            }

            let Some(cursor) = last_id.filter(|id| !id.is_empty()) else {
                if data.is_empty() {
                    return Err(BackendError::Other(
                        "Anthropic model list indicated another page without a last_id cursor"
                            .to_string(),
                    ));
                }
                truncation_reason = Some("missing_cursor");
                break;
            };
            if !seen_cursors.insert(cursor.clone()) {
                if data.is_empty() {
                    return Err(BackendError::Other(format!(
                        "Anthropic model list repeated pagination cursor '{cursor}'"
                    )));
                }
                truncation_reason = Some("repeated_cursor");
                break;
            }
            after_id = Some(cursor);
        }

        diagnostics.log("catalog");
        if let Some(reason) = truncation_reason {
            tracing::warn!(
                reason,
                records_seen,
                unique_models = data.len(),
                max_pages = ANTHROPIC_MAX_MODEL_PAGES,
                max_records = ANTHROPIC_MAX_MODEL_RECORDS,
                error = truncation_error.as_deref().unwrap_or(""),
                "Anthropic model catalog is incomplete; returning the useful partial catalog"
            );
        }

        Ok(ListResponse {
            object: "list".to_string(),
            data,
            first_id: None,
            last_id: None,
            // Chalk Router returns an aggregated, non-pageable catalog, matching
            // its OpenAI model-list behavior. Truncation is observable in logs,
            // not through Anthropic's cursor-pagination fields.
            has_more: false,
        })
    }

    async fn list_models_all_pages(&self) -> Result<ListResponse<Model>, BackendError> {
        self.walk_model_catalog(|after_id| async move {
            self.fetch_models_page(after_id.as_deref()).await
        })
        .await
    }

    async fn api_error(response: reqwest::Response) -> BackendError {
        super::provider::upstream_error(response, |text| {
            serde_json::from_str::<AnthropicErrorResponse>(text)
                .ok()
                .map(|r| r.error.message)
        })
        .await
    }

    /// Only translated Chat dispatch errors can carry this classification.
    async fn chat_api_error(
        response: reqwest::Response,
        thinking: Option<Thinking<'_>>,
    ) -> BackendError {
        super::provider::upstream_error_with_fallback(response, |status, text| {
            let error = serde_json::from_str::<AnthropicErrorResponse>(text).ok()?;
            let reasoning_incompatible = reasoning_support_rejection(status, &error, thinking);
            Some((error.error.message, reasoning_incompatible))
        })
        .await
    }

    /// Extract a flat string from an OpenAI-style content field (string or content-parts array).
    fn extract_text(content: Option<&Value>) -> String {
        match content {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(|t| t.as_str())
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        }
    }

    /// Convert OpenAI-style chat messages to Anthropic format. Returns the message list
    /// (assistant/user only) and any system text hoisted out into the top-level `system` field.
    fn convert_messages(messages: &[ChatMessage]) -> (Vec<AnthropicMessage>, Option<String>) {
        let mut system_parts: Vec<String> = Vec::new();
        let mut out: Vec<AnthropicMessage> = Vec::new();

        for msg in messages {
            match msg.role.as_str() {
                "system" | "developer" => {
                    let text = Self::extract_text(msg.content.as_ref());
                    if !text.is_empty() {
                        system_parts.push(text);
                    }
                }
                "tool" => {
                    // OpenAI 'tool' role -> Anthropic user message with a tool_result block
                    let tool_use_id = msg.tool_call_id.clone().unwrap_or_default();
                    let result_text = Self::extract_text(msg.content.as_ref());
                    let block = json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": result_text,
                    });
                    out.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: Value::Array(vec![block]),
                    });
                }
                "assistant" => {
                    let mut blocks: Vec<Value> = Vec::new();
                    let text = Self::extract_text(msg.content.as_ref());
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                    if let Some(tool_calls) = &msg.tool_calls {
                        for tc in tool_calls {
                            let input = crate::translation::chat_controls::tool_call_input(
                                &tc.function.arguments,
                            );
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.function.name,
                                "input": input,
                            }));
                        }
                    }
                    let content = if blocks.is_empty() {
                        Value::String(String::new())
                    } else {
                        Value::Array(blocks)
                    };
                    out.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content,
                    });
                }
                _ => {
                    // user (or unknown role) -> user with plain text
                    let text = Self::extract_text(msg.content.as_ref());
                    out.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: Value::String(text),
                    });
                }
            }
        }

        let system = if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n\n"))
        };
        (out, system)
    }

    /// Translate OpenAI tool definitions into Anthropic tool definitions.
    fn convert_tools(tools: &[Tool]) -> Vec<AnthropicTool> {
        tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.function.name.clone(),
                description: t.function.description.clone(),
                input_schema: t
                    .function
                    .parameters
                    .clone()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}).into()),
            })
            .collect()
    }

    /// Translate an OpenAI `tool_choice` value into an Anthropic `tool_choice` object.
    /// Returns None if the input cannot be mapped meaningfully. When tools are
    /// present, an omitted choice has the API default of `auto`, which must be
    /// made explicit to carry `parallel_tool_calls: false` losslessly.
    ///
    /// `parallel_tool_calls: Some(false)` maps onto Anthropic's
    /// `disable_parallel_tool_use: true`, which Anthropic only recognizes on
    /// the `auto`, `any`, and `tool` choice types (not `none`, which disables
    /// tool use entirely).
    fn convert_tool_choice(
        tool_choice: Option<&Value>,
        parallel_tool_calls: Option<bool>,
        has_tools: bool,
    ) -> Option<Value> {
        let tool_choice = tool_choice.filter(|choice| !choice.is_null());
        let mut mapped = match tool_choice {
            Some(v) => match v {
                Value::String(s) => match s.as_str() {
                    "none" => Some(json!({"type": "none"})),
                    "auto" => Some(json!({"type": "auto"})),
                    "required" => Some(json!({"type": "any"})),
                    _ => None,
                },
                Value::Object(_) => {
                    if v.get("type").and_then(|t| t.as_str()) == Some("function") {
                        v.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .map(|name| json!({"type": "tool", "name": name}))
                    } else {
                        None
                    }
                }
                _ => None,
            },
            None if parallel_tool_calls == Some(false) && has_tools => {
                Some(json!({"type": "auto"}))
            }
            None => None,
        }?;
        if parallel_tool_calls == Some(false) {
            if let Some(map) = mapped.as_object_mut() {
                if map.get("type").and_then(Value::as_str) != Some("none") {
                    map.insert("disable_parallel_tool_use".to_string(), json!(true));
                }
            }
        }
        Some(mapped)
    }

    /// Translate the caller's OpenAI `response_format` into Anthropic's native
    /// structured-output config (`output_config.format`).
    ///
    /// Anthropic returns the JSON as an ordinary text content block, so no
    /// response rewriting is needed — [`Self::convert_response`] already maps a
    /// text block to `message.content`. This composes with caller-supplied
    /// tools. Returns `None` when there is no structured-output request.
    ///
    /// Models that predate Anthropic's structured-output feature are rejected by
    /// the API with a clear error, which the backend forwards to the caller.
    fn native_output_config(
        response_format: Option<&crate::schema_json::SchemaJson>,
    ) -> Result<Option<crate::schema_json::SchemaJson>, BackendError> {
        use crate::schema_json::SchemaJson;
        let Some(raw) = response_format else {
            return Ok(None);
        };
        let Some(rf) = ResponseFormat::parse(raw)
            .map_err(|error| BackendError::api_status(400, error.to_string()))?
        else {
            return Ok(None);
        };
        let schema = rf.schema_or_permissive().clone();
        Ok(Some(SchemaJson::object([(
            "format",
            SchemaJson::object([("type", json!("json_schema").into()), ("schema", schema)]),
        )])))
    }

    /// Merge effort into the native structured-output config, if present.
    fn output_config(
        config: Option<crate::schema_json::SchemaJson>,
        effort: Option<&str>,
    ) -> Result<Option<crate::schema_json::SchemaJson>, BackendError> {
        use crate::schema_json::SchemaJson;
        let Some(effort) = effort else {
            return Ok(config);
        };
        let Some(mut config) = config else {
            return Ok(Some(SchemaJson::object([("effort", json!(effort).into())])));
        };
        config
            .set_member("effort", json!(effort).into())
            .map_err(|error| BackendError::Other(error.to_string()))?;
        Ok(Some(config))
    }

    /// If validation is enabled, check the structured-output content against the
    /// requested schema and log a warning on mismatch (never fails the response).
    fn validate_structured_output(
        resp: &ChatCompletionResponse,
        response_format: Option<&crate::schema_json::SchemaJson>,
    ) {
        if !structured_output::validation_enabled() {
            return;
        }
        let Some(rf) =
            response_format.and_then(|format| ResponseFormat::parse(format).ok().flatten())
        else {
            return;
        };
        let schema = rf.schema();
        for choice in &resp.choices {
            if let Some(content) = choice.message.content.as_ref().and_then(|c| c.as_str()) {
                match structured_output::validate_json_output(content, schema) {
                    structured_output::Validation::Ok => {}
                    other => tracing::warn!(
                        validation = ?other,
                        "structured output failed validation"
                    ),
                }
            }
        }
    }

    /// Convert Anthropic stop_reason to OpenAI finish_reason
    fn convert_stop_reason(stop_reason: Option<String>) -> Option<String> {
        stop_reason.map(|reason| match reason.as_str() {
            "end_turn" => "stop".to_string(),
            "max_tokens" => "length".to_string(),
            "stop_sequence" => "stop".to_string(),
            "tool_use" => "tool_calls".to_string(),
            other => other.to_string(),
        })
    }

    /// Convert Anthropic response to OpenAI format
    fn convert_response(
        anthropic_resp: AnthropicResponse,
        model: String,
    ) -> ChatCompletionResponse {
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        for block in anthropic_resp.content.into_iter() {
            match block {
                AnthropicContent::Text { text } => text_parts.push(text),
                AnthropicContent::ToolUse { id, name, input } => {
                    let arguments =
                        serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                    tool_calls.push(ToolCall {
                        id,
                        tool_type: "function".to_string(),
                        function: FunctionCall { name, arguments },
                    });
                }
                AnthropicContent::Other => {}
            }
        }

        let content = if text_parts.is_empty() {
            None
        } else {
            Some(Value::String(text_parts.join("\n")))
        };
        let tool_calls = if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        };

        ChatCompletionResponse {
            id: anthropic_resp.id,
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp(),
            model,
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content,
                    name: None,
                    tool_calls,
                    tool_call_id: None,
                },
                finish_reason: Self::convert_stop_reason(anthropic_resp.stop_reason),
            }],
            usage: Some(Usage {
                prompt_tokens: anthropic_resp
                    .usage
                    .input_tokens
                    .saturating_add(anthropic_resp.usage.cache_read_input_tokens)
                    .saturating_add(anthropic_resp.usage.cache_creation_input_tokens),
                completion_tokens: Some(anthropic_resp.usage.output_tokens),
                total_tokens: anthropic_resp
                    .usage
                    .input_tokens
                    .saturating_add(anthropic_resp.usage.cache_read_input_tokens)
                    .saturating_add(anthropic_resp.usage.cache_creation_input_tokens)
                    .saturating_add(anthropic_resp.usage.output_tokens),
                prompt_tokens_details: Some(crate::types::PromptTokenDetails {
                    cached_tokens: anthropic_resp.usage.cache_read_input_tokens,
                    cache_write_tokens: anthropic_resp.usage.cache_creation_input_tokens,
                }),
                completion_tokens_details: None,
                ..Default::default()
            }),
        }
    }
}

/// Stateful translator that converts Anthropic SSE events into OpenAI
/// chat.completion.chunk SSE bytes.
struct AnthropicStreamTranslator {
    id: String,
    model: String,
    created: i64,
    /// Maps an Anthropic content-block index to its OpenAI tool_calls index.
    /// Anthropic block indices include text blocks; OpenAI tool_calls indices count only tool calls.
    block_to_tool: HashMap<i64, usize>,
    tool_count: usize,
    final_finish_reason: Option<String>,
    input_tokens: i32,
    output_tokens: i32,
    /// Cache-read input tokens from `usage.cache_read_input_tokens`. Anthropic
    /// reports these on `message_start` and, on some API versions, updates
    /// them again on `message_delta`.
    cache_read_input_tokens: i32,
    /// Cache-creation (cache-write) input tokens from
    /// `usage.cache_creation_input_tokens`, mirrored the same way.
    cache_creation_input_tokens: i32,
    role_emitted: bool,
}

impl AnthropicStreamTranslator {
    fn new(model: String, created: i64) -> Self {
        Self {
            id: String::new(),
            model,
            created,
            block_to_tool: HashMap::new(),
            tool_count: 0,
            final_finish_reason: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            role_emitted: false,
        }
    }

    fn handle_event(&mut self, event_name: &str, data: &Value) -> Vec<bytes::Bytes> {
        let mut out = Vec::new();
        match event_name {
            "message_start" => {
                if let Some(msg) = data.get("message") {
                    if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                        self.id = id.to_string();
                    }
                    if let Some(usage) = msg.get("usage") {
                        if let Some(it) = usage.get("input_tokens").and_then(|v| v.as_i64()) {
                            self.input_tokens = it as i32;
                        }
                        if let Some(cr) = usage
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_i64())
                        {
                            self.cache_read_input_tokens = cr as i32;
                        }
                        if let Some(cc) = usage
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_i64())
                        {
                            self.cache_creation_input_tokens = cc as i32;
                        }
                    }
                }
                if !self.role_emitted {
                    out.push(self.format_chunk(json!({"role": "assistant"}), None));
                    self.role_emitted = true;
                }
            }
            "content_block_start" => {
                let index = data.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                if let Some(block) = data.get("content_block") {
                    if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        let tool_idx = self.tool_count;
                        self.tool_count += 1;
                        self.block_to_tool.insert(index, tool_idx);
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        out.push(self.format_chunk(
                            json!({
                                "tool_calls": [{
                                    "index": tool_idx,
                                    "id": id,
                                    "type": "function",
                                    "function": {"name": name, "arguments": ""}
                                }]
                            }),
                            None,
                        ));
                    }
                }
            }
            "content_block_delta" => {
                let index = data.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                if let Some(delta) = data.get("delta") {
                    match delta.get("type").and_then(|v| v.as_str()) {
                        Some("text_delta") => {
                            if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                out.push(self.format_chunk(json!({"content": text}), None));
                            }
                        }
                        Some("input_json_delta") => {
                            if let (Some(partial), Some(&tool_idx)) = (
                                delta.get("partial_json").and_then(|v| v.as_str()),
                                self.block_to_tool.get(&index),
                            ) {
                                out.push(self.format_chunk(
                                    json!({
                                        "tool_calls": [{
                                            "index": tool_idx,
                                            "function": {"arguments": partial}
                                        }]
                                    }),
                                    None,
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(stop_reason) = data
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|v| v.as_str())
                {
                    self.final_finish_reason =
                        AnthropicBackend::convert_stop_reason(Some(stop_reason.to_string()));
                }
                if let Some(usage) = data.get("usage") {
                    if let Some(ot) = usage.get("output_tokens").and_then(|v| v.as_i64()) {
                        self.output_tokens = ot as i32;
                    }
                    // Newer Anthropic API versions can also report (updated)
                    // input/cache usage on `message_delta`; mirror them if present.
                    if let Some(it) = usage.get("input_tokens").and_then(|v| v.as_i64()) {
                        self.input_tokens = it as i32;
                    }
                    if let Some(cr) = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_i64())
                    {
                        self.cache_read_input_tokens = cr as i32;
                    }
                    if let Some(cc) = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_i64())
                    {
                        self.cache_creation_input_tokens = cc as i32;
                    }
                }
            }
            "message_stop" => {
                let prompt_tokens = self
                    .input_tokens
                    .saturating_add(self.cache_read_input_tokens)
                    .saturating_add(self.cache_creation_input_tokens);
                let prompt_tokens_details =
                    serde_json::to_value(crate::types::PromptTokenDetails {
                        cached_tokens: self.cache_read_input_tokens,
                        cache_write_tokens: self.cache_creation_input_tokens,
                    })
                    .unwrap_or(Value::Null);
                let chunk = json!({
                    "id": self.id,
                    "object": "chat.completion.chunk",
                    "created": self.created,
                    "model": self.model,
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": self.final_finish_reason,
                    }],
                    "usage": {
                        "prompt_tokens": prompt_tokens,
                        "completion_tokens": self.output_tokens,
                        "total_tokens": prompt_tokens + self.output_tokens,
                        "prompt_tokens_details": prompt_tokens_details,
                    }
                });
                if let Ok(s) = serde_json::to_string(&chunk) {
                    out.push(bytes::Bytes::from(format!("data: {}\n\n", s)));
                }
            }
            _ => {}
        }
        out
    }

    fn format_chunk(&self, delta: Value, finish_reason: Option<&str>) -> bytes::Bytes {
        let chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }]
        });
        bytes::Bytes::from(format!(
            "data: {}\n\n",
            serde_json::to_string(&chunk).unwrap_or_default()
        ))
    }
}

/// Parse one SSE event block (text between blank-line boundaries) into
/// (event_name, data_value). Returns None if there is no parseable `data:` payload.
fn parse_sse_event(event: &str) -> Option<(String, Value)> {
    let mut event_name = String::new();
    let mut data_buf = String::new();
    for line in event.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event_name = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data_buf.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(&data_buf).ok()?;
    Some((event_name, value))
}

/// Search `buf` for a `"\n\n"` boundary, resuming from `from` — an offset
/// already confirmed to contain no boundary — instead of rescanning the
/// whole buffer. Returns the absolute byte offset where the boundary
/// starts, if any.
fn find_double_newline(buf: &[u8], from: usize) -> Option<usize> {
    let from = from.min(buf.len());
    buf[from..]
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|p| from + p)
}

/// Incrementally frames Server-Sent Events out of a byte stream. Buffers
/// partial reads and yields each complete `(event_name, data)` pair as soon
/// as its `"\n\n"` boundary arrives.
///
/// Tracks a resume offset (see [`find_double_newline`]) so a boundary that
/// straddles many small reads — e.g. a single large event delivered as many
/// small network chunks — is not rescanned from the start of the buffer on
/// every read, which previously made framing an event of size `n` bytes
/// delivered in `k` chunks cost O(n*k) instead of O(n).
struct SseFramer {
    buffer: Vec<u8>,
    scanned: usize,
}

impl SseFramer {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
            scanned: 0,
        }
    }

    /// Feed newly-received bytes and append every complete event's
    /// `(event_name, data)` pair currently available to `out`.
    fn push(&mut self, chunk: &[u8], out: &mut Vec<(String, Value)>) {
        self.buffer.extend_from_slice(chunk);
        while let Some(boundary) = find_double_newline(&self.buffer, self.scanned) {
            // Parse from a borrowed slice of the buffer before draining it,
            // rather than copying the raw event text into an owned `String`
            // first — `parse_sse_event` already returns owned data, so the
            // intermediate copy was redundant.
            let event = std::str::from_utf8(&self.buffer[..boundary])
                .ok()
                .and_then(parse_sse_event);
            self.buffer.drain(..boundary + 2);
            self.scanned = 0;
            if let Some(pair) = event {
                out.push(pair);
            }
        }
        // No boundary in the buffered remainder: remember how much of it
        // we've already scanned (minus one byte, in case a "\n\n" boundary
        // straddles the next chunk's seam) so the next call only rescans
        // its tail instead of the whole buffer.
        self.scanned = self.buffer.len().saturating_sub(1);
    }
}

#[async_trait]
impl BackendProvider for AnthropicBackend {
    fn check_translated_chat(&self, request: &ChatCompletionRequest) -> Result<(), BackendError> {
        crate::translation::chat_controls::validate_anthropic_chat_request(request)
    }

    async fn send_anthropic_request(
        &self,
        endpoint: AnthropicRequestEndpoint,
        headers: &AnthropicRequestHeaders,
        body: bytes::Bytes,
    ) -> Result<reqwest::Response, BackendError> {
        let url = match endpoint {
            AnthropicRequestEndpoint::Messages => self.messages_url()?,
            AnthropicRequestEndpoint::CountTokens => self.count_tokens_url()?,
        };
        let mut request = self
            .client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("content-type", "application/json");
        for (name, value) in headers.iter() {
            request = request.header(name, value);
        }
        Ok(request.body(body).send().await?)
    }

    async fn create_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        crate::translation::chat_controls::validate_anthropic_chat_request(&request)?;
        let url = self.messages_url()?;

        // Convert messages to Anthropic format and hoist any system messages
        let (anthropic_messages, system) = Self::convert_messages(&request.messages);

        // Translate tool definitions and tool_choice
        let tools = request
            .tools
            .as_deref()
            .filter(|t| !t.is_empty())
            .map(Self::convert_tools);
        let tool_choice = tools.as_ref().and_then(|_| {
            Self::convert_tool_choice(
                request.tool_choice.as_ref(),
                request.parallel_tool_calls,
                tools.is_some(),
            )
        });

        let thinking = reasoning_controls(request.reasoning_effort.as_deref());
        let output_config = Self::native_output_config(request.response_format.as_ref())?;
        let structured = output_config.is_some();
        let output_config =
            Self::output_config(output_config, thinking.and_then(Thinking::effort))?;

        // Build Anthropic request
        let anthropic_req = AnthropicRequest {
            model: request.model.clone(),
            messages: anthropic_messages,
            max_tokens: anthropic_max_tokens(&request),
            system,
            temperature: request.temperature,
            top_p: request.top_p,
            stop_sequences: request.stop,
            tools,
            tool_choice,
            output_config,
            thinking,
            stream: None,
        };

        let mut req_builder = self
            .client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("Content-Type", "application/json");
        if structured {
            req_builder = req_builder.header("anthropic-beta", ANTHROPIC_STRUCTURED_OUTPUT_BETA);
        }
        let response = req_builder.json(&anthropic_req).send().await?;

        let status = response.status();

        if status.is_success() {
            let anthropic_resp = response
                .json::<AnthropicResponse>()
                .await
                .map_err(BackendError::from)?;
            let resp = Self::convert_response(anthropic_resp, request.model);
            Self::validate_structured_output(&resp, request.response_format.as_ref());
            Ok(resp)
        } else {
            Err(Self::chat_api_error(response, thinking).await)
        }
    }

    async fn create_chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<SseByteStream, BackendError> {
        crate::translation::chat_controls::validate_anthropic_chat_request(&request)?;
        let url = self.messages_url()?;
        let model = request.model.clone();

        let (anthropic_messages, system) = Self::convert_messages(&request.messages);
        let tools = request
            .tools
            .as_deref()
            .filter(|t| !t.is_empty())
            .map(Self::convert_tools);
        let tool_choice = tools.as_ref().and_then(|_| {
            Self::convert_tool_choice(
                request.tool_choice.as_ref(),
                request.parallel_tool_calls,
                tools.is_some(),
            )
        });

        let thinking = reasoning_controls(request.reasoning_effort.as_deref());
        let output_config = Self::native_output_config(request.response_format.as_ref())?;
        let structured = output_config.is_some();
        let output_config =
            Self::output_config(output_config, thinking.and_then(Thinking::effort))?;

        let anthropic_req = AnthropicRequest {
            model: request.model.clone(),
            messages: anthropic_messages,
            max_tokens: anthropic_max_tokens(&request),
            system,
            temperature: request.temperature,
            top_p: request.top_p,
            stop_sequences: request.stop,
            tools,
            tool_choice,
            output_config,
            thinking,
            stream: Some(true),
        };

        let mut req_builder = self
            .client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("Content-Type", "application/json");
        if structured {
            req_builder = req_builder.header("anthropic-beta", ANTHROPIC_STRUCTURED_OUTPUT_BETA);
        }
        let response = req_builder.json(&anthropic_req).send().await?;

        let status = response.status();
        if !status.is_success() {
            return Err(Self::chat_api_error(response, thinking).await);
        }

        let mut upstream = Box::pin(response.bytes_stream());
        let (tx, rx) = mpsc::channel::<Result<bytes::Bytes, BackendError>>(64);
        let created = chrono::Utc::now().timestamp();

        tokio::spawn(async move {
            let mut translator = AnthropicStreamTranslator::new(model, created);
            let mut framer = SseFramer::new();

            while let Some(chunk_result) = upstream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        let mut events = Vec::new();
                        framer.push(&chunk, &mut events);
                        for (event_name, data) in events {
                            for chunk_bytes in translator.handle_event(&event_name, &data) {
                                if tx.send(Ok(chunk_bytes)).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(BackendError::from(e))).await;
                        return;
                    }
                }
            }

            let _ = tx
                .send(Ok(bytes::Bytes::from_static(b"data: [DONE]\n\n")))
                .await;
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    async fn create_embedding(
        &self,
        _request: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(BackendError::api_status(
            400,
            "Anthropic does not support embeddings",
        ))
    }

    async fn create_image(
        &self,
        _request: CreateImageRequest,
    ) -> Result<ImageResponse, BackendError> {
        Err(BackendError::api_status(
            400,
            "Anthropic does not support image generation",
        ))
    }

    async fn create_speech(
        &self,
        _request: CreateSpeechRequest,
    ) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::api_status(
            400,
            "Anthropic does not support speech synthesis",
        ))
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
        Err(BackendError::api_status(
            400,
            "Anthropic does not support audio transcription",
        ))
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
        Err(BackendError::api_status(
            400,
            "Anthropic does not support audio translation",
        ))
    }

    async fn create_moderation(
        &self,
        _request: CreateModerationRequest,
    ) -> Result<CreateModerationResponse, BackendError> {
        Err(BackendError::api_status(
            400,
            "Anthropic does not support moderation",
        ))
    }

    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        self.list_models_all_pages().await
    }

    async fn probe_models(&self) -> Result<ListResponse<Model>, BackendError> {
        let AnthropicModelsPage {
            data: page_models, ..
        } = self.fetch_models_page(None).await?;
        let mut data = Vec::with_capacity(page_models.len());
        let mut diagnostics = ModelMappingDiagnostics::default();
        for record in page_models {
            let model_info = match serde_json::from_value::<AnthropicModelInfo>(record) {
                Ok(model_info) => model_info,
                Err(_) => {
                    diagnostics.skipped_invalid_id += 1;
                    continue;
                }
            };
            if let Some(model) = model_info.into_model(&mut diagnostics) {
                data.push(model);
            }
        }
        diagnostics.log("probe");
        Ok(ListResponse {
            object: "list".to_string(),
            data,
            first_id: None,
            last_id: None,
            has_more: false,
        })
    }

    async fn get_model(&self, model_id: &str) -> Result<Model, BackendError> {
        let url = self.model_url(model_id)?;
        let response = self
            .authenticated_get(url, self.model_detail_timeout)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response).await);
        }
        let model_info = response.json::<AnthropicModelInfo>().await?;
        let mut diagnostics = ModelMappingDiagnostics::default();
        let model = model_info.into_model(&mut diagnostics).ok_or_else(|| {
            BackendError::api_status(502, "Anthropic model response did not contain a usable id")
        })?;
        diagnostics.log("model_detail");
        Ok(model)
    }

    fn name(&self) -> &str {
        "anthropic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_support_rejections_match_the_controls_sent() {
        for (effort, message) in [
            ("high", "adaptive thinking is not supported on this model"),
            ("future-depth", "This model does not support the effort parameter."),
            ("xhigh", "This model does not support effort level 'xhigh'. Supported levels: high, low, max, medium."),
            ("minimal", "This model does not support effort level 'low'. Supported levels: medium, high."),
            ("none", r#""thinking.type.disabled" is not supported for this model. Use "thinking.type.adaptive" and "output_config.effort" to control thinking behavior."#),
            ("none", r#""thinking.type.disabled" is not supported for this model. Thinking defaults to adaptive mode when not specified; use "thinking.type.enabled" with "budget_tokens" for extended thinking."#),
            ("none", r#"To turn thinking off on this model, send "thinking": {"type": "between_tools"} instead of {"type": "disabled"}. The model does not think before responding. The short updates it writes between tool calls come back as thinking blocks."#),
        ] {
            let error = AnthropicErrorResponse {
                error_type: "error".into(),
                error: AnthropicErrorDetail {
                    error_type: "invalid_request_error".into(),
                    message: message.into(),
                },
            };
            assert!(
                reasoning_support_rejection(400, &error, reasoning_controls(Some(effort))),
                "{effort}: {message}"
            );
            assert!(!reasoning_support_rejection(400, &error, None), "{message}");
            if effort == "none" {
                assert!(
                    !reasoning_support_rejection(400, &error, reasoning_controls(Some("high"))),
                    "{message}"
                );
            }
        }
    }

    #[test]
    fn unrelated_or_ambiguous_reasoning_rejections_are_terminal() {
        for (message, effort) in [
            ("invalid output_config.effort", Some("bogus")),
            ("output_config.effort: Extra inputs are not permitted", Some("high")),
            ("This model does not support tool_choice with adaptive thinking", Some("high")),
            ("max_tokens is too small for thinking", Some("high")),
            ("messages.0: adaptive thinking is not supported on this model", Some("high")),
            ("adaptive thinking is not supported on this model; invalid schema", Some("high")),
            ("adaptive thinking is unsupported for an unknown reason", Some("high")),
            ("adaptive thinking is not supported on this model", None),
            ("adaptive thinking is not supported on this model", Some("none")),
            ("This model does not support the effort parameter.", Some("none")),
            ("This model does not support effort level 'xhigh'. Supported levels: high, low, max, medium.", Some("high")),
            ("This model does not support effort level 'minimal'. Supported levels: high, low, max, medium.", Some("minimal")),
            ("This model does not support effort level 'high'. Supported levels: high, low, max, medium.", Some("high")),
            ("This model does not support effort level 'xhigh'. Supported levels: high, low; invalid schema.", Some("xhigh")),
            ("output_config.effort 'xhigh' is not supported when thinking is disabled on this model. Use effort 'high' or below, or enable thinking.", Some("xhigh")),
        ] {
            let error = AnthropicErrorResponse {
                error_type: "error".into(),
                error: AnthropicErrorDetail {
                    error_type: "invalid_request_error".into(),
                    message: message.into(),
                },
            };
            assert!(
                !reasoning_support_rejection(400, &error, reasoning_controls(effort)),
                "{effort:?}: {message}"
            );
        }
    }

    #[test]
    fn reasoning_rejections_require_valid_status_and_error_envelope() {
        let message = "adaptive thinking is not supported on this model";
        for (status, envelope) in [
            (
                400,
                json!({"error":{"type":"invalid_request_error","message":message}}),
            ),
            (
                400,
                json!({"type":"future-error","error":{"type":"invalid_request_error","message":message}}),
            ),
            (400, json!({"type":"error","error":{"message":message}})),
            (
                400,
                json!({"type":"error","error":{"type":"authentication_error","message":message}}),
            ),
            (
                401,
                json!({"type":"error","error":{"type":"invalid_request_error","message":message}}),
            ),
            (
                422,
                json!({"type":"error","error":{"type":"invalid_request_error","message":message}}),
            ),
        ] {
            let eligible = serde_json::from_value::<AnthropicErrorResponse>(envelope.clone())
                .is_ok_and(|error| {
                    reasoning_support_rejection(status, &error, reasoning_controls(Some("high")))
                });
            assert!(!eligible, "{status}: {envelope}");
        }
    }

    #[tokio::test]
    async fn translated_chat_guards_controls_before_transport_in_both_modes() {
        // A bad transport configuration would fail first if validation were
        // omitted. These requests must be rejected without constructing it.
        let backend =
            AnthropicBackend::with_base_url(Client::new(), "unused".into(), "not a URL".into());
        for body in [
            r#"{"model":"claude-test","messages":[{"role":"user","content":"hello"}],"n":2}"#,
            r#"{"model":"claude-test","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,aGk="}}]}]}"#,
        ] {
            let request = serde_json::from_str(body).unwrap();
            let error = backend
                .create_native_chat_completion(request)
                .await
                .unwrap_err();
            assert!(matches!(error, BackendError::ApiError { status: 400, .. }));
            let request = serde_json::from_str(body).unwrap();
            let error = match backend.create_native_chat_completion_stream(request).await {
                Ok(_) => panic!("unrepresentable stream was accepted"),
                Err(error) => error,
            };
            assert!(matches!(error, BackendError::ApiError { status: 400, .. }));
        }
    }

    #[test]
    fn anthropic_base_url_accepts_loopback_and_public_endpoints() {
        for base in [
            "http://127.0.0.1:18000/v1",
            "http://[::1]:18000/v1",
            "http://localhost:18000/v1",
            "http://localhost:18000/v1/",
            "http://localhost:18001/v1",
            "http://localhost:18000/other",
            "https://localhost:18000/v1",
            "https://api.anthropic.com/v1",
        ] {
            assert!(validate_anthropic_base_url(Some(base)).is_ok(), "{base}");
        }
        assert!(validate_anthropic_base_url(None).is_ok());
        assert!(validate_anthropic_base_url(Some("  ")).is_ok());
    }

    #[test]
    fn anthropic_base_url_rejects_private_and_malformed_urls() {
        for invalid in [
            "http://mock.localhost:18000/v1",
            "http://10.0.0.1/v1",
            "http://169.254.169.254/v1",
            "http://[fd00::1]/v1",
            "relative/path",
            "http://user:pass@127.0.0.1/v1",
            "http://127.0.0.1/v1?query=x",
            "http://127.0.0.1/v1#fragment",
            "ftp://127.0.0.1/v1",
        ] {
            assert!(
                validate_anthropic_base_url(Some(invalid)).is_err(),
                "{invalid}"
            );
        }
    }
    #[tokio::test]
    async fn localhost_native_requests_reach_ipv4_mock_without_proxy_or_redirects() {
        check_localhost_native_requests("127.0.0.1:0").await;
    }

    #[tokio::test]
    async fn localhost_native_requests_reach_ipv6_mock_without_proxy_or_redirects() {
        check_localhost_native_requests("[::1]:0").await;
    }

    async fn check_localhost_native_requests(bind_address: &str) {
        use axum::response::IntoResponse;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let redirect_calls = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route(
                "/v1/messages",
                axum::routing::post(
                    |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                        assert_eq!(headers["x-api-key"], "unused");
                        assert!(headers["host"].to_str().unwrap().starts_with("localhost:"));
                        if body["redirect"] == true {
                            axum::response::Redirect::temporary("/secret-target").into_response()
                        } else {
                            axum::Json(body).into_response()
                        }
                    },
                ),
            )
            .route(
                "/secret-target",
                axum::routing::post({
                    let redirect_calls = redirect_calls.clone();
                    move || async move {
                        redirect_calls.fetch_add(1, Ordering::SeqCst);
                        axum::Json(json!({}))
                    }
                }),
            );
        let listener = match tokio::net::TcpListener::bind(bind_address).await {
            Ok(listener) => listener,
            // Some CI network namespaces have no IPv6 loopback address. Keep
            // IPv4 coverage mandatory and run IPv6 coverage wherever available.
            Err(error)
                if bind_address == "[::1]:0"
                    && error.kind() == std::io::ErrorKind::AddrNotAvailable =>
            {
                eprintln!("Skipping IPv6 loopback test: {error}");
                return;
            }
            Err(error) => panic!("could not bind local mock at {bind_address}: {error}"),
        };
        let base_url = format!(
            "http://localhost:{}/v1",
            listener.local_addr().unwrap().port()
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // The local backend must replace the caller's proxy-enabled client.
        let proxied_client = Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap();
        let backend =
            AnthropicBackend::with_base_url(proxied_client, "unused".to_string(), base_url);
        let body =
            json!({"model": "mock/fast", "messages": [{"role": "user", "content": "hello"}]});
        let response = backend
            .send_anthropic_request(
                AnthropicRequestEndpoint::Messages,
                &AnthropicRequestHeaders::default(),
                body.to_string().into(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.json::<Value>().await.unwrap(), body);

        let response = backend
            .send_anthropic_request(
                AnthropicRequestEndpoint::Messages,
                &AnthropicRequestHeaders::default(),
                json!({"redirect": true}).to_string().into(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(redirect_calls.load(Ordering::SeqCst), 0);
        server.abort();
    }

    use crate::types::{ChatMessage, FunctionCall, Tool, ToolCall, ToolFunction};

    fn user_msg(text: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_string(),
            content: Some(json!(text)),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn system_msg(text: &str) -> ChatMessage {
        ChatMessage {
            role: "system".to_string(),
            content: Some(json!(text)),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    #[tokio::test]
    async fn model_requests_have_a_deadline() {
        async fn slow_model() -> axum::Json<Value> {
            tokio::time::sleep(Duration::from_millis(100)).await;
            axum::Json(json!({
                "type": "model",
                "id": "claude-slow",
                "created_at": "2026-07-24T00:00:00Z"
            }))
        }

        let app = axum::Router::new().route("/models/:model_id", axum::routing::get(slow_model));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let backend = AnthropicBackend::with_request_timeout(
            Client::new(),
            "test-key".to_string(),
            format!("http://{addr}"),
            Duration::from_millis(10),
        );

        let error = backend
            .get_model("claude-slow")
            .await
            .expect_err("request deadline");

        match error {
            BackendError::RequestError(error) => assert!(error.is_timeout()),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_pages_have_a_shorter_deadline_than_the_catalog_walk() {
        async fn slow_page() -> axum::Json<Value> {
            tokio::time::sleep(Duration::from_millis(100)).await;
            axum::Json(json!({"data": [], "has_more": false, "last_id": null}))
        }

        let app = axum::Router::new().route("/models", axum::routing::get(slow_page));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let base_url = format!("http://{addr}");
        let backend = AnthropicBackend::with_model_timeouts(
            Client::new(),
            "test-key".to_string(),
            Some(&base_url),
            Duration::from_millis(10),
            Duration::from_millis(100),
            Duration::from_millis(200),
        );

        let error = backend.list_models().await.expect_err("page deadline");

        match error {
            BackendError::RequestError(error) => assert!(error.is_timeout()),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_pages_enforce_a_response_size_limit() {
        async fn oversized_page() -> axum::response::Response {
            axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .body(axum::body::Body::from(vec![
                    b'x';
                    ANTHROPIC_MODEL_PAGE_MAX_BYTES
                        + 1
                ]))
                .unwrap()
        }

        let app = axum::Router::new().route("/models", axum::routing::get(oversized_page));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let backend = AnthropicBackend::with_base_url(
            Client::new(),
            "test-key".to_string(),
            format!("http://{addr}"),
        );

        let error = backend.list_models().await.expect_err("oversized response");

        assert!(matches!(&error, BackendError::ApiError { status: 502, .. }));
        assert!(error.to_string().contains("response limit"));
    }

    #[tokio::test(start_paused = true)]
    async fn catalog_walk_has_one_overall_deadline() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // Page and detail timeouts are irrelevant here (no HTTP happens); only the
        // 30 ms catalog deadline matters. Each page takes 20 ms of virtual time, so
        // page 1 lands at t=20ms and the deadline cuts page 2 at t=30ms.
        let backend = AnthropicBackend::with_model_timeouts(
            Client::new(),
            "test-key".to_string(),
            None,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_millis(30),
        );
        let calls = Arc::new(AtomicUsize::new(0));

        let catalog = backend
            .walk_model_catalog(|after_id| {
                let calls = calls.clone();
                async move {
                    let index = calls.fetch_add(1, Ordering::SeqCst);
                    let expected_cursor = index.checked_sub(1).map(|prev| format!("cursor-{prev}"));
                    assert_eq!(after_id, expected_cursor);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Ok(AnthropicModelsPage {
                        data: vec![json!({
                            "type": "model",
                            "id": format!("claude-page-{index}"),
                            "created_at": "2026-07-24T00:00:00Z"
                        })],
                        last_id: Some(format!("cursor-{index}")),
                        has_more: true,
                    })
                }
            })
            .await
            .expect("useful pages survive the catalog deadline");

        assert_eq!(catalog.data.len(), 1);
        assert_eq!(catalog.data[0].id, "claude-page-0");
        assert!(!catalog.has_more);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn catalog_deadline_before_any_page_is_a_504() {
        let backend = AnthropicBackend::with_model_timeouts(
            Client::new(),
            "test-key".to_string(),
            None,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_millis(30),
        );

        let error = backend
            .walk_model_catalog(|_after_id| async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(AnthropicModelsPage {
                    data: Vec::new(),
                    last_id: None,
                    has_more: false,
                })
            })
            .await
            .expect_err("deadline before the first page");

        assert!(
            matches!(&error, BackendError::ApiError { status: 504, .. }),
            "{error:?}"
        );
        assert!(error
            .to_string()
            .contains("Timed out retrieving the Anthropic model catalog"));
    }

    #[test]
    fn messages_and_models_share_url_path_construction() {
        let backend = AnthropicBackend::with_base_url(
            Client::new(),
            "test-key".to_string(),
            "https://gateway.example/anthropic/v1/".to_string(),
        );

        assert_eq!(
            backend.messages_url().unwrap().as_str(),
            "https://gateway.example/anthropic/v1/messages"
        );
        assert_eq!(
            backend.models_url().unwrap().as_str(),
            "https://gateway.example/anthropic/v1/models"
        );
        assert_eq!(
            backend.model_url("alias/with/slash").unwrap().as_str(),
            "https://gateway.example/anthropic/v1/models/alias%2Fwith%2Fslash"
        );
        assert_eq!(
            backend.endpoints().unwrap().count_tokens_url().as_str(),
            "https://gateway.example/anthropic/v1/messages/count_tokens"
        );
    }

    #[test]
    fn public_endpoint_validation_uses_the_canonical_anthropic_parser() {
        let canonical = AnthropicEndpoints::from_base_url(Some("  \t ")).unwrap();
        let validated = AnthropicEndpoints::from_public_base_url(Some("  \t ")).unwrap();
        assert_eq!(canonical.base, validated.base);

        for private in [
            "http://localhost:8080/v1",
            "http://service.localhost/v1",
            "http://127.0.0.1/v1",
            "http://10.0.0.1/v1",
            "http://169.254.169.254/v1",
            "http://[::1]/v1",
            "http://[fd00::1]/v1",
            "http://[fe80::1]/v1",
            "http://[::ffff:127.0.0.1]/v1",
        ] {
            let error = AnthropicEndpoints::from_public_base_url(Some(private))
                .expect_err("non-public Anthropic endpoint");
            assert!(error.to_string().contains("must use a public host"));
        }
    }

    #[test]
    fn convert_messages_hoists_system_into_top_level_system() {
        let msgs = vec![system_msg("you are helpful"), user_msg("hello")];
        let (out, system) = AnthropicBackend::convert_messages(&msgs);
        assert_eq!(out.len(), 1, "system message must not appear in messages");
        assert_eq!(out[0].role, "user");
        assert_eq!(out[0].content, json!("hello"));
        assert_eq!(system.as_deref(), Some("you are helpful"));
    }

    #[test]
    fn convert_messages_hoists_developer_like_system_and_defaults_unusable_tool_arguments() {
        let developer = ChatMessage {
            role: "developer".to_string(),
            content: Some(json!([{"type":"text","text":"be brief"}])),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        };
        let calls = ["", "not-json", "[1]"]
            .into_iter()
            .enumerate()
            .map(|(index, arguments)| ToolCall {
                id: format!("call_{index}"),
                tool_type: "function".to_string(),
                function: FunctionCall {
                    name: "f".to_string(),
                    arguments: arguments.to_string(),
                },
            })
            .collect();
        let assistant = ChatMessage {
            role: "assistant".to_string(),
            content: None,
            name: None,
            tool_calls: Some(calls),
            tool_call_id: None,
        };
        let (out, system) =
            AnthropicBackend::convert_messages(&[developer, user_msg("hi"), assistant]);
        assert_eq!(system.as_deref(), Some("be brief"));
        assert_eq!(out.len(), 2);
        for block in out[1].content.as_array().unwrap() {
            assert_eq!(block["input"], json!({}));
        }
    }

    #[test]
    fn convert_messages_concatenates_multiple_system_messages() {
        let msgs = vec![
            system_msg("rule one"),
            system_msg("rule two"),
            user_msg("hi"),
        ];
        let (_, system) = AnthropicBackend::convert_messages(&msgs);
        assert_eq!(system.as_deref(), Some("rule one\n\nrule two"));
    }

    #[test]
    fn convert_messages_translates_assistant_tool_calls_into_tool_use_blocks() {
        let assistant = ChatMessage {
            role: "assistant".to_string(),
            content: Some(json!("let me look that up")),
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_abc".to_string(),
                tool_type: "function".to_string(),
                function: FunctionCall {
                    name: "list_files".to_string(),
                    arguments: r#"{"path":"/tmp"}"#.to_string(),
                },
            }]),
            tool_call_id: None,
        };
        let (out, _) = AnthropicBackend::convert_messages(&[assistant]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "assistant");
        let blocks = out[0]
            .content
            .as_array()
            .expect("assistant content must be a block array");
        assert_eq!(blocks.len(), 2, "expected one text and one tool_use block");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "let me look that up");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "call_abc");
        assert_eq!(blocks[1]["name"], "list_files");
        assert_eq!(blocks[1]["input"], json!({"path": "/tmp"}));
    }

    #[test]
    fn convert_messages_translates_tool_role_into_user_with_tool_result() {
        let tool_msg = ChatMessage {
            role: "tool".to_string(),
            content: Some(json!("file1.txt\nfile2.txt")),
            name: None,
            tool_calls: None,
            tool_call_id: Some("call_abc".to_string()),
        };
        let (out, _) = AnthropicBackend::convert_messages(&[tool_msg]);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].role, "user",
            "tool-role messages must be sent as user"
        );
        let blocks = out[0]
            .content
            .as_array()
            .expect("tool message must produce a block array");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "call_abc");
        assert_eq!(blocks[0]["content"], "file1.txt\nfile2.txt");
    }

    #[test]
    fn convert_tools_maps_openai_function_to_anthropic_tool() {
        let tools = vec![Tool {
            tool_type: "function".to_string(),
            function: ToolFunction {
                name: "bash".to_string(),
                description: Some("run a shell command".to_string()),
                parameters: Some(
                    json!({
                        "type": "object",
                        "properties": {"command": {"type": "string"}},
                        "required": ["command"],
                    })
                    .into(),
                ),
            },
        }];
        let out = AnthropicBackend::convert_tools(&tools);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "bash");
        assert_eq!(out[0].description.as_deref(), Some("run a shell command"));
        assert_eq!(
            out[0].input_schema["properties"]["command"]["type"],
            "string"
        );
    }

    #[test]
    fn convert_tool_choice_maps_string_modes_and_named_function() {
        assert_eq!(
            AnthropicBackend::convert_tool_choice(Some(&json!("auto")), None, true),
            Some(json!({"type": "auto"})),
        );
        assert_eq!(
            AnthropicBackend::convert_tool_choice(Some(&json!("required")), None, true),
            Some(json!({"type": "any"})),
        );
        assert_eq!(
            AnthropicBackend::convert_tool_choice(Some(&json!("none")), None, true),
            Some(json!({"type": "none"})),
        );
        assert_eq!(
            AnthropicBackend::convert_tool_choice(
                Some(&json!({
                    "type": "function",
                    "function": {"name": "bash"}
                })),
                None,
                true
            ),
            Some(json!({"type": "tool", "name": "bash"})),
        );
        assert_eq!(
            AnthropicBackend::convert_tool_choice(None, None, true),
            None
        );
    }

    #[test]
    fn convert_tool_choice_maps_parallel_tool_calls_false_to_disable_parallel_tool_use() {
        assert_eq!(
            AnthropicBackend::convert_tool_choice(Some(&json!("auto")), Some(false), true),
            Some(json!({"type": "auto", "disable_parallel_tool_use": true})),
        );
        assert_eq!(
            AnthropicBackend::convert_tool_choice(Some(&json!("required")), Some(false), true),
            Some(json!({"type": "any", "disable_parallel_tool_use": true})),
        );
        assert_eq!(
            AnthropicBackend::convert_tool_choice(
                Some(&json!({
                    "type": "function",
                    "function": {"name": "bash"}
                })),
                Some(false),
                true
            ),
            Some(json!({"type": "tool", "name": "bash", "disable_parallel_tool_use": true})),
        );
        // "none" disables tool use entirely; Anthropic does not accept
        // disable_parallel_tool_use alongside it, so it is left out.
        assert_eq!(
            AnthropicBackend::convert_tool_choice(Some(&json!("none")), Some(false), true),
            Some(json!({"type": "none"})),
        );
        // parallel_tool_calls: true (or unset) leaves the mapping unchanged.
        assert_eq!(
            AnthropicBackend::convert_tool_choice(Some(&json!("auto")), Some(true), true),
            Some(json!({"type": "auto"})),
        );
        assert_eq!(
            AnthropicBackend::convert_tool_choice(None, Some(false), true),
            Some(json!({"type": "auto", "disable_parallel_tool_use": true})),
        );
        assert_eq!(
            AnthropicBackend::convert_tool_choice(None, Some(false), false),
            None,
        );
    }

    #[test]
    fn convert_response_emits_tool_calls_for_tool_use_blocks() {
        let raw = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5-20250929",
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5},
            "content": [
                {"type": "text", "text": "I'll list the files."},
                {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": "ls"}}
            ]
        });
        let resp: AnthropicResponse = serde_json::from_value(raw)
            .expect("AnthropicResponse must deserialize tool_use blocks");
        let converted = AnthropicBackend::convert_response(resp, "claude-sonnet-4-5".to_string());
        assert_eq!(converted.choices.len(), 1);
        let choice = &converted.choices[0];
        assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));
        let tool_calls = choice
            .message
            .tool_calls
            .as_ref()
            .expect("tool_use blocks must produce tool_calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "toolu_1");
        assert_eq!(tool_calls[0].tool_type, "function");
        assert_eq!(tool_calls[0].function.name, "bash");
        let parsed_args: Value = serde_json::from_str(&tool_calls[0].function.arguments)
            .expect("function arguments must be a JSON string");
        assert_eq!(parsed_args, json!({"command": "ls"}));
        assert_eq!(
            choice.message.content,
            Some(json!("I'll list the files.")),
            "text blocks must still appear in message.content",
        );
    }

    #[test]
    fn convert_response_handles_text_only_response() {
        let raw = json!({
            "id": "msg_2",
            "type": "message",
            "role": "assistant",
            "model": "claude-haiku-4-5",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 4},
            "content": [{"type": "text", "text": "hi there"}]
        });
        let resp: AnthropicResponse = serde_json::from_value(raw).unwrap();
        let converted = AnthropicBackend::convert_response(resp, "claude-haiku-4-5".to_string());
        let choice = &converted.choices[0];
        assert_eq!(choice.finish_reason.as_deref(), Some("stop"));
        assert!(choice.message.tool_calls.is_none());
        assert_eq!(choice.message.content, Some(json!("hi there")));
    }

    /// Helper: parse a chunk byte payload (`data: {...}\n\n`) to its inner JSON.
    fn parse_chunk(b: &bytes::Bytes) -> Value {
        let s = std::str::from_utf8(b).expect("utf8 chunk");
        let payload = s
            .strip_prefix("data: ")
            .expect("chunk must start with 'data: '")
            .trim_end_matches("\n\n");
        serde_json::from_str(payload).expect("chunk JSON")
    }

    #[test]
    fn translator_emits_role_then_content_then_finish() {
        let mut t = AnthropicStreamTranslator::new("claude-test".to_string(), 1234);

        let role = t.handle_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": "msg_x",
                    "role": "assistant",
                    "usage": {"input_tokens": 5},
                }
            }),
        );
        assert_eq!(role.len(), 1);
        let role_json = parse_chunk(&role[0]);
        assert_eq!(role_json["object"], "chat.completion.chunk");
        assert_eq!(role_json["id"], "msg_x");
        assert_eq!(role_json["choices"][0]["delta"]["role"], "assistant");

        let content = t.handle_event(
            "content_block_delta",
            &json!({
                "index": 0,
                "delta": {"type": "text_delta", "text": "hello"}
            }),
        );
        assert_eq!(content.len(), 1);
        let content_json = parse_chunk(&content[0]);
        assert_eq!(content_json["choices"][0]["delta"]["content"], "hello");
        assert!(content_json["choices"][0]["finish_reason"].is_null());

        let _ = t.handle_event(
            "message_delta",
            &json!({
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 7}
            }),
        );

        let stop = t.handle_event("message_stop", &json!({"type": "message_stop"}));
        assert_eq!(stop.len(), 1);
        let stop_json = parse_chunk(&stop[0]);
        assert_eq!(stop_json["choices"][0]["finish_reason"], "stop");
        assert_eq!(stop_json["usage"]["prompt_tokens"], 5);
        assert_eq!(stop_json["usage"]["completion_tokens"], 7);
        assert_eq!(stop_json["usage"]["total_tokens"], 12);
    }

    #[test]
    fn translator_counts_cache_tokens_in_streamed_usage() {
        let mut t = AnthropicStreamTranslator::new("claude-test".to_string(), 1234);

        let _ = t.handle_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": "msg_cache",
                    "role": "assistant",
                    "usage": {
                        "input_tokens": 5,
                        "cache_read_input_tokens": 20,
                        "cache_creation_input_tokens": 30,
                    },
                }
            }),
        );

        let _ = t.handle_event(
            "content_block_delta",
            &json!({
                "index": 0,
                "delta": {"type": "text_delta", "text": "hi"}
            }),
        );

        let _ = t.handle_event(
            "message_delta",
            &json!({
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 7}
            }),
        );

        let stop = t.handle_event("message_stop", &json!({"type": "message_stop"}));
        assert_eq!(stop.len(), 1);
        let stop_json = parse_chunk(&stop[0]);
        // prompt_tokens must include cache-read and cache-write tokens, matching
        // the non-streaming translator (input + cache_read + cache_creation).
        assert_eq!(stop_json["usage"]["prompt_tokens"], 55);
        assert_eq!(stop_json["usage"]["completion_tokens"], 7);
        assert_eq!(stop_json["usage"]["total_tokens"], 62);
        assert_eq!(
            stop_json["usage"]["prompt_tokens_details"]["cached_tokens"],
            20
        );
        assert_eq!(
            stop_json["usage"]["prompt_tokens_details"]["cache_write_tokens"],
            30
        );
    }

    #[test]
    fn translator_emits_tool_call_id_name_and_argument_deltas() {
        let mut t = AnthropicStreamTranslator::new("claude-test".to_string(), 1234);
        let _ = t.handle_event(
            "message_start",
            &json!({"message": {"id": "msg_y", "role": "assistant", "usage": {"input_tokens": 1}}}),
        );

        // Tool block start: should emit a tool_calls delta with id, name, empty arguments.
        let start = t.handle_event(
            "content_block_start",
            &json!({
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "bash",
                    "input": {},
                }
            }),
        );
        assert_eq!(start.len(), 1);
        let s_json = parse_chunk(&start[0]);
        let tc = &s_json["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["index"], 0);
        assert_eq!(tc["id"], "toolu_1");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "bash");
        assert_eq!(tc["function"]["arguments"], "");

        // Argument streaming: input_json_delta carries partial JSON strings.
        let arg1 = t.handle_event(
            "content_block_delta",
            &json!({
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{\"command\":"}
            }),
        );
        assert_eq!(arg1.len(), 1);
        let a1 = parse_chunk(&arg1[0]);
        let tc1 = &a1["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc1["index"], 0);
        assert_eq!(tc1["function"]["arguments"], "{\"command\":");

        let arg2 = t.handle_event(
            "content_block_delta",
            &json!({
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "\"ls\"}"}
            }),
        );
        let a2 = parse_chunk(&arg2[0]);
        assert_eq!(
            a2["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "\"ls\"}"
        );

        let _ = t.handle_event(
            "message_delta",
            &json!({"delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 4}}),
        );
        let stop = t.handle_event("message_stop", &json!({}));
        let s = parse_chunk(&stop[0]);
        assert_eq!(s["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn translator_assigns_distinct_indices_to_concurrent_tool_calls() {
        let mut t = AnthropicStreamTranslator::new("m".to_string(), 0);
        let _ = t.handle_event(
            "message_start",
            &json!({"message": {"id": "msg_z", "role": "assistant", "usage": {"input_tokens": 0}}}),
        );

        // Anthropic block 0 = first tool, block 1 = second tool.
        let _ = t.handle_event(
            "content_block_start",
            &json!({
                "index": 0,
                "content_block": {"type": "tool_use", "id": "tu_a", "name": "bash", "input": {}}
            }),
        );
        let _ = t.handle_event(
            "content_block_start",
            &json!({
                "index": 1,
                "content_block": {"type": "tool_use", "id": "tu_b", "name": "ls", "input": {}}
            }),
        );

        let d = t.handle_event(
            "content_block_delta",
            &json!({
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": "x"}
            }),
        );
        let chunk = parse_chunk(&d[0]);
        // Second tool must come through with OpenAI tool_calls index 1.
        assert_eq!(chunk["choices"][0]["delta"]["tool_calls"][0]["index"], 1);
    }

    #[test]
    fn parse_sse_event_extracts_event_and_data() {
        let raw = "event: message_start\ndata: {\"a\":1}";
        let (name, val) = parse_sse_event(raw).expect("parses");
        assert_eq!(name, "message_start");
        assert_eq!(val, json!({"a": 1}));
    }

    #[test]
    fn parse_sse_event_returns_none_when_no_data() {
        // A ping event has only `event:` line, no parsable data.
        assert!(parse_sse_event("event: ping").is_none());
    }

    #[test]
    fn sse_framer_yields_identical_events_regardless_of_chunk_boundaries() {
        let raw = "event: message_start\ndata: {\"a\":1}\n\nevent: content_block_delta\ndata: {\"b\":2}\n\nevent: message_stop\ndata: {}\n\n";

        // Fed as a single, whole chunk.
        let mut whole = SseFramer::new();
        let mut from_whole = Vec::new();
        whole.push(raw.as_bytes(), &mut from_whole);

        // Fed one byte at a time, so every "\n\n" boundary — and plenty of
        // non-boundary bytes — straddle a chunk seam.
        let mut chunked = SseFramer::new();
        let mut from_chunked = Vec::new();
        for byte in raw.as_bytes() {
            chunked.push(std::slice::from_ref(byte), &mut from_chunked);
        }

        assert_eq!(
            from_whole, from_chunked,
            "byte-at-a-time delivery must frame the same events as a single chunk"
        );
        assert_eq!(from_whole.len(), 3);
        assert_eq!(
            from_whole[0],
            ("message_start".to_string(), json!({"a": 1}))
        );
        assert_eq!(
            from_whole[1],
            ("content_block_delta".to_string(), json!({"b": 2}))
        );
        assert_eq!(from_whole[2], ("message_stop".to_string(), json!({})));
    }

    // --- Structured output (`response_format`) via native output_config ------

    #[test]
    fn native_output_config_maps_json_schema() {
        let rf = json!({
            "type": "json_schema",
            "json_schema": {
                "name": "weather",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": {"temp": {"type": "number"}},
                    "required": ["temp"]
                }
            }
        });
        let cfg = AnthropicBackend::native_output_config(Some(&rf.into()))
            .unwrap()
            .expect("json_schema must produce an output_config");
        assert_eq!(cfg["format"]["type"], "json_schema");
        assert_eq!(
            cfg["format"]["schema"]["properties"]["temp"]["type"],
            "number"
        );
        assert_eq!(cfg["format"]["schema"]["required"], json!(["temp"]));
    }

    #[test]
    fn native_output_config_maps_json_object_to_permissive_schema() {
        let cfg =
            AnthropicBackend::native_output_config(Some(&json!({"type": "json_object"}).into()))
                .unwrap()
                .expect("json_object must produce an output_config");
        assert_eq!(cfg["format"]["type"], "json_schema");
        assert_eq!(cfg["format"]["schema"], json!({"type": "object"}));
    }

    #[test]
    fn json_object_ignores_unrelated_schema_configuration() {
        let format = json!({"type":"json_object","json_schema":null}).into();
        let config = AnthropicBackend::native_output_config(Some(&format))
            .unwrap()
            .unwrap();
        assert_eq!(config["format"]["schema"], json!({"type":"object"}));
    }

    #[test]
    fn native_output_config_preserves_legacy_schema_defaults() {
        for config in [Value::Null, json!(42), json!("odd"), json!([]), json!({})] {
            let format = json!({"type":"json_schema","json_schema":config}).into();
            let output = AnthropicBackend::native_output_config(Some(&format))
                .unwrap()
                .unwrap();
            assert_eq!(output["format"]["schema"], json!({"type":"object"}));
        }
        for schema in [Value::Null, json!(42), json!([])] {
            let format = json!({"type":"json_schema","json_schema":{"schema":schema}}).into();
            let output = AnthropicBackend::native_output_config(Some(&format))
                .unwrap()
                .unwrap();
            assert_eq!(output["format"]["schema"], schema);
        }
        let absent = json!({"type":"json_schema"}).into();
        assert!(AnthropicBackend::native_output_config(Some(&absent))
            .unwrap()
            .is_none());
    }

    #[test]
    fn native_output_config_none_without_response_format() {
        assert!(AnthropicBackend::native_output_config(None)
            .unwrap()
            .is_none());
        assert!(
            AnthropicBackend::native_output_config(Some(&json!({"type": "text"}).into()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn native_structured_output_returns_json_as_content() {
        // Native structured output comes back as an ordinary text block, so the
        // existing text-block handling already surfaces it as message.content.
        let raw = json!({
            "id": "msg_j",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5},
            "content": [{"type": "text", "text": "{\"temp\":21}"}]
        });
        let resp: AnthropicResponse = serde_json::from_value(raw).unwrap();
        let converted = AnthropicBackend::convert_response(resp, "claude-sonnet-4-5".to_string());
        let choice = &converted.choices[0];
        assert_eq!(choice.finish_reason.as_deref(), Some("stop"));
        assert!(choice.message.tool_calls.is_none());
        let content = choice
            .message
            .content
            .as_ref()
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(content).unwrap(),
            json!({"temp": 21})
        );
    }
}
