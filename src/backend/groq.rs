use super::openai_compatible::OpenAICompatibleBackend;

/// Groq's OpenAI-compatible API endpoint.
pub const GROQ_API_BASE_URL: &str = "https://api.groq.com/openai/v1";

/// Build a Groq backend using its OpenAI-compatible API.
pub fn groq_backend(
    http_client: reqwest::Client,
    api_key: String,
    base_url_override: Option<&str>,
) -> OpenAICompatibleBackend {
    let base_url = crate::provider_registry::resolve_base_url("groq", base_url_override)
        .expect("groq has a registry default base URL");
    OpenAICompatibleBackend::for_provider(http_client, "groq", api_key, base_url)
}
