use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;

use super::provider::{BackendError, BackendProvider};
use crate::decisions::{DecisionsRequest, DecisionsResponse};
use crate::jev_prompting::JevPrompt;
use crate::types::{ChatCompletionRequest, ChatCompletionResponse, ListResponse, Model};

pub const TYPESAFE_API_BASE_URL: &str = "https://api.typesafe.ai/v1";

#[derive(Debug, Deserialize)]
struct TypeSafeModelList {
    models: Vec<TypeSafeModelMetadata>,
}

#[derive(Debug, Deserialize)]
struct TypeSafeModelMetadata {
    name: String,
    description: String,
    release_date: String,
}

impl TypeSafeModelMetadata {
    fn into_model(self) -> Option<Model> {
        if self.name.trim().is_empty() {
            return None;
        }
        let created = release_timestamp(&self.release_date).unwrap_or_default();
        let provider_metadata = std::collections::BTreeMap::from([
            (
                "description".to_string(),
                serde_json::Value::String(self.description),
            ),
            (
                "release_date".to_string(),
                serde_json::Value::String(self.release_date),
            ),
        ]);
        Some(Model {
            id: self.name,
            object: "model".to_string(),
            created,
            owned_by: "typesafe".to_string(),
            capabilities: None,
            provider_metadata,
        })
    }
}

/// TypeSafe documents `release_date` as `YYYY-MM-DD`, but the live catalog
/// returns RFC 3339 timestamps; accept both so `created` is never zeroed.
fn release_timestamp(release_date: &str) -> Option<i64> {
    if let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(release_date) {
        return Some(timestamp.timestamp());
    }
    chrono::NaiveDate::parse_from_str(release_date, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|date| date.and_utc().timestamp())
}

/// Native TypeSafe System One backend.
///
/// Serves two surfaces from the same `POST /v1/systemone` endpoint: the
/// Decisions / System One request as-is, and OpenAI chat completions through
/// [`JevPrompt`], which turns a prompt plus its `response_format` schema into
/// typed questions and the typed answers back into the schema's JSON object.
pub struct TypeSafeBackend {
    client: Client,
    api_key: String,
    base_url: String,
}

impl TypeSafeBackend {
    pub fn new(client: Client, api_key: String, base_url_override: Option<&str>) -> Self {
        let base_url = crate::provider_registry::resolve_base_url("typesafe", base_url_override)
            .expect("typesafe has a registry default base URL");
        Self {
            client,
            api_key,
            base_url,
        }
    }

    pub(crate) fn wire_prompt(
        prompt: &JevPrompt,
    ) -> Result<crate::schema_json::SchemaJson, BackendError> {
        prompt.wire_request().map_err(BackendError::Other)
    }

    async fn model_error(response: reqwest::Response) -> BackendError {
        super::provider::upstream_error(response, |text| {
            let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
            value
                .pointer("/error/message")
                .or_else(|| value.get("detail"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .await
    }

    async fn fetch_models(&self) -> Result<Vec<Model>, BackendError> {
        let response = self
            .client
            .get(format!("{}/models", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Self::model_error(response).await);
        }
        let response = response
            .json::<TypeSafeModelList>()
            .await
            .map_err(|error| {
                BackendError::api_status(
                    502,
                    format!("invalid TypeSafe model-list response: {error}"),
                )
            })?;
        let mut seen = std::collections::HashSet::new();
        Ok(response
            .models
            .into_iter()
            .filter_map(TypeSafeModelMetadata::into_model)
            .filter(|model| seen.insert(model.id.clone()))
            .collect())
    }
}

#[async_trait]
impl BackendProvider for TypeSafeBackend {
    /// Jev prompting: a chat prompt answered as typed decisions. Translation
    /// validation failures are the caller's to fix (400); wire-plan failures
    /// are internal errors (500). TypeSafe's own status codes
    /// pass through so the fallback walker still retries 429/5xx.
    async fn create_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        let prompt =
            JevPrompt::plan(&request).map_err(|message| BackendError::api_status(400, message))?;
        let response = self
            .client
            .post(format!("{}/systemone", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&Self::wire_prompt(&prompt)?)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Self::model_error(response).await);
        }
        let answers = response
            .json::<DecisionsResponse>()
            .await
            .map_err(|error| {
                BackendError::InvalidResponse(format!(
                    "invalid TypeSafe System One response: {error}"
                ))
            })?;
        prompt
            .into_chat_completion(answers)
            .map_err(BackendError::InvalidResponse)
    }

    async fn send_decisions_request(
        &self,
        request: DecisionsRequest,
    ) -> Result<reqwest::Response, BackendError> {
        let images = !request.assets.is_empty() || request.has_box_questions();
        let request = request
            .into_system_one_request()
            .map_err(|message| BackendError::api_status(400, message))?;
        let mut builder = self
            .client
            .post(format!("{}/systemone", self.base_url))
            .bearer_auth(&self.api_key);
        if images {
            // TypeSafe gates assets and box questions behind this opt-in.
            builder = builder.header("TypeSafe-Danger-Zone-Images", "true");
        }
        builder
            .json(&request)
            .send()
            .await
            .map_err(BackendError::from)
    }

    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        Ok(ListResponse {
            object: "list".to_string(),
            data: self.fetch_models().await?,
            first_id: None,
            last_id: None,
            has_more: false,
        })
    }

    async fn get_model(&self, model_id: &str) -> Result<Model, BackendError> {
        self.fetch_models()
            .await?
            .into_iter()
            .find(|model| model.id == model_id)
            .ok_or_else(|| BackendError::api_status(404, format!("model '{model_id}' not found")))
    }

    fn name(&self) -> &str {
        "typesafe"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_metadata_maps_to_an_openai_model_card() {
        let model = TypeSafeModelMetadata {
            name: "jev-latest".to_string(),
            description: "Latest stable Jev".to_string(),
            release_date: "2026-09-17".to_string(),
        }
        .into_model()
        .unwrap();
        assert_eq!(model.id, "jev-latest");
        assert_eq!(model.owned_by, "typesafe");
        assert_eq!(
            model.created,
            chrono::DateTime::parse_from_rfc3339("2026-09-17T00:00:00Z")
                .unwrap()
                .timestamp()
        );
        assert_eq!(model.provider_metadata["description"], "Latest stable Jev");
    }

    #[test]
    fn live_catalog_timestamps_are_accepted_as_release_dates() {
        let model = TypeSafeModelMetadata {
            name: "jev-preview".to_string(),
            description: String::new(),
            release_date: "2026-09-10T18:39:06.057655+00:00".to_string(),
        }
        .into_model()
        .unwrap();
        assert_eq!(
            model.created,
            chrono::DateTime::parse_from_rfc3339("2026-09-10T18:39:06Z")
                .unwrap()
                .timestamp()
        );
    }

    #[test]
    fn unusable_names_are_omitted_and_invalid_dates_default_to_zero() {
        assert!(TypeSafeModelMetadata {
            name: "  ".to_string(),
            description: String::new(),
            release_date: "2026-09-17".to_string(),
        }
        .into_model()
        .is_none());
        assert_eq!(
            TypeSafeModelMetadata {
                name: "future-system-one".to_string(),
                description: String::new(),
                release_date: "not-a-date".to_string(),
            }
            .into_model()
            .unwrap()
            .created,
            0
        );
    }
}
