//! Translate the gateway embedding contract to Gemini Embedding 2's native API.

use super::{BackendError, GeminiBackend};
use crate::backend::google_embedding::prepare_embedding;
use crate::translation::gemini_embeddings::{
    embedding_response, google_error_message, EmbeddingResponse,
};
use crate::types::{CreateEmbeddingRequest, CreateEmbeddingResponse};
use serde_json::json;

impl GeminiBackend {
    pub(super) async fn create_multimodal_embedding(
        &self,
        request: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        let prepared = prepare_embedding(&request).await?;
        let expected_count = prepared.contents.len();
        let model = format!(
            "models/{}",
            request
                .model
                .strip_prefix("models/")
                .unwrap_or(&request.model)
        );
        let requests: Vec<_> = prepared
            .contents
            .into_iter()
            .map(|content| {
                let mut body = json!({"model": model, "content": content});
                if let Some(dimensions) = request.dimensions {
                    body["outputDimensionality"] = json!(dimensions);
                }
                body
            })
            .collect();
        let (operation, body) = if prepared.batch {
            ("batchEmbedContents", json!({"requests": requests}))
        } else {
            (
                "embedContent",
                requests.into_iter().next().expect("nonempty contents"),
            )
        };
        let response = self
            .client
            .post(format!("{}/{model}:{operation}", self.native_base_url))
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(
                super::super::provider::upstream_error(response, google_error_message).await,
            );
        }
        let parsed: EmbeddingResponse = response.json().await?;
        let embeddings = if prepared.batch {
            parsed.embeddings
        } else {
            parsed.embedding.map(|value| vec![value])
        }
        .ok_or_else(|| BackendError::Other("Gemini response omitted embeddings".into()))?;
        let usage = crate::translation::gemini_embeddings::observe_embedding_usage(
            parsed.usage_metadata.as_ref(),
            prepared.has_images,
        )
        .map_err(|message| BackendError::Other(message.into()))?;
        embedding_response(
            embeddings,
            expected_count,
            prepared.base64,
            request.model,
            usage,
        )
    }
}
