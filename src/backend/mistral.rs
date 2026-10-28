use super::openai_compatible::OpenAICompatibleBackend;

/// Mistral AI's OpenAI-compatible API endpoint.
pub const MISTRAL_API_BASE_URL: &str = "https://api.mistral.ai/v1";

/// Build a Mistral AI backend using its OpenAI-compatible API.
pub fn mistral_backend(
    http_client: reqwest::Client,
    api_key: String,
    base_url_override: Option<&str>,
) -> OpenAICompatibleBackend {
    let base_url = crate::provider_registry::resolve_base_url("mistral", base_url_override)
        .expect("mistral has a registry default base URL");
    OpenAICompatibleBackend::for_provider(http_client, "mistral", api_key, base_url)
}
