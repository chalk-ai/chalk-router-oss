use super::openai_compatible::OpenAICompatibleBackend;

/// OpenRouter's OpenAI-compatible API endpoint.
pub const OPENROUTER_API_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Build an OpenRouter backend using its OpenAI-compatible API.
///
/// OpenRouter accepts the API key as a bearer token, which is the
/// authentication scheme used by [`OpenAICompatibleBackend`].
pub fn openrouter_backend(
    http_client: reqwest::Client,
    api_key: String,
    base_url_override: Option<&str>,
) -> OpenAICompatibleBackend {
    let base_url = crate::provider_registry::resolve_base_url("openrouter", base_url_override)
        .expect("openrouter has a registry default base URL");
    OpenAICompatibleBackend::for_provider(http_client, "openrouter", api_key, base_url.clone())
        .with_anthropic_base_url(base_url)
}
