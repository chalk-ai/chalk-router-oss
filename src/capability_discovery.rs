//! Provider-native catalog metadata mapped into sparse internal facts.
//!
//! Raw provider fields are never treated as Chalk's public capability schema.

use crate::capabilities::{
    CapabilityFactPatch, DeliveryMode, EndpointFeature, InputModality, Interface,
    InterfaceOperation, ModelFeature, ModelSurface, OperationPatch, OutputModality, Support,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub fn from_provider_metadata(
    provider_kind: &str,
    metadata: &BTreeMap<String, Value>,
) -> Option<CapabilityFactPatch> {
    let value = Value::Object(metadata.clone().into_iter().collect());
    match provider_kind {
        "openrouter" => openrouter(&value),
        "fireworks" => fireworks(&value),
        "bedrock" => bedrock(&value),
        _ => recognized_openai_compatible(&value),
    }
}

pub fn openrouter(value: &Value) -> Option<CapabilityFactPatch> {
    let architecture = value.get("architecture")?;
    let mut patch = CapabilityFactPatch::default();
    map_modalities(
        architecture.get("input_modalities"),
        architecture.get("output_modalities"),
        &mut patch,
    );
    patch.limits.context_window = positive_limit(value.get("context_length"));
    if string_array(value.get("supported_parameters")).contains(&"tools") {
        patch
            .features
            .insert(ModelFeature::ToolCalling, Support::Supported);
        operation(
            &mut patch,
            Interface::OpenaiChatCompletions,
            InterfaceOperation::Create,
        )
        .features
        .insert(EndpointFeature::ToolCalling, Support::Supported);
    }
    infer_surface_from_modalities(&mut patch);
    if patch.surfaces.get(&ModelSurface::ImageGeneration) == Some(&Support::Supported) {
        operation(
            &mut patch,
            Interface::OpenaiImages,
            InterfaceOperation::Generate,
        )
        .support = Some(Support::Supported);
    }
    if patch.surfaces.get(&ModelSurface::Decisions) == Some(&Support::Supported) {
        // Decision models answer typed questions on the Decisions endpoint
        // only; OpenRouter rejects them on chat. The whole window is input:
        // answers are a few tokens however large the state is.
        patch
            .surfaces
            .insert(ModelSurface::Chat, Support::Unsupported);
        patch.limits.max_input_tokens = patch.limits.context_window;
        operation(
            &mut patch,
            Interface::RouterDecisions,
            InterfaceOperation::Create,
        )
        .mode = Some(DeliveryMode::Native);
    }
    Some(patch)
}

pub fn fireworks(value: &Value) -> Option<CapabilityFactPatch> {
    let mut patch = CapabilityFactPatch::default();
    let kind = value
        .get("kind")
        .or_else(|| value.get("workload_type"))
        .and_then(Value::as_str)?;
    match kind {
        "embedding" | "embeddings" => {
            patch
                .surfaces
                .insert(ModelSurface::Embeddings, Support::Supported);
            patch
                .modalities
                .input
                .insert(InputModality::Text, Support::Supported);
            patch
                .modalities
                .output
                .insert(OutputModality::Embeddings, Support::Supported);
            operation(
                &mut patch,
                Interface::OpenaiEmbeddings,
                InterfaceOperation::Create,
            )
            .support = Some(Support::Supported);
        }
        "image" | "image_generation" => {
            patch
                .surfaces
                .insert(ModelSurface::ImageGeneration, Support::Supported);
            patch
                .modalities
                .input
                .insert(InputModality::Text, Support::Supported);
            patch
                .modalities
                .output
                .insert(OutputModality::Image, Support::Supported);
            operation(
                &mut patch,
                Interface::OpenaiImages,
                InterfaceOperation::Generate,
            )
            .support = Some(Support::Supported);
        }
        "text" | "chat" | "conversation" => {
            patch
                .surfaces
                .insert(ModelSurface::Chat, Support::Supported);
            patch
                .modalities
                .input
                .insert(InputModality::Text, Support::Supported);
            patch
                .modalities
                .output
                .insert(OutputModality::Text, Support::Supported);
        }
        _ => return None,
    }
    if value.get("supports_image_input").and_then(Value::as_bool) == Some(true) {
        patch
            .modalities
            .input
            .insert(InputModality::Image, Support::Supported);
    }
    if value.get("supports_tools").and_then(Value::as_bool) == Some(true) {
        patch
            .features
            .insert(ModelFeature::ToolCalling, Support::Supported);
    }
    patch.limits.context_window = positive_limit(value.get("context_length"));
    Some(patch)
}

pub fn bedrock(value: &Value) -> Option<CapabilityFactPatch> {
    let mut patch = CapabilityFactPatch::default();
    map_modalities(
        value.get("inputModalities"),
        value.get("outputModalities"),
        &mut patch,
    );
    infer_surface_from_modalities(&mut patch);
    // `responseStreamingSupported` describes Bedrock's InvokeModel API, not
    // Mantle's OpenAI or Anthropic compatibility endpoints. Do not use it to
    // infer a router protocol capability or override the transport policy.
    (!patch.surfaces.is_empty()).then_some(patch)
}

fn recognized_openai_compatible(value: &Value) -> Option<CapabilityFactPatch> {
    let discovery = value.get("chalk_capability_discovery")?.clone();
    serde_json::from_value(discovery).ok()
}

fn operation(
    patch: &mut CapabilityFactPatch,
    interface: Interface,
    operation: InterfaceOperation,
) -> &mut OperationPatch {
    patch
        .interfaces
        .entry(interface)
        .or_default()
        .operations
        .entry(operation)
        .or_default()
}

fn map_modalities(input: Option<&Value>, output: Option<&Value>, patch: &mut CapabilityFactPatch) {
    for modality in string_array(input) {
        let modality = match modality.to_ascii_lowercase().as_str() {
            "text" => Some(InputModality::Text),
            "image" => Some(InputModality::Image),
            "audio" => Some(InputModality::Audio),
            "video" => Some(InputModality::Video),
            "pdf" | "file" => Some(InputModality::Pdf),
            _ => None,
        };
        if let Some(modality) = modality {
            patch.modalities.input.insert(modality, Support::Supported);
        }
    }
    for modality in string_array(output) {
        let modality = match modality.to_ascii_lowercase().as_str() {
            "text" => Some(OutputModality::Text),
            "image" => Some(OutputModality::Image),
            "audio" => Some(OutputModality::Audio),
            "video" => Some(OutputModality::Video),
            "embeddings" | "embedding" => Some(OutputModality::Embeddings),
            "decisions" => Some(OutputModality::Decisions),
            _ => None,
        };
        if let Some(modality) = modality {
            patch.modalities.output.insert(modality, Support::Supported);
        }
    }
}

fn infer_surface_from_modalities(patch: &mut CapabilityFactPatch) {
    if patch.modalities.output.get(&OutputModality::Image) == Some(&Support::Supported) {
        patch
            .surfaces
            .insert(ModelSurface::ImageGeneration, Support::Supported);
    }
    if patch.modalities.output.get(&OutputModality::Embeddings) == Some(&Support::Supported) {
        patch
            .surfaces
            .insert(ModelSurface::Embeddings, Support::Supported);
    }
    if patch.modalities.output.get(&OutputModality::Decisions) == Some(&Support::Supported) {
        patch
            .surfaces
            .insert(ModelSurface::Decisions, Support::Supported);
    }
    if patch.modalities.output.get(&OutputModality::Text) == Some(&Support::Supported) {
        patch
            .surfaces
            .insert(ModelSurface::Chat, Support::Supported);
    }
}

/// Catalogs report zero for unknown lengths; a zero fact fails validation and
/// would make the whole model unresolvable, so it is treated as absent.
fn positive_limit(value: Option<&Value>) -> Option<Option<u64>> {
    value
        .and_then(Value::as_u64)
        .filter(|limit| *limit > 0)
        .map(Some)
}

fn string_array(value: Option<&Value>) -> Vec<&str> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_openrouter_catalog_metadata() {
        let value: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/capabilities/openrouter-model.json"
        ))
        .unwrap();
        let patch = openrouter(&value).unwrap();
        assert_eq!(
            patch.surfaces[&ModelSurface::ImageGeneration],
            Support::Supported
        );
        assert_eq!(patch.limits.context_window, Some(Some(1_000_000)));
        let resolved =
            crate::capabilities::resolve("openrouter", "google/gemini-image", Some(&patch), None)
                .unwrap();
        assert_eq!(
            resolved.profile.interfaces[&Interface::OpenaiImages].operations
                [&InterfaceOperation::Generate]
                .support,
            Support::Supported
        );
    }

    #[test]
    fn openrouter_decision_catalog_entries_open_the_decisions_surface() {
        let value = serde_json::json!({
            "id": "acme/decider-9",
            "context_length": 131072,
            "architecture": {
                "input_modalities": ["text", "image"],
                "output_modalities": ["decisions"]
            },
            "supported_parameters": []
        });
        let patch = openrouter(&value).unwrap();
        assert_eq!(patch.surfaces[&ModelSurface::Decisions], Support::Supported);
        assert_eq!(patch.surfaces[&ModelSurface::Chat], Support::Unsupported);
        assert_eq!(patch.limits.max_input_tokens, Some(Some(131_072)));
        let resolved =
            crate::capabilities::resolve("openrouter", "acme/decider-9", Some(&patch), None)
                .unwrap();
        assert_eq!(
            resolved.profile.interfaces[&Interface::RouterDecisions].operations
                [&InterfaceOperation::Create]
                .support,
            Support::Supported
        );
        assert_eq!(
            resolved.profile.modalities.input[&InputModality::Image],
            Support::Supported
        );

        let unpublished = serde_json::json!({
            "context_length": 0,
            "architecture": {"input_modalities": ["text"], "output_modalities": ["decisions"]}
        });
        assert_eq!(
            openrouter(&unpublished).unwrap().limits.context_window,
            None
        );
    }

    #[test]
    fn maps_fireworks_and_bedrock_catalog_metadata() {
        let fireworks: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/capabilities/fireworks-model.json"
        ))
        .unwrap();
        let fireworks = super::fireworks(&fireworks).unwrap();
        assert_eq!(fireworks.surfaces[&ModelSurface::Chat], Support::Supported);
        assert_eq!(
            fireworks.features[&ModelFeature::ToolCalling],
            Support::Supported
        );

        for (kind, interface, operation_name) in [
            (
                "embeddings",
                Interface::OpenaiEmbeddings,
                InterfaceOperation::Create,
            ),
            (
                "image_generation",
                Interface::OpenaiImages,
                InterfaceOperation::Generate,
            ),
        ] {
            let mapped = super::fireworks(&serde_json::json!({"kind": kind})).unwrap();
            assert_eq!(
                mapped.interfaces[&interface].operations[&operation_name].support,
                Some(Support::Supported)
            );
        }

        let bedrock: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/capabilities/bedrock-model.json"
        ))
        .unwrap();
        let bedrock = super::bedrock(&bedrock).unwrap();
        assert_eq!(
            bedrock.modalities.input[&InputModality::Image],
            Support::Supported
        );
        assert!(bedrock.interfaces.is_empty());
    }

    #[test]
    fn ignores_unrecognized_generic_capabilities() {
        let unrelated = serde_json::json!({"capabilities": ["fast", "cheap"]});
        assert!(recognized_openai_compatible(&unrelated).is_none());
    }

    #[test]
    fn zero_catalog_lengths_are_unknown_and_still_resolve() {
        let openrouter_zero = openrouter(&serde_json::json!({
            "architecture": {"input_modalities": ["text"], "output_modalities": ["embeddings"]},
            "context_length": 0
        }))
        .unwrap();
        let fireworks_zero =
            fireworks(&serde_json::json!({"kind": "chat", "context_length": 0})).unwrap();
        for (provider, patch) in [
            ("openrouter", openrouter_zero),
            ("fireworks", fireworks_zero),
        ] {
            assert_eq!(patch.limits.context_window, None, "{provider}");
            let resolved =
                crate::capabilities::resolve(provider, "vendor/model", Some(&patch), None).unwrap();
            assert_eq!(resolved.profile.limits.context_window, None, "{provider}");
        }
    }
}
