use super::openai_compatible::OpenAICompatibleBackend;

/// Perplexity's API root: OpenAI-compatible chat at `/chat/completions`, and
/// System One decisions at `/v1/decisions`.
pub const PERPLEXITY_API_BASE_URL: &str = "https://api.perplexity.ai";

/// Build a Perplexity backend using its OpenAI-compatible API.
pub fn perplexity_backend(
    http_client: reqwest::Client,
    api_key: String,
    base_url_override: Option<&str>,
) -> OpenAICompatibleBackend {
    let base_url = crate::provider_registry::resolve_base_url("perplexity", base_url_override)
        .expect("perplexity has a registry default base URL");
    OpenAICompatibleBackend::for_provider(http_client, "perplexity", api_key, base_url)
}
