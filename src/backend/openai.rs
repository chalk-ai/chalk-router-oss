use super::openai_compatible::OpenAICompatibleBackend;

/// OpenAI's API endpoint.
pub const OPENAI_API_BASE_URL: &str = "https://api.openai.com/v1";

/// Build an OpenAI backend, using the supplied base URL when configured.
pub fn openai_backend(
    http_client: reqwest::Client,
    api_key: String,
    base_url_override: Option<&str>,
) -> OpenAICompatibleBackend {
    match base_url_override {
        Some(url) => OpenAICompatibleBackend::with_base_url(http_client, api_key, url.to_string()),
        None => OpenAICompatibleBackend::new(http_client, api_key),
    }
}
