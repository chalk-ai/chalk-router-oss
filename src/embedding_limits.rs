//! Immutable provider limits, separate from editable capability declarations.

pub(crate) struct EmbeddingAdmissionBounds {
    pub max_input_tokens: u64,
    pub max_images_per_document: usize,
    pub image_mime_types: &'static [&'static str],
}

impl EmbeddingAdmissionBounds {
    /// The published capability copy. Operators may override that copy;
    /// admission keeps using these bounds.
    pub(crate) fn capability_limits(&self) -> crate::capabilities::EmbeddingInputLimits {
        crate::capabilities::EmbeddingInputLimits {
            max_images_per_document: Some(self.max_images_per_document as u64),
            image_mime_types: Some(
                self.image_mime_types
                    .iter()
                    .map(|mime_type| (*mime_type).to_string())
                    .collect(),
            ),
        }
    }
}

/// https://ai.google.dev/gemini-api/docs/embeddings#supported-modalities-and-limits
pub(crate) const GEMINI_EMBEDDING_2_LIMITS: EmbeddingAdmissionBounds = EmbeddingAdmissionBounds {
    max_input_tokens: 8_192,
    max_images_per_document: 6,
    image_mime_types: &["image/png", "image/jpeg"],
};

pub(crate) fn gemini_embedding_2_limits(model: &str) -> Option<&'static EmbeddingAdmissionBounds> {
    matches!(
        model.strip_prefix("models/").unwrap_or(model),
        "gemini-embedding-2" | "gemini-embedding-2-preview"
    )
    .then_some(&GEMINI_EMBEDDING_2_LIMITS)
}
