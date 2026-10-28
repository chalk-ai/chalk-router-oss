use super::openai_compatible::OpenAICompatibleBackend;

/// Together AI's OpenAI-compatible API endpoint.
pub const TOGETHER_API_BASE_URL: &str = "https://api.together.xyz/v1";

/// Build a Together AI backend using its OpenAI-compatible API.
pub fn together_backend(
    http_client: reqwest::Client,
    api_key: String,
    base_url_override: Option<&str>,
) -> OpenAICompatibleBackend {
    let base_url = crate::provider_registry::resolve_base_url("together", base_url_override)
        .expect("together has a registry default base URL");
    OpenAICompatibleBackend::for_provider(http_client, "together", api_key, base_url)
}
