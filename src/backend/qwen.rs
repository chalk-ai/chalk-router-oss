use super::openai_compatible::OpenAICompatibleBackend;

/// Alibaba Cloud Model Studio's OpenAI-compatible endpoint for the US
/// (Virginia) region.
pub const QWEN_OPENAI_BASE_URL: &str = "https://dashscope-us.aliyuncs.com/compatible-mode/v1";
pub const QWEN_ANTHROPIC_BASE_URL: &str = "https://dashscope-us.aliyuncs.com/apps/anthropic/v1";

/// Build a Qwen backend using Alibaba Cloud Model Studio's OpenAI-compatible
/// API.
///
/// The `api_key` is sourced from `QWEN_API_KEY` by the caller and is sent as a
/// bearer token by [`OpenAICompatibleBackend`].
pub fn qwen_backend(
    http_client: reqwest::Client,
    api_key: String,
    base_url_override: Option<&str>,
) -> OpenAICompatibleBackend {
    let openai_base_url = crate::provider_registry::resolve_base_url("qwen", base_url_override)
        .expect("qwen has a registry default base URL");
    let anthropic_base_url = resolve_qwen_anthropic_base_url(&openai_base_url);
    OpenAICompatibleBackend::for_provider(http_client, "qwen", api_key, openai_base_url)
        .with_anthropic_base_url(anthropic_base_url)
}

/// Derive Qwen's distinct Anthropic-protocol endpoint from its resolved
/// OpenAI-compatible base URL, honoring a region- or workspace-specific
/// override applied to the latter. Qwen is the only wrapper backend whose
/// Anthropic and OpenAI-compatible protocols live under different roots, so
/// this derivation stays provider-specific rather than going through the
/// shared `resolve_base_url`.
pub fn resolve_qwen_anthropic_base_url(openai_base_url: &str) -> String {
    openai_base_url
        .strip_suffix("/compatible-mode/v1")
        .map(|root| format!("{root}/apps/anthropic/v1"))
        .unwrap_or_else(|| openai_base_url.to_string())
}

#[cfg(test)]
mod tests {
    use super::{resolve_qwen_anthropic_base_url, QWEN_ANTHROPIC_BASE_URL, QWEN_OPENAI_BASE_URL};

    #[test]
    fn derives_the_distinct_anthropic_protocol_root() {
        assert_eq!(
            resolve_qwen_anthropic_base_url(QWEN_OPENAI_BASE_URL),
            QWEN_ANTHROPIC_BASE_URL
        );
    }

    #[test]
    fn derives_the_anthropic_root_for_a_workspace_override() {
        assert_eq!(
            resolve_qwen_anthropic_base_url(
                "https://workspace.example.aliyuncs.com/compatible-mode/v1"
            ),
            "https://workspace.example.aliyuncs.com/apps/anthropic/v1"
        );
    }
}
