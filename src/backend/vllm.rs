use super::openai_compatible::OpenAICompatibleBackend;
use super::provider::BackendError;

const UNAUTHENTICATED_API_KEY: &str = "vllm-no-api-key";

/// Normalize a vLLM server URL to the OpenAI-compatible `/v1` API root.
///
/// vLLM is commonly configured either with its server root
/// (`http://localhost:8000`) or with the versioned API root already included
/// (`http://localhost:8000/v1`). Both forms resolve to a single trailing
/// `/v1`; custom path prefixes are preserved.
pub fn normalize_vllm_base_url(base_url: &str) -> Result<String, BackendError> {
    let base_url = base_url.trim();
    if base_url.is_empty() {
        return Err(BackendError::ConfigError(
            "provider 'vllm' requires a base URL".to_string(),
        ));
    }

    let mut url = reqwest::Url::parse(base_url).map_err(|error| {
        BackendError::ConfigError(format!("provider 'vllm' has an invalid base URL: {error}"))
    })?;

    let mut path = url.path().trim_end_matches('/');
    while let Some(without_version) = path.strip_suffix("/v1") {
        path = without_version.trim_end_matches('/');
    }

    let versioned_path = if path.is_empty() {
        "/v1".to_string()
    } else {
        format!("{path}/v1")
    };
    url.set_path(&versioned_path);

    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn resolve_api_key(api_key: Option<&str>) -> String {
    api_key
        .map(str::trim)
        .filter(|api_key| !api_key.is_empty())
        .unwrap_or(UNAUTHENTICATED_API_KEY)
        .to_string()
}

/// Create a backend for a vLLM server's OpenAI-compatible API.
///
/// A base URL is required because vLLM has no universal hosted endpoint. API
/// keys are optional because vLLM servers can run without authentication; the
/// placeholder keeps [`OpenAICompatibleBackend`]'s bearer-token interface harmlessly
/// satisfied in that case.
pub fn vllm_backend(
    http_client: reqwest::Client,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<OpenAICompatibleBackend, BackendError> {
    let base_url = normalize_vllm_base_url(base_url.unwrap_or_default())?;
    Ok(OpenAICompatibleBackend::for_provider(
        http_client,
        "vllm",
        resolve_api_key(api_key),
        base_url,
    ))
}

#[cfg(test)]
mod tests {
    use super::{normalize_vllm_base_url, resolve_api_key, vllm_backend, UNAUTHENTICATED_API_KEY};
    use crate::backend::BackendError;

    #[test]
    fn appends_v1_to_server_root() {
        assert_eq!(
            normalize_vllm_base_url("http://localhost:8000").unwrap(),
            "http://localhost:8000/v1"
        );
    }

    #[test]
    fn keeps_exactly_one_trailing_v1() {
        for base_url in [
            "http://localhost:8000/v1",
            "http://localhost:8000/v1/",
            "http://localhost:8000/v1/v1///",
        ] {
            assert_eq!(
                normalize_vllm_base_url(base_url).unwrap(),
                "http://localhost:8000/v1"
            );
        }
    }

    #[test]
    fn preserves_custom_path_prefix() {
        assert_eq!(
            normalize_vllm_base_url("https://models.example.com/inference/").unwrap(),
            "https://models.example.com/inference/v1"
        );
    }

    #[test]
    fn requires_a_valid_configured_base_url() {
        for base_url in [None, Some("  "), Some("not a URL")] {
            let error = match vllm_backend(reqwest::Client::new(), base_url, None) {
                Ok(_) => panic!("expected invalid base URL to be rejected"),
                Err(error) => error,
            };
            assert!(matches!(error, BackendError::ConfigError(_)));
        }
    }

    #[test]
    fn uses_placeholder_only_when_api_key_is_absent() {
        assert_eq!(resolve_api_key(None), UNAUTHENTICATED_API_KEY);
        assert_eq!(resolve_api_key(Some("  ")), UNAUTHENTICATED_API_KEY);
        assert_eq!(resolve_api_key(Some(" secret ")), "secret");

        assert!(vllm_backend(reqwest::Client::new(), Some("http://localhost:8000"), None).is_ok());
        assert!(vllm_backend(
            reqwest::Client::new(),
            Some("http://localhost:8000/v1"),
            Some("secret"),
        )
        .is_ok());
    }
}
