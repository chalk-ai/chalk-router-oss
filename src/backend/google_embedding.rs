//! Build Gemini Embedding 2 `content` objects from the gateway embedding
//! request, fetching image URLs under the public-content egress policy. Shared by
//! the Gemini Developer API and Vertex backends, which differ only in the
//! endpoint and credential.

use super::BackendError;
use crate::types::{CreateEmbeddingRequest, EmbeddingInput, EmbeddingInputItem};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

/// A gateway embedding request translated into native `content` objects.
pub(crate) struct PreparedEmbedding {
    /// One `{"parts": [...]}` per output vector.
    pub(crate) contents: Vec<Value>,
    /// A text batch: each string gets its own vector.
    pub(crate) batch: bool,
    pub(crate) has_images: bool,
    pub(crate) base64: bool,
}

fn inline_image_part(mime_type: &str, data: String) -> Value {
    let mut part = json!({"inlineData": {"mimeType": mime_type}});
    // Move owned normalized/encoded data into the JSON body without copying it.
    part["inlineData"]["data"] = Value::String(data);
    part
}

async fn image_embedding_part(url: &str) -> Result<Value, BackendError> {
    if let Some(uri) = crate::storage::split_data_uri(url) {
        if !uri.media_type.starts_with("image/") || !uri.base64 || uri.data.is_none() {
            return Err(BackendError::api_status(
                400,
                "embedding images require base64 image data URIs",
            ));
        }
        let data = uri.validated_base64().map_err(|error| {
            BackendError::api_status(400, format!("invalid embedding image: {error}"))
        })?;
        if data.is_empty() || uri.media_type == "image/bin" {
            return Err(BackendError::api_status(
                400,
                "embedding image is empty or has no image MIME type",
            ));
        }
        let mime_type = if uri.media_type == "image/jpg" {
            "image/jpeg"
        } else {
            uri.media_type.as_ref()
        };
        return Ok(inline_image_part(mime_type, data.into_owned()));
    }
    if !reqwest::Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https")) {
        return Err(BackendError::api_status(
            400,
            "embedding images require HTTP(S) URLs or base64 image data URIs",
        ));
    }
    let (bytes, extension) = crate::storage::fetch_image_bytes(url)
        .await
        .map_err(|error| {
            BackendError::api_status(400, format!("invalid embedding image: {error}"))
        })?;
    let extension = extension.split(';').next().unwrap_or(&extension);
    let extension = if extension == "jpg" {
        "jpeg"
    } else {
        extension
    };
    if bytes.is_empty() || extension == "bin" {
        return Err(BackendError::api_status(
            400,
            "embedding image is empty or has no image MIME type",
        ));
    }
    Ok(inline_image_part(
        &format!("image/{extension}"),
        STANDARD.encode(bytes),
    ))
}

/// Validate a Gemini Embedding 2 request and build its native contents.
pub(crate) async fn prepare_embedding(
    request: &CreateEmbeddingRequest,
) -> Result<PreparedEmbedding, BackendError> {
    let base64 = match request.encoding_format.as_deref() {
        None | Some("float") => false,
        Some("base64") => true,
        Some(_) => {
            return Err(BackendError::api_status(
                400,
                "encoding_format must be float or base64",
            ))
        }
    };
    if request
        .dimensions
        .is_some_and(|dimensions| !(128..=3072).contains(&dimensions))
    {
        return Err(BackendError::api_status(
            400,
            "Gemini Embedding 2 dimensions must be between 128 and 3072",
        ));
    }
    // Each string in a text batch gets its own vector. A multimodal list is
    // one document, matching the existing Voyage embedding contract.
    let batch = matches!(request.input, EmbeddingInput::Multiple(_));
    let has_images = matches!(&request.input, EmbeddingInput::Multimodal(items)
        if items.iter().any(|item| matches!(item, EmbeddingInputItem::ImageUrl { .. })));
    let contents = match &request.input {
        EmbeddingInput::Single(text) => vec![json!({"parts": [{"text": text}]})],
        EmbeddingInput::Multiple(texts) => texts
            .iter()
            .map(|text| json!({"parts": [{"text": text}]}))
            .collect(),
        EmbeddingInput::Multimodal(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                parts.push(match item {
                    EmbeddingInputItem::Text { text } => json!({"text": text}),
                    EmbeddingInputItem::ImageUrl { image_url } => {
                        image_embedding_part(&image_url.url).await?
                    }
                });
            }
            if parts.is_empty() {
                return Err(BackendError::api_status(
                    400,
                    "embedding input must not be empty",
                ));
            }
            vec![json!({"parts": parts})]
        }
        EmbeddingInput::Tokens(_) | EmbeddingInput::TokenBatch(_) => {
            return Err(BackendError::api_status(
                400,
                "Gemini Embedding 2 does not support token-array input",
            ))
        }
    };
    if contents.is_empty() {
        return Err(BackendError::api_status(
            400,
            "embedding input must not be empty",
        ));
    }
    Ok(PreparedEmbedding {
        contents,
        batch,
        has_images,
        base64,
    })
}
