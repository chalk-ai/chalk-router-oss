//! Conservative embedding admission bounds, separate from reported usage.

use crate::{backend::BackendError, embedding_limits::gemini_embedding_2_limits};

/// One document produces one embedding, even when it contains multiple images.
pub(crate) struct EmbeddingDocument {
    pub image_count: usize,
    /// The existing conservative text estimate; `None` remains unknown.
    pub text_tokens: Option<u64>,
}

/// Google's documented maximum is shared across all parts of one document.
/// https://ai.google.dev/gemini-api/docs/embeddings#supported-modalities-and-limits
/// Using that ceiling avoids relying on generation-model image tiling rules or
/// making an upstream token-counting call before quota admission. It deliberately
/// requires 8,192 available tokens even when the actual usage is much smaller.
pub(crate) fn embedding_token_reservation(
    provider_kind: &str,
    upstream_model: &str,
    documents: impl IntoIterator<Item = EmbeddingDocument>,
) -> Result<Option<u64>, BackendError> {
    // Vertex serves the same model, qualified by its publisher.
    let limits = match provider_kind {
        "gemini" => gemini_embedding_2_limits(upstream_model),
        "vertex" => gemini_embedding_2_limits(
            upstream_model
                .strip_prefix("google/")
                .unwrap_or(upstream_model),
        ),
        _ => None,
    };
    documents.into_iter().try_fold(Some(0u64), |sum, document| {
        let tokens = if document.image_count == 0 {
            document.text_tokens
        } else if let Some(limits) = limits {
            if document.image_count > limits.max_images_per_document {
                return Err(BackendError::api_status(
                    400,
                    format!(
                        "Gemini Embedding 2 supports at most {} images per document",
                        limits.max_images_per_document
                    ),
                ));
            }
            Some(limits.max_input_tokens)
        } else {
            None
        };
        // Never drop an unknown document or wrap a batch total on overflow.
        Ok(sum
            .zip(tokens)
            .map(|(sum, tokens)| sum.saturating_add(tokens)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(image_count: usize, text_tokens: Option<u64>) -> EmbeddingDocument {
        EmbeddingDocument {
            image_count,
            text_tokens,
        }
    }

    #[test]
    fn known_gemini_models_use_one_ceiling_per_image_document() {
        for model in ["gemini-embedding-2", "models/gemini-embedding-2-preview"] {
            assert_eq!(
                embedding_token_reservation(
                    "gemini",
                    model,
                    [document(1, None), document(6, None), document(0, Some(9))]
                )
                .unwrap(),
                Some(16_393),
            );
            assert!(embedding_token_reservation("gemini", model, [document(7, None)]).is_err());
        }
    }

    #[test]
    fn vertex_serves_gemini_embedding_2_under_the_same_ceiling() {
        for model in ["gemini-embedding-2", "google/gemini-embedding-2"] {
            assert_eq!(
                embedding_token_reservation("vertex", model, [document(2, None)]).unwrap(),
                Some(8_192),
            );
            assert!(embedding_token_reservation("vertex", model, [document(7, None)]).is_err());
        }
    }

    #[test]
    fn other_models_and_unknown_documents_remain_unknown() {
        for (provider, model) in [
            ("gemini", "gemini-embedding-001"),
            ("vertex", "text-embedding-005"),
            ("openrouter", "gemini-embedding-2"),
            ("openrouter", "google/gemini-embedding-2"),
            ("voyage", "voyage-multimodal-3"),
        ] {
            assert_eq!(
                embedding_token_reservation(provider, model, [document(1, Some(20))]).unwrap(),
                None
            );
        }
        assert_eq!(
            embedding_token_reservation(
                "gemini",
                "gemini-embedding-2",
                [document(1, None), document(0, None)]
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn text_estimates_are_preserved_and_batch_sums_saturate() {
        assert_eq!(
            embedding_token_reservation(
                "openai",
                "text-embedding-3-small",
                [document(0, Some(12))]
            )
            .unwrap(),
            Some(12)
        );
        assert_eq!(
            embedding_token_reservation(
                "gemini",
                "gemini-embedding-2",
                [document(0, Some(u64::MAX)), document(1, None)]
            )
            .unwrap(),
            Some(u64::MAX)
        );
    }
}
