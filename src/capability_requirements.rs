//! Shared extraction of effective capability requirements from public request
//! bodies. Keeping these builders outside handlers and backends prevents the
//! runtime checks from diverging across dispatch paths.

use crate::capabilities::{
    CapabilityRequirements, InputModality, Interface, InterfaceOperation, ModelSurface,
    OutputModality,
};
use crate::types::{CreateImageRequest, EmbeddingInput, EmbeddingInputItem};

pub(crate) fn embedding_requirements(input: &EmbeddingInput) -> CapabilityRequirements {
    let mut requirements = CapabilityRequirements::for_interface_operation(
        Interface::OpenaiEmbeddings,
        InterfaceOperation::Create,
    )
    .surface(ModelSurface::Embeddings)
    .output(OutputModality::Embeddings);
    match input {
        EmbeddingInput::Single(_)
        | EmbeddingInput::Multiple(_)
        | EmbeddingInput::Tokens(_)
        | EmbeddingInput::TokenBatch(_) => {
            requirements = requirements.input(InputModality::Text);
        }
        EmbeddingInput::Multimodal(items) => {
            if items
                .iter()
                .any(|item| matches!(item, EmbeddingInputItem::Text { .. }))
            {
                requirements = requirements.input(InputModality::Text);
            }
            if items
                .iter()
                .any(|item| matches!(item, EmbeddingInputItem::ImageUrl { .. }))
            {
                requirements = requirements.input(InputModality::Image);
            }
        }
    }
    requirements
}

pub(crate) fn moderation_requirements() -> CapabilityRequirements {
    CapabilityRequirements::for_interface_operation(
        Interface::OpenaiModerations,
        InterfaceOperation::Create,
    )
    .surface(ModelSurface::Moderation)
    .input(InputModality::Text)
}

pub(crate) fn audio_speech_requirements() -> CapabilityRequirements {
    CapabilityRequirements::for_interface_operation(
        Interface::OpenaiAudio,
        InterfaceOperation::Speech,
    )
    .surface(ModelSurface::AudioSpeech)
    .input(InputModality::Text)
    .output(OutputModality::Audio)
}

pub(crate) fn audio_transcription_requirements() -> CapabilityRequirements {
    CapabilityRequirements::for_interface_operation(
        Interface::OpenaiAudio,
        InterfaceOperation::Transcription,
    )
    .surface(ModelSurface::AudioTranscription)
    .input(InputModality::Audio)
    .output(OutputModality::Text)
}

pub(crate) fn audio_translation_requirements() -> CapabilityRequirements {
    CapabilityRequirements::for_interface_operation(
        Interface::OpenaiAudio,
        InterfaceOperation::Translation,
    )
    .surface(ModelSurface::AudioTranscription)
    .input(InputModality::Audio)
    .output(OutputModality::Text)
}

pub(crate) fn image_generation_requirements(
    request: &CreateImageRequest,
) -> CapabilityRequirements {
    let mut requirements = CapabilityRequirements::for_interface_operation(
        Interface::OpenaiImages,
        InterfaceOperation::Generate,
    )
    .surface(ModelSurface::ImageGeneration)
    .input(InputModality::Text)
    .output(OutputModality::Image);
    for (name, value) in [
        ("n", request.n.map(serde_json::Value::from)),
        (
            "quality",
            request.quality.clone().map(serde_json::Value::from),
        ),
        ("size", request.size.clone().map(serde_json::Value::from)),
        (
            "aspect_ratio",
            request.aspect_ratio.clone().map(serde_json::Value::from),
        ),
        (
            "image_size",
            request.image_size.clone().map(serde_json::Value::from),
        ),
    ] {
        if let Some(value) = value {
            requirements = requirements.parameter(name, value);
        }
    }
    requirements
}

/// The auxiliary judge transports require only text classification support.
pub(crate) fn judge_requirements(decisions: bool) -> CapabilityRequirements {
    use crate::capabilities::{Protocol, ProtocolOperation};
    let (protocol, surface, output) = if decisions {
        (
            Protocol::RouterDecisions,
            ModelSurface::Decisions,
            OutputModality::Decisions,
        )
    } else {
        (
            Protocol::OpenaiChatCompletions,
            ModelSurface::Chat,
            OutputModality::Text,
        )
    };
    CapabilityRequirements::for_operation(protocol, ProtocolOperation::Create)
        .surface(surface)
        .input(InputModality::Text)
        .output(output)
}

pub(crate) fn gemini_interaction_requirements(body: &serde_json::Value) -> CapabilityRequirements {
    use crate::capabilities::EndpointFeature;
    let mut requirements = CapabilityRequirements::for_interface_operation(
        Interface::GeminiInteractions,
        InterfaceOperation::Create,
    );
    for (present, feature) in [
        (
            body["stream"].as_bool() == Some(true),
            EndpointFeature::Streaming,
        ),
        (
            body.get("previous_interaction_id")
                .is_some_and(|v| !v.is_null()),
            EndpointFeature::StoredContinuation,
        ),
        (
            body.get("tools")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|v| !v.is_empty()),
            EndpointFeature::ToolCalling,
        ),
    ] {
        if present {
            requirements = requirements.endpoint_feature(feature);
        }
    }
    requirements
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_every_supported_image_request_parameter_once() {
        let request: CreateImageRequest = serde_json::from_value(serde_json::json!({
            "model": "openai/gpt-image-2",
            "prompt": "a chalk drawing",
            "n": 2,
            "quality": "high",
            "size": "1024x1024",
            "aspect_ratio": "1:1",
            "image_size": "2K"
        }))
        .unwrap();
        let requirements = image_generation_requirements(&request);

        assert_eq!(
            requirements.parameters,
            std::collections::BTreeMap::from([
                ("aspect_ratio".to_string(), serde_json::json!("1:1")),
                ("image_size".to_string(), serde_json::json!("2K")),
                ("n".to_string(), serde_json::json!(2)),
                ("quality".to_string(), serde_json::json!("high")),
                ("size".to_string(), serde_json::json!("1024x1024")),
            ])
        );
    }
}
