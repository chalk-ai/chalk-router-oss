use super::openai_compatible::OpenAICompatibleBackend;

/// Cohere's OpenAI-compatible API endpoint.
pub const COHERE_OPENAI_BASE_URL: &str = "https://api.cohere.ai/compatibility/v1";

/// Build a Cohere backend using its OpenAI-compatible API.
///
/// Cohere accepts the API key as a bearer token, which is the authentication
/// scheme used by [`OpenAICompatibleBackend`].
pub fn cohere_backend(
    http_client: reqwest::Client,
    api_key: String,
    base_url_override: Option<&str>,
) -> OpenAICompatibleBackend {
    let base_url = crate::provider_registry::resolve_base_url("cohere", base_url_override)
        .expect("cohere has a registry default base URL");
    OpenAICompatibleBackend::for_provider(http_client, "cohere", api_key, base_url)
}
