//! Native Gemini embedding responses: shape their vectors as the gateway
//! response and observe usage without changing the wire representation. Shared
//! by the Gemini Developer API and Vertex backends.

use crate::{
    backend::BackendError,
    pricing::InputTokenAttribution,
    types::{CreateEmbeddingResponse, Embedding, EmbeddingData, Usage},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EmbeddingResponse {
    pub(crate) embedding: Option<ContentEmbedding>,
    pub(crate) embeddings: Option<Vec<ContentEmbedding>>,
    pub(crate) usage_metadata: Option<Value>,
}

#[derive(Deserialize)]
pub(crate) struct ContentEmbedding {
    pub(crate) values: Vec<f32>,
}

/// Shape native vectors as the gateway response, rejecting a count mismatch
/// or an empty vector rather than returning a silently wrong batch.
pub(crate) fn embedding_response(
    embeddings: Vec<ContentEmbedding>,
    expected_count: usize,
    base64: bool,
    model: String,
    usage: Option<Usage>,
) -> Result<CreateEmbeddingResponse, BackendError> {
    if embeddings.len() != expected_count
        || embeddings
            .iter()
            .any(|embedding| embedding.values.is_empty())
    {
        return Err(BackendError::Other(
            "Gemini returned an invalid embedding count or empty vector".into(),
        ));
    }
    let data = embeddings
        .into_iter()
        .enumerate()
        .map(|(index, embedding)| Embedding {
            object: "embedding".into(),
            index: index as i32,
            embedding: if base64 {
                EmbeddingData::Base64(
                    STANDARD.encode(
                        embedding
                            .values
                            .iter()
                            .flat_map(|value| value.to_le_bytes())
                            .collect::<Vec<_>>(),
                    ),
                )
            } else {
                EmbeddingData::Float(embedding.values)
            },
        })
        .collect();
    Ok(CreateEmbeddingResponse {
        object: "list".into(),
        model,
        data,
        usage,
    })
}

/// Google's error envelope nests the message under `error`.
pub(crate) fn google_error_message(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()?
        .pointer("/error/message")?
        .as_str()
        .map(str::to_owned)
}

/// The Gemini API embeds modality counts in `promptTokenDetails` (singular
/// `Token`), unlike the similarly named generation-usage field; Vertex's
/// `:embedContent` uses the generation spelling, `promptTokensDetails`. Invalid
/// attribution must not discard valid totals, or turn an unknown image cost
/// into a text estimate.
pub(crate) fn observe_embedding_usage(
    metadata: Option<&Value>,
    has_images: bool,
) -> Result<Option<Usage>, &'static str> {
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    if !metadata.is_object() {
        return Err("Gemini returned invalid embedding usage metadata");
    }
    let Some(tokens) = metadata.get("promptTokenCount") else {
        return Ok(None);
    };
    let tokens = tokens
        .as_u64()
        .and_then(|tokens| i32::try_from(tokens).ok())
        .ok_or("Gemini returned an invalid embedding prompt token count")?;
    let details = metadata
        .get("promptTokenDetails")
        .or_else(|| metadata.get("promptTokensDetails"));
    let input_token_attribution = match details {
        None if !has_images => InputTokenAttribution::TextRate,
        None => InputTokenAttribution::Unknown,
        Some(details) => {
            attribution(details, tokens as u64).unwrap_or(InputTokenAttribution::Unknown)
        }
    };
    Ok(Some(Usage {
        prompt_tokens: tokens,
        total_tokens: tokens,
        input_token_attribution,
        ..Default::default()
    }))
}

fn attribution(details: &Value, total: u64) -> Option<InputTokenAttribution> {
    let details = details.as_array()?;
    let mut text_tokens = 0_u64;
    let mut image_tokens = 0_u64;
    for detail in details {
        let tokens = detail.get("tokenCount")?.as_u64()?;
        match detail.get("modality")?.as_str()? {
            "TEXT" => text_tokens = text_tokens.checked_add(tokens)?,
            "IMAGE" => image_tokens = image_tokens.checked_add(tokens)?,
            _ => return None,
        }
    }
    if text_tokens.checked_add(image_tokens)? != total {
        return None;
    }
    Some(InputTokenAttribution::TextAndImage {
        text_tokens,
        image_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn observes_complete_modality_totals_without_changing_wire_usage() {
        let usage = observe_embedding_usage(
            Some(&json!({
                "promptTokenCount": 265,
                "promptTokenDetails": [
                    {"modality":"TEXT", "tokenCount":7},
                    {"modality":"IMAGE", "tokenCount":258}
                ]
            })),
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            usage.input_token_attribution,
            InputTokenAttribution::TextAndImage {
                text_tokens: 7,
                image_tokens: 258,
            }
        );
        assert_eq!(
            serde_json::to_value(&usage).unwrap(),
            json!({
                "prompt_tokens":265, "total_tokens":265,
            })
        );
    }

    #[test]
    fn vertex_modality_details_use_the_generation_spelling() {
        // Verbatim shape of a Vertex `:embedContent` response for one image and text.
        let usage = observe_embedding_usage(
            Some(&json!({
                "promptTokenCount": 521,
                "totalTokenCount": 521,
                "trafficType": "ON_DEMAND",
                "promptTokensDetails": [
                    {"modality":"IMAGE", "tokenCount":516},
                    {"modality":"TEXT", "tokenCount":5}
                ]
            })),
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            usage.input_token_attribution,
            InputTokenAttribution::TextAndImage {
                text_tokens: 5,
                image_tokens: 516,
            }
        );
    }

    #[test]
    fn missing_or_invalid_details_leave_valid_totals_unpriced() {
        for details in [
            Value::Null,
            json!({"modality":"IMAGE", "tokenCount":258}),
            json!([]),
            json!([{"modality":"IMAGE", "tokenCount":257}]),
            json!([{"modality":"IMAGE", "tokenCount":-1}]),
            json!([{"modality":"IMAGE", "tokenCount":"258"}]),
            json!([{"modality":"AUDIO", "tokenCount":258}]),
            json!([{"modality":"IMAGE"}]),
            json!([{"modality":"IMAGE", "tokenCount":u64::MAX}, {"modality":"IMAGE", "tokenCount":1}]),
        ] {
            let usage = observe_embedding_usage(
                Some(&json!({
                    "promptTokenCount":258, "promptTokenDetails":details,
                })),
                true,
            )
            .unwrap()
            .unwrap();
            assert_eq!(usage.prompt_tokens, 258);
            assert_eq!(
                usage.input_token_attribution,
                InputTokenAttribution::Unknown
            );
        }
        let usage = observe_embedding_usage(Some(&json!({"promptTokenCount":258})), true)
            .unwrap()
            .unwrap();
        assert_eq!(
            usage.input_token_attribution,
            InputTokenAttribution::Unknown
        );
    }

    #[test]
    fn text_without_modality_details_preserves_legacy_pricing() {
        let usage = observe_embedding_usage(Some(&json!({"promptTokenCount":7})), false)
            .unwrap()
            .unwrap();
        assert_eq!(
            usage.input_token_attribution,
            InputTokenAttribution::TextRate
        );
        assert!(observe_embedding_usage(None, false).unwrap().is_none());
        assert!(observe_embedding_usage(Some(&json!({})), true)
            .unwrap()
            .is_none());
    }

    #[test]
    fn malformed_total_never_becomes_valid_usage() {
        for total in [json!(-1), json!(i64::MAX), json!("258"), json!(null)] {
            assert!(
                observe_embedding_usage(Some(&json!({"promptTokenCount":total})), true).is_err()
            );
        }
    }

    #[test]
    fn zero_and_repeated_modality_counts_are_summed_with_checks() {
        for (total, details, expected) in [
            (
                0,
                json!([]),
                InputTokenAttribution::TextAndImage {
                    text_tokens: 0,
                    image_tokens: 0,
                },
            ),
            (
                516,
                json!([{"modality":"IMAGE", "tokenCount":258}, {"modality":"IMAGE", "tokenCount":258}]),
                InputTokenAttribution::TextAndImage {
                    text_tokens: 0,
                    image_tokens: 516,
                },
            ),
        ] {
            let usage = observe_embedding_usage(
                Some(&json!({"promptTokenCount":total, "promptTokenDetails":details})),
                true,
            )
            .unwrap()
            .unwrap();
            assert_eq!(usage.input_token_attribution, expected);
        }
    }
}
