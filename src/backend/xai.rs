use super::openai_compatible::OpenAICompatibleBackend;

/// xAI's OpenAI-compatible API endpoint.
pub const XAI_OPENAI_BASE_URL: &str = "https://api.x.ai/v1";

/// Build an xAI backend using its OpenAI-compatible API.
///
/// xAI accepts its API key as a bearer token, which is the authentication
/// scheme used by [`OpenAICompatibleBackend`].
pub fn xai_backend(
    http_client: reqwest::Client,
    api_key: String,
    base_url_override: Option<&str>,
) -> OpenAICompatibleBackend {
    let base_url = crate::provider_registry::resolve_base_url("xai", base_url_override)
        .expect("xai has a registry default base URL");
    OpenAICompatibleBackend::for_provider(http_client, "xai", api_key, base_url)
}
