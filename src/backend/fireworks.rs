use super::openai_compatible::OpenAICompatibleBackend;

/// Fireworks AI's OpenAI-compatible API endpoint.
pub const FIREWORKS_API_BASE_URL: &str = "https://api.fireworks.ai/inference/v1";

/// Build a Fireworks AI backend using its OpenAI-compatible API.
pub fn fireworks_backend(
    http_client: reqwest::Client,
    api_key: String,
    base_url_override: Option<&str>,
) -> OpenAICompatibleBackend {
    let base_url = crate::provider_registry::resolve_base_url("fireworks", base_url_override)
        .expect("fireworks has a registry default base URL");
    OpenAICompatibleBackend::for_provider(http_client, "fireworks", api_key, base_url.clone())
        .with_anthropic_base_url(base_url)
}
