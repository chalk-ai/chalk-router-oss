//! Model and protocol capability vocabulary.
//!
//! Capability profiles deliberately separate what a model can do from how a
//! particular public protocol operation reaches an upstream. The registry
//! defaults in this module are conservative: connection declarations and
//! discovery can refine them later without changing this stable vocabulary.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const CAPABILITY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    Native,
    Adapted,
    Translated,
    Unsupported,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSurface {
    Chat,
    Decisions,
    Embeddings,
    ImageGeneration,
    Rerank,
    Moderation,
    AudioTranscription,
    AudioSpeech,
    Realtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputModality {
    Text,
    Image,
    Audio,
    Video,
    Pdf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputModality {
    Text,
    Decisions,
    Image,
    Audio,
    Video,
    Embeddings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFeature {
    /// Text input and output without requiring audio or tools.
    TextOnlyChat,
    ToolCalling,
    ParallelToolCalling,
    StrictToolSchema,
    StructuredOutput,
    /// Chat requests must ask for structured output; plain conversational
    /// prompting is not a valid request shape for this model.
    StructuredOutputOnly,
    Reasoning,
    PromptCaching,
    ComputerUse,
    /// Decisions `box` questions: locating something in an image.
    DecisionBoxes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    OpenaiResponses,
    OpenaiChatCompletions,
    AnthropicMessages,
    GeminiInteractions,
    RouterDecisions,
    TypesafeSystemOne,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolOperation {
    Create,
    Retrieve,
    Delete,
    Compact,
    Cancel,
    ListInputItems,
    CountTokens,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointFeature {
    Streaming,
    StoredContinuation,
    Background,
    ToolCalling,
    ImageInput,
    ServerToolWebSearch,
    ServerToolFileSearch,
    ServerToolCodeInterpreter,
    ServerToolComputerUse,
    ServerToolMcp,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_input: Option<EmbeddingInputLimits>,
}

impl ModelLimits {
    /// Omitted patch fields inherit; `null` clears; a value replaces.
    fn apply(&mut self, patch: &ModelLimitsPatch) {
        if let Some(value) = patch.context_window {
            self.context_window = value;
        }
        if let Some(value) = patch.max_input_tokens {
            self.max_input_tokens = value;
        }
        if let Some(value) = patch.max_output_tokens {
            self.max_output_tokens = value;
        }
        if let Some(value) = &patch.embedding_input {
            self.embedding_input = value.clone();
        }
    }
}

/// Published embedding constraints. Missing fields are unknown, not unlimited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingInputLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_images_per_document: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_mime_types: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationProfile {
    pub support: Support,
    #[serde(skip)]
    pub mode: DeliveryMode,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub features: BTreeMap<EndpointFeature, Support>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, ParameterCapability>,
}

impl OperationProfile {
    pub fn new(mode: DeliveryMode) -> Self {
        Self {
            support: match mode {
                DeliveryMode::Native | DeliveryMode::Adapted | DeliveryMode::Translated => {
                    Support::Supported
                }
                DeliveryMode::Unsupported => Support::Unsupported,
                DeliveryMode::Unknown => Support::Unknown,
            },
            mode,
            features: BTreeMap::new(),
            parameters: BTreeMap::new(),
        }
    }

    pub fn feature(mut self, feature: EndpointFeature, support: Support) -> Self {
        self.features.insert(feature, support);
        self
    }

    pub fn parameter(
        mut self,
        name: impl Into<String>,
        support: Support,
        constraint: Option<ParameterConstraint>,
    ) -> Self {
        self.parameters.insert(
            name.into(),
            ParameterCapability {
                support,
                constraint,
            },
        );
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InterfaceProfile {
    #[serde(default)]
    pub operations: BTreeMap<InterfaceOperation, OperationProfile>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modalities {
    #[serde(default)]
    pub input: BTreeMap<InputModality, Support>,
    #[serde(default)]
    pub output: BTreeMap<OutputModality, Support>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityProfile {
    pub schema_version: u32,
    #[serde(default)]
    pub surfaces: BTreeMap<ModelSurface, Support>,
    #[serde(default)]
    pub modalities: Modalities,
    #[serde(default)]
    pub features: BTreeMap<ModelFeature, Support>,
    #[serde(default)]
    pub limits: ModelLimits,
    #[serde(default)]
    pub interfaces: BTreeMap<Interface, InterfaceProfile>,
}

impl Default for CapabilityProfile {
    fn default() -> Self {
        Self {
            schema_version: CAPABILITY_SCHEMA_VERSION,
            surfaces: BTreeMap::new(),
            modalities: Modalities::default(),
            features: BTreeMap::new(),
            limits: ModelLimits::default(),
            interfaces: BTreeMap::new(),
        }
    }
}

impl CapabilityProfile {
    pub fn operation(
        &self,
        protocol: Protocol,
        operation: ProtocolOperation,
    ) -> Option<&OperationProfile> {
        self.interfaces
            .get(&protocol_interface(protocol))
            .and_then(|profile| {
                profile
                    .operations
                    .get(&protocol_operation_interface(operation))
            })
    }

    /// Whether this profile can be attempted for a concrete request.
    ///
    /// Only an explicit `Unsupported` leaf rejects the request. Missing and
    /// `Unknown` leaves are deliberately permissive during the enforcement
    /// rollout; upstream validation remains authoritative for those routes.
    pub fn satisfies(&self, requirements: &CapabilityRequirements) -> bool {
        self.first_failure(requirements).is_none()
    }

    /// Tri-state decision for diagnostics and clients that must distinguish a
    /// verified route from an allowed-but-unverified attempt.
    pub fn support_for(&self, requirements: &CapabilityRequirements) -> Support {
        if self.first_failure(requirements).is_some() {
            return Support::Unsupported;
        }
        let mut unknown = requirements
            .surfaces
            .iter()
            .any(|value| self.surfaces.get(value) != Some(&Support::Supported))
            || requirements
                .modalities
                .input
                .iter()
                .any(|value| self.modalities.input.get(value) != Some(&Support::Supported))
            || requirements
                .modalities
                .output
                .iter()
                .any(|value| self.modalities.output.get(value) != Some(&Support::Supported))
            || requirements
                .features
                .iter()
                .any(|value| self.features.get(value) != Some(&Support::Supported));

        if let Some((interface, operation)) = requirements.interface_operation {
            let operation = self
                .interfaces
                .get(&interface)
                .and_then(|profile| profile.operations.get(&operation));
            unknown |= operation.map(|profile| profile.support) != Some(Support::Supported);
            unknown |= requirements.endpoint_features.iter().any(|feature| {
                operation.and_then(|profile| profile.features.get(feature))
                    != Some(&Support::Supported)
            });
            unknown |= requirements.parameters.keys().any(|name| {
                operation
                    .and_then(|profile| profile.parameters.get(name))
                    .map(|value| value.support)
                    != Some(Support::Supported)
            });
        }

        if unknown {
            Support::Unknown
        } else {
            Support::Supported
        }
    }

    pub fn first_failure(&self, requirements: &CapabilityRequirements) -> Option<String> {
        for surface in &requirements.surfaces {
            if self.surfaces.get(surface) == Some(&Support::Unsupported) {
                return Some(format!("surface '{}' is unsupported", enum_name(surface)));
            }
        }
        for modality in &requirements.modalities.input {
            if self.modalities.input.get(modality) == Some(&Support::Unsupported) {
                return Some(format!(
                    "input modality '{}' is unsupported",
                    enum_name(modality)
                ));
            }
        }
        for modality in &requirements.modalities.output {
            if self.modalities.output.get(modality) == Some(&Support::Unsupported) {
                return Some(format!(
                    "output modality '{}' is unsupported",
                    enum_name(modality)
                ));
            }
        }
        for feature in &requirements.features {
            if self.features.get(feature) == Some(&Support::Unsupported) {
                return Some(format!("feature '{}' is unsupported", enum_name(feature)));
            }
        }
        let (interface, operation) = requirements.interface_operation?;
        let profile = self
            .interfaces
            .get(&interface)
            .and_then(|interface| interface.operations.get(&operation));
        let profile = profile?;
        if profile.support == Support::Unsupported {
            return Some(format!(
                "{}.{} is unsupported",
                enum_name(&interface),
                enum_name(&operation)
            ));
        }
        for feature in &requirements.endpoint_features {
            if profile.features.get(feature) == Some(&Support::Unsupported) {
                return Some(format!(
                    "operation feature '{}' is unsupported",
                    enum_name(feature)
                ));
            }
        }
        for (name, value) in &requirements.parameters {
            let Some(parameter) = profile.parameters.get(name) else {
                continue;
            };
            if parameter.support == Support::Unsupported {
                return Some(format!("parameter '{name}' is unsupported"));
            }
            if parameter.support == Support::Supported
                && parameter
                    .constraint
                    .as_ref()
                    .is_some_and(|constraint| !parameter_value_allowed(constraint, value))
            {
                return Some(format!("parameter '{name}' value {value} is unsupported"));
            }
        }
        None
    }

    /// Applies a sparse declaration one leaf at a time.
    ///
    /// An explicit `Unknown` is a value and therefore masks a lower-priority
    /// default. Absent fields inherit. Limit patches use `null` to explicitly
    /// clear a lower-priority value. An embedding-input object replaces the
    /// whole record; absent members of that object are unknown.
    pub fn overlay(&mut self, patch: &CapabilityFactPatch) -> Result<(), CapabilitySchemaError> {
        patch.validate()?;
        self.surfaces
            .extend(patch.surfaces.iter().map(|(key, value)| (*key, *value)));
        self.modalities.input.extend(
            patch
                .modalities
                .input
                .iter()
                .map(|(key, value)| (*key, *value)),
        );
        self.modalities.output.extend(
            patch
                .modalities
                .output
                .iter()
                .map(|(key, value)| (*key, *value)),
        );
        self.features
            .extend(patch.features.iter().map(|(key, value)| (*key, *value)));

        self.limits.apply(&patch.limits);

        for (interface, interface_patch) in &patch.interfaces {
            let target = self.interfaces.entry(*interface).or_default();
            for (operation, operation_patch) in &interface_patch.operations {
                let target = target
                    .operations
                    .entry(*operation)
                    .or_insert_with(|| OperationProfile::new(DeliveryMode::Unknown));
                if let Some(mode) = operation_patch.mode {
                    target.mode = mode;
                    target.support = match mode {
                        DeliveryMode::Native | DeliveryMode::Adapted | DeliveryMode::Translated => {
                            Support::Supported
                        }
                        DeliveryMode::Unsupported => Support::Unsupported,
                        DeliveryMode::Unknown => Support::Unknown,
                    };
                }
                if let Some(support) = operation_patch.support {
                    target.support = support;
                }
                target.features.extend(
                    operation_patch
                        .features
                        .iter()
                        .map(|(feature, support)| (*feature, *support)),
                );
                target.parameters.extend(
                    operation_patch
                        .parameters
                        .iter()
                        .map(|(name, capability)| (name.clone(), capability.clone())),
                );
            }
        }
        Ok(())
    }
}

/// Capability leaves required by one concrete request. This is intentionally
/// transient request state, not persisted metadata or a caller-supplied
/// override.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilityRequirements {
    pub interface_operation: Option<(Interface, InterfaceOperation)>,
    pub surfaces: BTreeSet<ModelSurface>,
    pub modalities: RequiredModalities,
    pub features: BTreeSet<ModelFeature>,
    pub endpoint_features: BTreeSet<EndpointFeature>,
    pub parameters: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequiredModalities {
    pub input: BTreeSet<InputModality>,
    pub output: BTreeSet<OutputModality>,
}

impl CapabilityRequirements {
    pub fn for_operation(protocol: Protocol, operation: ProtocolOperation) -> Self {
        Self {
            interface_operation: Some((
                protocol_interface(protocol),
                protocol_operation_interface(operation),
            )),
            ..Self::default()
        }
    }

    pub fn for_interface_operation(interface: Interface, operation: InterfaceOperation) -> Self {
        Self {
            interface_operation: Some((interface, operation)),
            ..Self::default()
        }
    }

    pub fn surface(mut self, surface: ModelSurface) -> Self {
        self.surfaces.insert(surface);
        self
    }

    pub fn input(mut self, modality: InputModality) -> Self {
        self.modalities.input.insert(modality);
        self
    }

    pub fn output(mut self, modality: OutputModality) -> Self {
        self.modalities.output.insert(modality);
        self
    }

    pub fn feature(mut self, feature: ModelFeature) -> Self {
        self.features.insert(feature);
        self
    }

    pub fn endpoint_feature(mut self, feature: EndpointFeature) -> Self {
        self.endpoint_features.insert(feature);
        self
    }

    pub fn parameter(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.parameters.insert(name.into(), value.into());
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLimitsPatch {
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub context_window: Option<Option<u64>>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_input_tokens: Option<Option<u64>>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_output_tokens: Option<Option<u64>>,
    /// Omitted inherits, null clears, and an object replaces the entire record.
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub embedding_input: Option<Option<EmbeddingInputLimits>>,
}

fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<DeliveryMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support: Option<Support>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub features: BTreeMap<EndpointFeature, Support>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, ParameterCapability>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterfaceFactPatch {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub operations: BTreeMap<InterfaceOperation, OperationPatch>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModalitiesPatch {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input: BTreeMap<InputModality, Support>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub output: BTreeMap<OutputModality, Support>,
}

fn modalities_patch_is_empty(patch: &ModalitiesPatch) -> bool {
    patch.input.is_empty() && patch.output.is_empty()
}

/// Internal sparse facts returned by provider discovery or connection-shape
/// detection. This is deliberately not the persisted declaration document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityFactPatch {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub surfaces: BTreeMap<ModelSurface, Support>,
    #[serde(default, skip_serializing_if = "modalities_patch_is_empty")]
    pub modalities: ModalitiesPatch,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub features: BTreeMap<ModelFeature, Support>,
    #[serde(default, skip_serializing_if = "limits_patch_is_empty")]
    pub limits: ModelLimitsPatch,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub interfaces: BTreeMap<Interface, InterfaceFactPatch>,
}

impl Default for CapabilityFactPatch {
    fn default() -> Self {
        Self {
            schema_version: CAPABILITY_SCHEMA_VERSION,
            surfaces: BTreeMap::new(),
            modalities: ModalitiesPatch::default(),
            features: BTreeMap::new(),
            limits: ModelLimitsPatch::default(),
            interfaces: BTreeMap::new(),
        }
    }
}

impl CapabilityFactPatch {
    /// Set one operation's support while preserving all other capability facts.
    pub fn with_operation_support(
        mut self,
        interface: Interface,
        operation: InterfaceOperation,
        support: Support,
    ) -> Self {
        self.interfaces
            .entry(interface)
            .or_default()
            .operations
            .entry(operation)
            .or_default()
            .support = Some(support);
        self
    }

    pub fn validate(&self) -> Result<(), CapabilitySchemaError> {
        if self.schema_version != CAPABILITY_SCHEMA_VERSION {
            return Err(CapabilitySchemaError {
                schema_version: self.schema_version,
                detail: None,
            });
        }
        validate_limits_patch(&self.limits, self.schema_version)?;
        for interface in self.interfaces.values() {
            for operation in interface.operations.values() {
                for (name, parameter) in &operation.parameters {
                    validate_parameter(name, parameter, self.schema_version)?;
                }
            }
        }
        Ok(())
    }
}

/// Sparse facts intrinsic to a model. Protocol/interface availability is not
/// accepted here because it belongs to the configured endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCapabilityPatch {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub surfaces: BTreeMap<ModelSurface, Support>,
    #[serde(default, skip_serializing_if = "modalities_patch_is_empty")]
    pub modalities: ModalitiesPatch,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub features: BTreeMap<ModelFeature, Support>,
    #[serde(default, skip_serializing_if = "limits_patch_is_empty")]
    pub limits: ModelLimitsPatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interface {
    OpenaiChatCompletions,
    OpenaiResponses,
    AnthropicMessages,
    GeminiInteractions,
    RouterDecisions,
    TypesafeSystemOne,
    OpenaiEmbeddings,
    OpenaiModerations,
    OpenaiImages,
    OpenaiAudio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceOperation {
    Create,
    Retrieve,
    Delete,
    Compact,
    Cancel,
    ListInputItems,
    CountTokens,
    Generate,
    Edit,
    Variations,
    Speech,
    Transcription,
    Translation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ParameterConstraint {
    Any,
    StringEnum { values: Vec<String> },
    IntegerRange { min: i64, max: i64 },
    NumberRange { min: f64, max: f64 },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParameterCapability {
    pub support: Support,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraint: Option<ParameterConstraint>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredOperationPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support: Option<Support>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub features: BTreeMap<EndpointFeature, Support>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, ParameterCapability>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredInterfacePatch {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub operations: BTreeMap<InterfaceOperation, DeclaredOperationPatch>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointCapabilityPatch {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub interfaces: BTreeMap<Interface, DeclaredInterfacePatch>,
}

/// Operator-authored connection declaration persisted in
/// `provider_connections.declared_capabilities`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityPatch {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "model_patch_is_empty")]
    pub model_defaults: ModelCapabilityPatch,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, ModelCapabilityPatch>,
    #[serde(default, skip_serializing_if = "endpoint_patch_is_empty")]
    pub endpoint: EndpointCapabilityPatch,
}

impl Default for CapabilityPatch {
    fn default() -> Self {
        Self {
            schema_version: CAPABILITY_SCHEMA_VERSION,
            model_defaults: ModelCapabilityPatch::default(),
            models: BTreeMap::new(),
            endpoint: EndpointCapabilityPatch::default(),
        }
    }
}

impl CapabilityPatch {
    pub fn validate(&self) -> Result<(), CapabilitySchemaError> {
        if self.schema_version != CAPABILITY_SCHEMA_VERSION {
            return Err(CapabilitySchemaError {
                schema_version: self.schema_version,
                detail: None,
            });
        }
        for (model, patch) in &self.models {
            if model.trim().is_empty() {
                return Err(CapabilitySchemaError {
                    schema_version: self.schema_version,
                    detail: Some("model override ids must be non-empty".to_string()),
                });
            }
            validate_model_patch(patch, self.schema_version)?;
        }
        validate_model_patch(&self.model_defaults, self.schema_version)?;
        for interface in self.endpoint.interfaces.values() {
            for operation in interface.operations.values() {
                for (name, parameter) in &operation.parameters {
                    validate_parameter(name, parameter, self.schema_version)?;
                }
            }
        }
        Ok(())
    }
}

fn validate_model_patch(
    patch: &ModelCapabilityPatch,
    schema_version: u32,
) -> Result<(), CapabilitySchemaError> {
    validate_limits_patch(&patch.limits, schema_version)
}

fn validate_limits_patch(
    limits: &ModelLimitsPatch,
    schema_version: u32,
) -> Result<(), CapabilitySchemaError> {
    for value in [
        limits.context_window,
        limits.max_input_tokens,
        limits.max_output_tokens,
    ]
    .into_iter()
    .flatten()
    .flatten()
    {
        if value == 0 {
            return Err(CapabilitySchemaError {
                schema_version,
                detail: Some("capability limits must be greater than zero".to_string()),
            });
        }
    }
    if let Some(Some(input)) = &limits.embedding_input {
        let invalid = |detail: &str| CapabilitySchemaError {
            schema_version,
            detail: Some(detail.to_string()),
        };
        if input.max_images_per_document.is_none() && input.image_mime_types.is_none() {
            return Err(invalid("embedding_input must contain at least one limit"));
        }
        if input.max_images_per_document == Some(0) {
            return Err(invalid("embedding image limits must be greater than zero"));
        }
        if let Some(types) = &input.image_mime_types {
            if types.is_empty() {
                return Err(invalid("image_mime_types must not be empty"));
            }
            let mut seen = std::collections::BTreeSet::new();
            for mime_type in types {
                let valid_subtype = mime_type.strip_prefix("image/").is_some_and(|subtype| {
                    !subtype.is_empty()
                        && subtype.bytes().all(|byte| {
                            byte.is_ascii_lowercase()
                                || byte.is_ascii_digit()
                                || b"!#$%&'+-.^_`|~".contains(&byte)
                        })
                });
                if !valid_subtype || !seen.insert(mime_type) {
                    return Err(invalid(
                        "image_mime_types must contain unique lowercase image MIME types without parameters or wildcards",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_parameter(
    name: &str,
    parameter: &ParameterCapability,
    schema_version: u32,
) -> Result<(), CapabilitySchemaError> {
    let invalid = |detail: String| CapabilitySchemaError {
        schema_version,
        detail: Some(detail),
    };
    if name.trim().is_empty() {
        return Err(invalid("parameter names must be non-empty".to_string()));
    }
    if parameter.support == Support::Supported && parameter.constraint.is_none() {
        return Err(invalid(format!(
            "supported parameter '{name}' requires a constraint"
        )));
    }
    if parameter.support != Support::Supported && parameter.constraint.is_some() {
        return Err(invalid(format!(
            "parameter '{name}' may only carry a constraint when supported"
        )));
    }
    match &parameter.constraint {
        Some(ParameterConstraint::StringEnum { values })
            if values.is_empty()
                || values.iter().any(|value| value.is_empty())
                || values.iter().collect::<BTreeSet<_>>().len() != values.len() =>
        {
            Err(invalid(format!(
                "parameter '{name}' string_enum values must be non-empty and unique"
            )))
        }
        Some(ParameterConstraint::IntegerRange { min, max }) if min > max => Err(invalid(format!(
            "parameter '{name}' integer_range min exceeds max"
        ))),
        Some(ParameterConstraint::NumberRange { min, max })
            if !min.is_finite() || !max.is_finite() || min > max =>
        {
            Err(invalid(format!(
                "parameter '{name}' number_range must be finite with min <= max"
            )))
        }
        _ => Ok(()),
    }
}

fn model_patch_is_empty(patch: &ModelCapabilityPatch) -> bool {
    patch.surfaces.is_empty()
        && modalities_patch_is_empty(&patch.modalities)
        && patch.features.is_empty()
        && limits_patch_is_empty(&patch.limits)
}

fn endpoint_patch_is_empty(patch: &EndpointCapabilityPatch) -> bool {
    patch.interfaces.is_empty()
}

fn limits_patch_is_empty(patch: &ModelLimitsPatch) -> bool {
    patch.context_window.is_none()
        && patch.max_input_tokens.is_none()
        && patch.max_output_tokens.is_none()
        && patch.embedding_input.is_none()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilitySchemaError {
    pub schema_version: u32,
    pub detail: Option<String>,
}

impl std::fmt::Display for CapabilitySchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(detail) = &self.detail {
            write!(f, "invalid capability declaration: {detail}")
        } else {
            write!(
                f,
                "unsupported capability schema version {}; expected {}",
                self.schema_version, CAPABILITY_SCHEMA_VERSION
            )
        }
    }
}

impl std::error::Error for CapabilitySchemaError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySource {
    ConnectionDeclaration,
    UpstreamDiscovery,
    ProviderRegistry,
    ProviderEndpoint,
    ChalkAdapter,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityPathSegment {
    Field(String),
    Key(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CapabilityPath(pub Vec<CapabilityPathSegment>);

impl CapabilityPath {
    fn fields(fields: &[&str]) -> Self {
        Self(
            fields
                .iter()
                .map(|field| CapabilityPathSegment::Field((*field).to_string()))
                .collect(),
        )
    }

    fn model_override(model: &str) -> Self {
        Self(vec![
            CapabilityPathSegment::Field("models".to_string()),
            CapabilityPathSegment::Key(model.to_string()),
        ])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityEvidence {
    pub path: CapabilityPath,
    pub source: CapabilitySource,
    pub rule: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityConflict {
    pub path: CapabilityPath,
    pub preferred: Value,
    pub overridden: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OperationKey {
    pub interface: Interface,
    pub operation: InterfaceOperation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedCapabilityProfile {
    #[serde(flatten)]
    pub profile: CapabilityProfile,
    #[serde(skip)]
    pub evidence: Vec<CapabilityEvidence>,
    #[serde(skip)]
    pub conflicts: Vec<CapabilityConflict>,
    #[serde(skip)]
    pub delivery: BTreeMap<OperationKey, DeliveryMode>,
}

/// Emit one bounded, structured decision record wherever a concrete request is
/// checked. Model ids are fields, never metric labels; capability documents are
/// deliberately omitted.
pub(crate) fn trace_decision(
    route: &crate::backend::ResolvedRoute,
    requested_model: &str,
    requirements: &CapabilityRequirements,
    operation_name: &str,
) -> Support {
    let support = route.capabilities.profile.support_for(requirements);
    let delivery = requirements
        .interface_operation
        .and_then(|(interface, operation)| {
            route
                .capabilities
                .delivery
                .get(&OperationKey {
                    interface,
                    operation,
                })
                .copied()
        })
        .unwrap_or(DeliveryMode::Unknown);
    let registry_rule = route
        .capabilities
        .evidence
        .iter()
        .find_map(|evidence| evidence.rule.as_deref())
        .unwrap_or("none");
    let evidence_sources: BTreeSet<_> = route
        .capabilities
        .evidence
        .iter()
        .map(|evidence| evidence.source)
        .collect();
    tracing::debug!(
        capability.requested_model = %requested_model,
        capability.upstream_model = %route.resolution.upstream_model_id,
        capability.operation = operation_name,
        capability.support = ?support,
        capability.delivery = ?delivery,
        capability.unknown_attempted = support == Support::Unknown,
        capability.registry_rule = registry_rule,
        capability.evidence_sources = ?evidence_sources,
        "chalk.router.capability.decision"
    );
    support
}

const CAPABILITY_DISCOVERY_CACHE_CAPACITY: usize = 10_000;
const CAPABILITY_DISCOVERY_CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DiscoveryKey {
    connection_fingerprint: String,
    provider_kind: String,
    model: String,
}

struct CachedDiscovery {
    facts: CapabilityFactPatch,
    refreshed_at: Instant,
}

/// Process-local discovery catalog shared by request resolution and model
/// list/detail projection. Effective profiles are cheap to resolve and are not
/// cached, so caller-controlled model ids cannot create permanent entries.
pub struct CapabilityCatalog {
    discovery: Mutex<HashMap<DiscoveryKey, CachedDiscovery>>,
    discovery_capacity: usize,
    discovery_ttl: Duration,
}

impl CapabilityCatalog {
    pub fn new() -> Self {
        Self::with_config(
            CAPABILITY_DISCOVERY_CACHE_CAPACITY,
            CAPABILITY_DISCOVERY_CACHE_TTL,
        )
    }

    pub fn resolve(
        &self,
        connection_fingerprint: &str,
        provider_kind: &str,
        model: &str,
        connection_facts: Option<&CapabilityFactPatch>,
        discovered: Option<&CapabilityFactPatch>,
        declared: Option<&CapabilityPatch>,
    ) -> Result<Arc<ResolvedCapabilityProfile>, CapabilitySchemaError> {
        let discovery_key = DiscoveryKey {
            connection_fingerprint: connection_fingerprint.to_string(),
            provider_kind: provider_kind.to_string(),
            model: model.to_string(),
        };
        let discovered = self.discovery_for(discovery_key, discovered);
        Ok(Arc::new(resolve_with_connection_facts(
            provider_kind,
            model,
            connection_facts,
            discovered.as_ref(),
            declared,
        )?))
    }

    fn with_config(discovery_capacity: usize, discovery_ttl: Duration) -> Self {
        Self {
            discovery: Mutex::new(HashMap::new()),
            discovery_capacity: discovery_capacity.max(1),
            discovery_ttl,
        }
    }

    fn discovery_for(
        &self,
        key: DiscoveryKey,
        fresh: Option<&CapabilityFactPatch>,
    ) -> Option<CapabilityFactPatch> {
        let now = Instant::now();
        let mut discovery = self
            .discovery
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(fresh) = fresh {
            if !discovery.contains_key(&key) && discovery.len() >= self.discovery_capacity {
                discovery
                    .retain(|_, value| now.duration_since(value.refreshed_at) < self.discovery_ttl);
            }
            if !discovery.contains_key(&key) && discovery.len() >= self.discovery_capacity {
                if let Some(oldest) = discovery
                    .iter()
                    .min_by_key(|(_, value)| value.refreshed_at)
                    .map(|(key, _)| key.clone())
                {
                    discovery.remove(&oldest);
                }
            }
            discovery.insert(
                key,
                CachedDiscovery {
                    facts: fresh.clone(),
                    refreshed_at: now,
                },
            );
            return Some(fresh.clone());
        }

        let cached = discovery.get(&key).and_then(|value| {
            (now.duration_since(value.refreshed_at) < self.discovery_ttl)
                .then(|| value.facts.clone())
        });
        if cached.is_none() {
            discovery.remove(&key);
        }
        cached
    }

    #[cfg(test)]
    fn discovery_len(&self) -> usize {
        self.discovery
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

impl Default for CapabilityCatalog {
    fn default() -> Self {
        Self::new()
    }
}

/// Conservative client-specific booleans derived from the canonical profile.
/// Client projections are outputs only; they are never persisted as capability
/// truth or merged back into a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VscodeCapabilityProjection {
    pub tool_calling: bool,
    pub vision: bool,
}

pub struct ClientCapabilityProjection;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PlaygroundCapabilityProjection {
    pub chat_support: Support,
    /// None means no confirmed streaming text-chat route.
    pub chat_protocol: Option<Protocol>,
}

impl ClientCapabilityProjection {
    pub fn playground(profile: &CapabilityProfile) -> PlaygroundCapabilityProjection {
        if profile.features.get(&ModelFeature::TextOnlyChat) == Some(&Support::Unsupported)
            || profile.features.get(&ModelFeature::StructuredOutputOnly)
                == Some(&Support::Supported)
        {
            return PlaygroundCapabilityProjection {
                chat_support: Support::Unsupported,
                chat_protocol: None,
            };
        }
        let mut chat_support = Support::Unsupported;
        let chat_protocol = [Protocol::OpenaiResponses, Protocol::OpenaiChatCompletions]
            .into_iter()
            .find(|protocol| {
                let requirements =
                    CapabilityRequirements::for_operation(*protocol, ProtocolOperation::Create)
                        .surface(ModelSurface::Chat)
                        .input(InputModality::Text)
                        .output(OutputModality::Text)
                        .endpoint_feature(EndpointFeature::Streaming);
                let support = profile.support_for(&requirements);
                if support != Support::Unsupported {
                    chat_support = support;
                }
                support == Support::Supported
            });
        PlaygroundCapabilityProjection {
            chat_support,
            chat_protocol,
        }
    }

    /// Project the Responses ingress fields understood by VS Code. Unknown is
    /// deliberately false so generated client configuration never over-promises.
    pub fn vscode(profile: &CapabilityProfile) -> VscodeCapabilityProjection {
        let create = profile.operation(Protocol::OpenaiResponses, ProtocolOperation::Create);
        let usable_create = create.is_some_and(|operation| {
            !matches!(
                operation.mode,
                DeliveryMode::Unsupported | DeliveryMode::Unknown
            )
        });
        VscodeCapabilityProjection {
            tool_calling: usable_create
                && profile.features.get(&ModelFeature::ToolCalling) == Some(&Support::Supported)
                && create
                    .and_then(|operation| operation.features.get(&EndpointFeature::ToolCalling))
                    == Some(&Support::Supported),
            vision: usable_create
                && profile.modalities.input.get(&InputModality::Image) == Some(&Support::Supported)
                && create
                    .and_then(|operation| operation.features.get(&EndpointFeature::ImageInput))
                    == Some(&Support::Supported),
        }
    }
}

/// Resolves model, endpoint, and adapter facts independently and publishes
/// their intersection for one canonical upstream model on one connection.
pub fn resolve(
    provider_kind: &str,
    model: &str,
    discovered: Option<&CapabilityFactPatch>,
    declared: Option<&CapabilityPatch>,
) -> Result<ResolvedCapabilityProfile, CapabilitySchemaError> {
    resolve_with_connection_facts(provider_kind, model, None, discovered, declared)
}

fn resolve_with_connection_facts(
    provider_kind: &str,
    model: &str,
    connection_facts: Option<&CapabilityFactPatch>,
    discovered: Option<&CapabilityFactPatch>,
    declared: Option<&CapabilityPatch>,
) -> Result<ResolvedCapabilityProfile, CapabilitySchemaError> {
    let registry_match = crate::model_capability_registry::lookup(provider_kind, model);
    let mut model_layer = CapabilityProfile::default();
    let model_facts = registry_match
        .as_ref()
        .map(|matched| matched.facts.clone())
        .unwrap_or_else(|| crate::model_capability_registry::provider_default(provider_kind));
    overlay_model_patch(&mut model_layer, &model_facts);

    let mut adapter_layer = crate::provider_registry::capabilities(provider_kind, model);
    adapter_layer.surfaces.clear();
    adapter_layer.modalities = Modalities::default();
    adapter_layer.features.clear();
    adapter_layer.limits = ModelLimits::default();
    // The provider defaults describe the concrete endpoint. Discovery and the
    // operator declaration may refine this copy, while adapter facts remain
    // immutable code-owned limits.
    let mut endpoint_layer = adapter_layer.clone();
    if let Some(endpoint_facts) = registry_match
        .as_ref()
        .and_then(|matched| matched.endpoint_facts.as_ref())
    {
        endpoint_layer.overlay(endpoint_facts)?;
    }
    if let Some(connection_facts) = connection_facts {
        endpoint_layer.overlay(connection_facts)?;
        overlay_adapter_delivery(&mut adapter_layer, connection_facts);
    }
    let mut evidence = vec![CapabilityEvidence {
        path: CapabilityPath::fields(&["model"]),
        source: CapabilitySource::ProviderRegistry,
        rule: registry_match
            .as_ref()
            .map(|matched| matched.rule_name.to_string()),
    }];

    if let Some(discovered) = discovered {
        model_layer.overlay(discovered)?;
        endpoint_layer.overlay(discovered)?;
        overlay_adapter_delivery(&mut adapter_layer, discovered);
        evidence.push(CapabilityEvidence {
            path: CapabilityPath::fields(&["discovery"]),
            source: CapabilitySource::UpstreamDiscovery,
            rule: None,
        });
    }

    let mut conflicts = Vec::new();
    if let Some(declared) = declared {
        declared.validate()?;
        record_model_conflicts(
            &model_layer,
            &declared.model_defaults,
            CapabilityPath::fields(&["model_defaults"]),
            &mut conflicts,
        );
        overlay_model_patch(&mut model_layer, &declared.model_defaults);
        evidence.push(CapabilityEvidence {
            path: CapabilityPath::fields(&["model_defaults"]),
            source: CapabilitySource::ConnectionDeclaration,
            rule: None,
        });
        if let Some(model_patch) = declared.models.get(model) {
            record_model_conflicts(
                &model_layer,
                model_patch,
                CapabilityPath::model_override(model),
                &mut conflicts,
            );
            overlay_model_patch(&mut model_layer, model_patch);
            evidence.push(CapabilityEvidence {
                path: CapabilityPath::model_override(model),
                source: CapabilitySource::ConnectionDeclaration,
                rule: None,
            });
        }
        overlay_endpoint_patch(&mut endpoint_layer, &declared.endpoint);
        evidence.push(CapabilityEvidence {
            path: CapabilityPath::fields(&["endpoint"]),
            source: CapabilitySource::ConnectionDeclaration,
            rule: None,
        });
    }

    evidence.push(CapabilityEvidence {
        path: CapabilityPath::fields(&["endpoint"]),
        source: CapabilitySource::ProviderEndpoint,
        rule: None,
    });
    evidence.push(CapabilityEvidence {
        path: CapabilityPath::fields(&["adapter"]),
        source: CapabilitySource::ChalkAdapter,
        rule: None,
    });

    materialize_unknown_model_axes(&mut model_layer);
    // Adapter support follows the final model surface facts, including
    // discovery and declarations. Endpoint support remains independently
    // sourced above, so model facts alone cannot advertise an operation.
    add_surface_interfaces(&mut adapter_layer, provider_kind, &model_layer);
    let (profile, delivery) = intersect_layers(&model_layer, &endpoint_layer, &adapter_layer);

    Ok(ResolvedCapabilityProfile {
        profile,
        evidence,
        conflicts,
        delivery,
    })
}

fn overlay_adapter_delivery(profile: &mut CapabilityProfile, patch: &CapabilityFactPatch) {
    for (interface, interface_patch) in &patch.interfaces {
        for (operation, operation_patch) in &interface_patch.operations {
            let Some(mode) = operation_patch.mode else {
                continue;
            };
            let target = profile
                .interfaces
                .entry(*interface)
                .or_default()
                .operations
                .entry(*operation)
                .or_insert_with(|| OperationProfile::new(mode));
            target.mode = mode;
            target.support = match mode {
                DeliveryMode::Native | DeliveryMode::Adapted | DeliveryMode::Translated => {
                    Support::Supported
                }
                DeliveryMode::Unsupported => Support::Unsupported,
                DeliveryMode::Unknown => Support::Unknown,
            };
        }
    }
}

fn record_model_conflicts(
    inherited: &CapabilityProfile,
    declared: &ModelCapabilityPatch,
    base_path: CapabilityPath,
    conflicts: &mut Vec<CapabilityConflict>,
) {
    for (surface, preferred) in &declared.surfaces {
        record_leaf_conflict(
            inherited.surfaces.get(surface),
            preferred,
            keyed_path(&base_path, "surfaces", enum_name(surface)),
            conflicts,
        );
    }
    for (modality, preferred) in &declared.modalities.input {
        record_leaf_conflict(
            inherited.modalities.input.get(modality),
            preferred,
            keyed_path(&base_path, "modalities.input", enum_name(modality)),
            conflicts,
        );
    }
    for (modality, preferred) in &declared.modalities.output {
        record_leaf_conflict(
            inherited.modalities.output.get(modality),
            preferred,
            keyed_path(&base_path, "modalities.output", enum_name(modality)),
            conflicts,
        );
    }
    for (feature, preferred) in &declared.features {
        record_leaf_conflict(
            inherited.features.get(feature),
            preferred,
            keyed_path(&base_path, "features", enum_name(feature)),
            conflicts,
        );
    }
    for (field, inherited, preferred) in [
        (
            "context_window",
            inherited.limits.context_window,
            declared.limits.context_window,
        ),
        (
            "max_input_tokens",
            inherited.limits.max_input_tokens,
            declared.limits.max_input_tokens,
        ),
        (
            "max_output_tokens",
            inherited.limits.max_output_tokens,
            declared.limits.max_output_tokens,
        ),
    ] {
        if let (Some(inherited), Some(preferred)) = (inherited, preferred) {
            record_leaf_conflict(
                Some(&Some(inherited)),
                &preferred,
                field_path(&base_path, &["limits", field]),
                conflicts,
            );
        }
    }
    if let (Some(inherited), Some(preferred)) = (
        &inherited.limits.embedding_input,
        &declared.limits.embedding_input,
    ) {
        record_leaf_conflict(
            Some(&Some(inherited.clone())),
            preferred,
            field_path(&base_path, &["limits", "embedding_input"]),
            conflicts,
        );
    }
}

fn record_leaf_conflict<T: PartialEq + Serialize>(
    inherited: Option<&T>,
    preferred: &T,
    path: CapabilityPath,
    conflicts: &mut Vec<CapabilityConflict>,
) {
    let Some(inherited) = inherited.filter(|inherited| *inherited != preferred) else {
        return;
    };
    conflicts.push(CapabilityConflict {
        path,
        preferred: serde_json::to_value(preferred).expect("capability leaf serializes"),
        overridden: serde_json::to_value(inherited).expect("capability leaf serializes"),
    });
}

fn field_path(base: &CapabilityPath, fields: &[&str]) -> CapabilityPath {
    let mut path = base.clone();
    path.0.extend(
        fields
            .iter()
            .map(|field| CapabilityPathSegment::Field((*field).to_string())),
    );
    path
}

fn keyed_path(base: &CapabilityPath, field: &str, key: String) -> CapabilityPath {
    let mut path = field_path(base, &field.split('.').collect::<Vec<_>>());
    path.0.push(CapabilityPathSegment::Key(key));
    path
}

pub(crate) fn overlay_model_patch(profile: &mut CapabilityProfile, patch: &ModelCapabilityPatch) {
    profile
        .surfaces
        .extend(patch.surfaces.iter().map(|(key, value)| (*key, *value)));
    profile.modalities.input.extend(
        patch
            .modalities
            .input
            .iter()
            .map(|(key, value)| (*key, *value)),
    );
    profile.modalities.output.extend(
        patch
            .modalities
            .output
            .iter()
            .map(|(key, value)| (*key, *value)),
    );
    profile
        .features
        .extend(patch.features.iter().map(|(key, value)| (*key, *value)));
    profile.limits.apply(&patch.limits);
}

fn overlay_endpoint_patch(profile: &mut CapabilityProfile, patch: &EndpointCapabilityPatch) {
    for (interface, interface_patch) in &patch.interfaces {
        let target = profile.interfaces.entry(*interface).or_default();
        for (operation, operation_patch) in &interface_patch.operations {
            let target = target
                .operations
                .entry(*operation)
                .or_insert_with(|| OperationProfile::new(DeliveryMode::Unknown));
            if let Some(support) = operation_patch.support {
                target.support = support;
                target.mode = match support {
                    Support::Supported => target.mode,
                    Support::Unsupported => DeliveryMode::Unsupported,
                    Support::Unknown => DeliveryMode::Unknown,
                };
            }
            target.features.extend(
                operation_patch
                    .features
                    .iter()
                    .map(|(feature, support)| (*feature, *support)),
            );
            target.parameters.extend(
                operation_patch
                    .parameters
                    .iter()
                    .map(|(name, capability)| (name.clone(), capability.clone())),
            );
        }
    }
}

fn insert_operation(
    profile: &mut CapabilityProfile,
    interface: Interface,
    operation: InterfaceOperation,
    mode: DeliveryMode,
) -> &mut OperationProfile {
    profile
        .interfaces
        .entry(interface)
        .or_default()
        .operations
        .entry(operation)
        .or_insert_with(|| OperationProfile::new(mode))
}

fn set_decision_operations(
    profile: &mut CapabilityProfile,
    router_decisions: DeliveryMode,
    typesafe_system_one: DeliveryMode,
) {
    for (interface, mode) in [
        (Interface::RouterDecisions, router_decisions),
        (Interface::TypesafeSystemOne, typesafe_system_one),
    ] {
        profile
            .interfaces
            .entry(interface)
            .or_default()
            .operations
            .insert(InterfaceOperation::Create, OperationProfile::new(mode));
    }
}

fn add_surface_interfaces(
    profile: &mut CapabilityProfile,
    provider_kind: &str,
    model: &CapabilityProfile,
) {
    if model.surfaces.get(&ModelSurface::Embeddings) == Some(&Support::Supported) {
        insert_operation(
            profile,
            Interface::OpenaiEmbeddings,
            InterfaceOperation::Create,
            DeliveryMode::Native,
        );
    }
    if model.surfaces.get(&ModelSurface::ImageGeneration) == Some(&Support::Supported) {
        let generate = insert_operation(
            profile,
            Interface::OpenaiImages,
            InterfaceOperation::Generate,
            DeliveryMode::Native,
        );
        match provider_kind {
            "openai" => {
                *generate = generate
                    .clone()
                    .parameter("size", Support::Supported, Some(ParameterConstraint::Any))
                    .parameter(
                        "quality",
                        Support::Supported,
                        Some(ParameterConstraint::Any),
                    )
                    .parameter("n", Support::Supported, Some(ParameterConstraint::Any));
            }
            "gemini" => {
                *generate = generate.clone().parameter(
                    "size",
                    Support::Supported,
                    Some(ParameterConstraint::Any),
                );
            }
            _ => {}
        }
        insert_operation(
            profile,
            Interface::OpenaiImages,
            InterfaceOperation::Edit,
            DeliveryMode::Unsupported,
        );
        insert_operation(
            profile,
            Interface::OpenaiImages,
            InterfaceOperation::Variations,
            DeliveryMode::Unsupported,
        );
    }
    if model.surfaces.get(&ModelSurface::AudioSpeech) == Some(&Support::Supported) {
        insert_operation(
            profile,
            Interface::OpenaiAudio,
            InterfaceOperation::Speech,
            DeliveryMode::Native,
        );
    }
    if model.surfaces.get(&ModelSurface::AudioTranscription) == Some(&Support::Supported) {
        insert_operation(
            profile,
            Interface::OpenaiAudio,
            InterfaceOperation::Transcription,
            DeliveryMode::Native,
        );
        insert_operation(
            profile,
            Interface::OpenaiAudio,
            InterfaceOperation::Translation,
            DeliveryMode::Native,
        );
    }
}

fn materialize_unknown_model_axes(profile: &mut CapabilityProfile) {
    for surface in [
        ModelSurface::Chat,
        ModelSurface::Decisions,
        ModelSurface::Embeddings,
        ModelSurface::ImageGeneration,
        ModelSurface::Rerank,
        ModelSurface::Moderation,
        ModelSurface::AudioTranscription,
        ModelSurface::AudioSpeech,
        ModelSurface::Realtime,
    ] {
        profile.surfaces.entry(surface).or_insert(Support::Unknown);
    }
    for modality in [
        InputModality::Text,
        InputModality::Image,
        InputModality::Audio,
        InputModality::Video,
        InputModality::Pdf,
    ] {
        profile
            .modalities
            .input
            .entry(modality)
            .or_insert(Support::Unknown);
    }
    for modality in [
        OutputModality::Text,
        OutputModality::Decisions,
        OutputModality::Image,
        OutputModality::Audio,
        OutputModality::Video,
        OutputModality::Embeddings,
    ] {
        profile
            .modalities
            .output
            .entry(modality)
            .or_insert(Support::Unknown);
    }
    for feature in [
        ModelFeature::ToolCalling,
        ModelFeature::ParallelToolCalling,
        ModelFeature::StrictToolSchema,
        ModelFeature::StructuredOutput,
        ModelFeature::StructuredOutputOnly,
        ModelFeature::Reasoning,
        ModelFeature::PromptCaching,
        ModelFeature::ComputerUse,
        ModelFeature::DecisionBoxes,
    ] {
        profile.features.entry(feature).or_insert(Support::Unknown);
    }
}

fn intersect_layers(
    model: &CapabilityProfile,
    endpoint: &CapabilityProfile,
    adapter: &CapabilityProfile,
) -> (CapabilityProfile, BTreeMap<OperationKey, DeliveryMode>) {
    let mut effective = model.clone();
    effective.interfaces.clear();
    let mut delivery = BTreeMap::new();
    let interfaces: BTreeSet<_> = endpoint
        .interfaces
        .keys()
        .chain(adapter.interfaces.keys())
        .copied()
        .collect();
    for interface in interfaces {
        let endpoint_interface = endpoint.interfaces.get(&interface);
        let adapter_interface = adapter.interfaces.get(&interface);
        let operations: BTreeSet<_> = endpoint_interface
            .into_iter()
            .flat_map(|profile| profile.operations.keys())
            .chain(
                adapter_interface
                    .into_iter()
                    .flat_map(|profile| profile.operations.keys()),
            )
            .copied()
            .collect();
        for operation in operations {
            let endpoint_operation =
                endpoint_interface.and_then(|profile| profile.operations.get(&operation));
            let adapter_operation =
                adapter_interface.and_then(|profile| profile.operations.get(&operation));
            let endpoint_support = endpoint_operation
                .map(|profile| profile.support)
                .unwrap_or(Support::Unknown);
            let adapter_support = adapter_operation
                .map(|profile| profile.support)
                .unwrap_or(Support::Unknown);
            let model_support = if interface == Interface::GeminiInteractions
                && operation == InterfaceOperation::Create
                && model.surfaces.get(&ModelSurface::ImageGeneration) == Some(&Support::Supported)
            {
                // Interactions also generates images. Excluding image models from text chat
                // must not disable their native image-generation route.
                Support::Supported
            } else {
                operation_surface(interface, operation)
                    .and_then(|surface| model.surfaces.get(&surface).copied())
                    .unwrap_or(Support::Unknown)
            };
            let support = intersect_support(
                intersect_support(endpoint_support, adapter_support),
                model_support,
            );
            let adapter_mode = adapter_operation
                .map(|profile| profile.mode)
                .unwrap_or(DeliveryMode::Unknown);
            let mode = match support {
                Support::Supported => adapter_mode,
                Support::Unsupported => DeliveryMode::Unsupported,
                Support::Unknown => DeliveryMode::Unknown,
            };
            let mut profile = OperationProfile::new(mode);
            profile.support = support;

            let features: BTreeSet<_> = endpoint_operation
                .into_iter()
                .flat_map(|profile| profile.features.keys())
                .chain(
                    adapter_operation
                        .into_iter()
                        .flat_map(|profile| profile.features.keys()),
                )
                .copied()
                .collect();
            for feature in features {
                let endpoint_support = endpoint_operation
                    .and_then(|profile| profile.features.get(&feature))
                    .copied()
                    .unwrap_or(Support::Unknown);
                let adapter_support = adapter_operation
                    .and_then(|profile| profile.features.get(&feature))
                    .copied()
                    .unwrap_or(Support::Unknown);
                profile.features.insert(
                    feature,
                    intersect_support(endpoint_support, adapter_support),
                );
            }

            let parameters: BTreeSet<_> = endpoint_operation
                .into_iter()
                .flat_map(|profile| profile.parameters.keys())
                .chain(
                    adapter_operation
                        .into_iter()
                        .flat_map(|profile| profile.parameters.keys()),
                )
                .cloned()
                .collect();
            for parameter in parameters {
                profile.parameters.insert(
                    parameter.clone(),
                    intersect_parameter_capability(
                        endpoint_operation.and_then(|profile| profile.parameters.get(&parameter)),
                        adapter_operation.and_then(|profile| profile.parameters.get(&parameter)),
                    ),
                );
            }

            effective
                .interfaces
                .entry(interface)
                .or_default()
                .operations
                .insert(operation, profile);
            delivery.insert(
                OperationKey {
                    interface,
                    operation,
                },
                mode,
            );
        }
    }
    (effective, delivery)
}

pub fn intersect_support(left: Support, right: Support) -> Support {
    match (left, right) {
        (Support::Unsupported, _) | (_, Support::Unsupported) => Support::Unsupported,
        (Support::Supported, Support::Supported) => Support::Supported,
        _ => Support::Unknown,
    }
}

fn intersect_parameter_capability(
    left: Option<&ParameterCapability>,
    right: Option<&ParameterCapability>,
) -> ParameterCapability {
    let left_support = left.map(|value| value.support).unwrap_or(Support::Unknown);
    let right_support = right.map(|value| value.support).unwrap_or(Support::Unknown);
    let mut support = intersect_support(left_support, right_support);
    let constraint = if support == Support::Supported {
        match (
            left.and_then(|value| value.constraint.as_ref()),
            right.and_then(|value| value.constraint.as_ref()),
        ) {
            (Some(left), Some(right)) => intersect_constraint(left, right),
            (Some(known), None) | (None, Some(known)) => Some(known.clone()),
            (None, None) => Some(ParameterConstraint::Unknown),
        }
    } else {
        None
    };
    if support == Support::Supported && constraint.is_none() {
        support = Support::Unsupported;
    }
    ParameterCapability {
        support,
        constraint,
    }
}

fn intersect_constraint(
    left: &ParameterConstraint,
    right: &ParameterConstraint,
) -> Option<ParameterConstraint> {
    match (left, right) {
        (ParameterConstraint::Any, other) | (other, ParameterConstraint::Any) => {
            Some(other.clone())
        }
        (ParameterConstraint::Unknown, other) | (other, ParameterConstraint::Unknown) => {
            Some(other.clone())
        }
        (
            ParameterConstraint::StringEnum { values: left },
            ParameterConstraint::StringEnum { values: right },
        ) => {
            let right: BTreeSet<_> = right.iter().collect();
            let values: Vec<_> = left
                .iter()
                .filter(|value| right.contains(value))
                .cloned()
                .collect();
            (!values.is_empty()).then_some(ParameterConstraint::StringEnum { values })
        }
        (
            ParameterConstraint::IntegerRange {
                min: left_min,
                max: left_max,
            },
            ParameterConstraint::IntegerRange {
                min: right_min,
                max: right_max,
            },
        ) => {
            let min = (*left_min).max(*right_min);
            let max = (*left_max).min(*right_max);
            (min <= max).then_some(ParameterConstraint::IntegerRange { min, max })
        }
        (
            ParameterConstraint::NumberRange {
                min: left_min,
                max: left_max,
            },
            ParameterConstraint::NumberRange {
                min: right_min,
                max: right_max,
            },
        ) => {
            let min = left_min.max(*right_min);
            let max = left_max.min(*right_max);
            (min <= max).then_some(ParameterConstraint::NumberRange { min, max })
        }
        _ => None,
    }
}

fn parameter_value_allowed(constraint: &ParameterConstraint, value: &Value) -> bool {
    match constraint {
        ParameterConstraint::Any | ParameterConstraint::Unknown => true,
        ParameterConstraint::StringEnum { values } => value
            .as_str()
            .is_some_and(|value| values.iter().any(|allowed| allowed == value)),
        ParameterConstraint::IntegerRange { min, max } => value
            .as_i64()
            .is_some_and(|value| value >= *min && value <= *max),
        ParameterConstraint::NumberRange { min, max } => value
            .as_f64()
            .is_some_and(|value| value >= *min && value <= *max),
    }
}

fn enum_name(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn operation_surface(interface: Interface, operation: InterfaceOperation) -> Option<ModelSurface> {
    match (interface, operation) {
        (Interface::OpenaiChatCompletions, InterfaceOperation::Create)
        | (Interface::OpenaiResponses, _)
        | (Interface::AnthropicMessages, _)
        | (Interface::GeminiInteractions, _) => Some(ModelSurface::Chat),
        (Interface::RouterDecisions, InterfaceOperation::Create)
        | (Interface::TypesafeSystemOne, InterfaceOperation::Create) => {
            Some(ModelSurface::Decisions)
        }
        (Interface::OpenaiEmbeddings, InterfaceOperation::Create) => Some(ModelSurface::Embeddings),
        (Interface::OpenaiModerations, InterfaceOperation::Create) => {
            Some(ModelSurface::Moderation)
        }
        (Interface::OpenaiImages, _) => Some(ModelSurface::ImageGeneration),
        (Interface::OpenaiAudio, InterfaceOperation::Speech) => Some(ModelSurface::AudioSpeech),
        (Interface::OpenaiAudio, InterfaceOperation::Transcription)
        | (Interface::OpenaiAudio, InterfaceOperation::Translation) => {
            Some(ModelSurface::AudioTranscription)
        }
        _ => None,
    }
}

fn protocol_interface(protocol: Protocol) -> Interface {
    match protocol {
        Protocol::OpenaiChatCompletions => Interface::OpenaiChatCompletions,
        Protocol::OpenaiResponses => Interface::OpenaiResponses,
        Protocol::AnthropicMessages => Interface::AnthropicMessages,
        Protocol::GeminiInteractions => Interface::GeminiInteractions,
        Protocol::RouterDecisions => Interface::RouterDecisions,
        Protocol::TypesafeSystemOne => Interface::TypesafeSystemOne,
    }
}

fn protocol_operation_interface(operation: ProtocolOperation) -> InterfaceOperation {
    match operation {
        ProtocolOperation::Create => InterfaceOperation::Create,
        ProtocolOperation::Retrieve => InterfaceOperation::Retrieve,
        ProtocolOperation::Delete => InterfaceOperation::Delete,
        ProtocolOperation::Compact => InterfaceOperation::Compact,
        ProtocolOperation::Cancel => InterfaceOperation::Cancel,
        ProtocolOperation::ListInputItems => InterfaceOperation::ListInputItems,
        ProtocolOperation::CountTokens => InterfaceOperation::CountTokens,
    }
}

pub(crate) fn generation_profile(
    responses: DeliveryMode,
    messages: DeliveryMode,
    count_tokens: DeliveryMode,
) -> CapabilityProfile {
    let mut profile = CapabilityProfile::default();
    profile
        .surfaces
        .insert(ModelSurface::Chat, Support::Supported);
    profile
        .modalities
        .input
        .insert(InputModality::Text, Support::Supported);
    profile
        .modalities
        .output
        .insert(OutputModality::Text, Support::Supported);

    let mut responses_profile = InterfaceProfile::default();
    responses_profile.operations.insert(
        InterfaceOperation::Create,
        OperationProfile::new(responses).feature(EndpointFeature::Streaming, Support::Supported),
    );
    profile
        .interfaces
        .insert(Interface::OpenaiResponses, responses_profile);

    let mut chat_profile = InterfaceProfile::default();
    chat_profile.operations.insert(
        InterfaceOperation::Create,
        OperationProfile::new(DeliveryMode::Native)
            .feature(EndpointFeature::Streaming, Support::Supported),
    );
    profile
        .interfaces
        .insert(Interface::OpenaiChatCompletions, chat_profile);

    let mut messages_profile = InterfaceProfile::default();
    messages_profile.operations.insert(
        InterfaceOperation::Create,
        OperationProfile::new(messages).feature(EndpointFeature::Streaming, Support::Supported),
    );
    messages_profile.operations.insert(
        InterfaceOperation::CountTokens,
        OperationProfile::new(count_tokens),
    );
    profile
        .interfaces
        .insert(Interface::AnthropicMessages, messages_profile);
    set_decision_operations(
        &mut profile,
        DeliveryMode::Unsupported,
        DeliveryMode::Unsupported,
    );
    profile
}

fn apply_registry_model_facts(profile: &mut CapabilityProfile, provider_kind: &str, model: &str) {
    profile.surfaces.clear();
    profile.modalities = Modalities::default();
    profile.features.clear();
    profile.limits = ModelLimits::default();
    let facts = crate::model_capability_registry::lookup(provider_kind, model)
        .map(|matched| matched.facts)
        .unwrap_or_else(|| crate::model_capability_registry::provider_default(provider_kind));
    overlay_model_patch(profile, &facts);
}

pub(crate) fn openai_capabilities(model: &str) -> CapabilityProfile {
    let mut profile = generation_profile(
        DeliveryMode::Native,
        DeliveryMode::Translated,
        DeliveryMode::Unsupported,
    );
    let responses = profile
        .interfaces
        .get_mut(&Interface::OpenaiResponses)
        .expect("generation profile has Responses");
    let create = responses
        .operations
        .get_mut(&InterfaceOperation::Create)
        .expect("generation profile has create");
    for feature in [
        EndpointFeature::StoredContinuation,
        EndpointFeature::Background,
        EndpointFeature::ToolCalling,
        EndpointFeature::ImageInput,
        EndpointFeature::ServerToolWebSearch,
        EndpointFeature::ServerToolFileSearch,
        EndpointFeature::ServerToolCodeInterpreter,
        EndpointFeature::ServerToolComputerUse,
        EndpointFeature::ServerToolMcp,
    ] {
        create.features.insert(feature, Support::Supported);
    }
    responses.operations.insert(
        InterfaceOperation::Retrieve,
        OperationProfile::new(DeliveryMode::Native)
            .feature(EndpointFeature::Streaming, Support::Supported),
    );
    for operation in [
        InterfaceOperation::Delete,
        InterfaceOperation::Compact,
        InterfaceOperation::Cancel,
    ] {
        responses
            .operations
            .insert(operation, OperationProfile::new(DeliveryMode::Native));
    }
    apply_registry_model_facts(&mut profile, "openai", model);
    if crate::decisions::is_openai_decision_model(model) {
        set_decision_operations(
            &mut profile,
            DeliveryMode::Translated,
            DeliveryMode::Unsupported,
        );
    }
    profile
}

pub(crate) fn perplexity_capabilities(model: &str) -> CapabilityProfile {
    let mut profile = translated_generation_capabilities(model);
    apply_registry_model_facts(&mut profile, "perplexity", model);
    if crate::decisions::is_perplexity_decision_model(model) {
        set_decision_operations(
            &mut profile,
            DeliveryMode::Native,
            DeliveryMode::Unsupported,
        );
    }
    profile
}

pub(crate) fn anthropic_capabilities(model: &str) -> CapabilityProfile {
    let mut profile = generation_profile(
        DeliveryMode::Translated,
        DeliveryMode::Native,
        DeliveryMode::Native,
    );
    apply_registry_model_facts(&mut profile, "anthropic", model);
    profile
}

pub(crate) fn translated_generation_capabilities(_: &str) -> CapabilityProfile {
    generation_profile(
        DeliveryMode::Translated,
        DeliveryMode::Translated,
        DeliveryMode::Unsupported,
    )
}

/// OpenAI generation clients use Google's compatibility endpoint; Embedding 2
/// is adapted to the native API to support images and combined text/image input.
pub(crate) fn gemini_capabilities(model: &str) -> CapabilityProfile {
    let mut profile = translated_generation_capabilities(model);
    let mut native = InterfaceProfile::default();
    native.operations.insert(
        InterfaceOperation::Create,
        OperationProfile::new(DeliveryMode::Native)
            .feature(EndpointFeature::Streaming, Support::Supported)
            .feature(EndpointFeature::StoredContinuation, Support::Supported)
            .feature(EndpointFeature::ToolCalling, Support::Supported)
            .feature(EndpointFeature::ImageInput, Support::Supported)
            .feature(EndpointFeature::Background, Support::Unsupported),
    );
    for operation in [
        InterfaceOperation::Retrieve,
        InterfaceOperation::Delete,
        InterfaceOperation::Cancel,
    ] {
        native
            .operations
            .insert(operation, OperationProfile::new(DeliveryMode::Unsupported));
    }
    profile
        .interfaces
        .insert(Interface::GeminiInteractions, native);
    apply_registry_model_facts(&mut profile, "gemini", model);
    if crate::embedding_limits::gemini_embedding_2_limits(model).is_some() {
        insert_operation(
            &mut profile,
            Interface::OpenaiEmbeddings,
            InterfaceOperation::Create,
            DeliveryMode::Adapted,
        )
        .mode = DeliveryMode::Adapted;
    }
    profile
}

pub(crate) fn vertex_capabilities(model: &str) -> CapabilityProfile {
    let mut profile = generation_profile(
        DeliveryMode::Translated,
        DeliveryMode::Adapted,
        DeliveryMode::Adapted,
    );
    apply_registry_model_facts(&mut profile, "vertex", model);
    profile
}

pub(crate) fn unknown_generation_capabilities(_: &str) -> CapabilityProfile {
    generation_profile(
        DeliveryMode::Translated,
        DeliveryMode::Unknown,
        DeliveryMode::Unknown,
    )
}

pub(crate) fn native_responses_capabilities(_: &str) -> CapabilityProfile {
    generation_profile(
        DeliveryMode::Native,
        DeliveryMode::Translated,
        DeliveryMode::Unsupported,
    )
}

pub(crate) fn native_responses_and_messages_capabilities(_: &str) -> CapabilityProfile {
    generation_profile(
        DeliveryMode::Native,
        DeliveryMode::Native,
        DeliveryMode::Unknown,
    )
}

pub(crate) fn xai_capabilities(model: &str) -> CapabilityProfile {
    let mut profile = native_responses_capabilities(model);
    apply_registry_model_facts(&mut profile, "xai", model);
    profile
}

pub(crate) fn bedrock_capabilities(model: &str) -> CapabilityProfile {
    // Mantle's OpenAI-compatible Chat Completions endpoint is the portable
    // generation transport for Bedrock providers. Responses is deliberately
    // translated through that endpoint unless a connection explicitly
    // advertises native Responses support; the Mantle catalog does not expose
    // that fact per model. This makes newly-added Bedrock models follow the
    // same deterministic path without a hand-maintained model allowlist.
    //
    // The documented closed GPT models are the exception: Mantle's
    // `/openai/v1/responses` serves them natively, and translating through
    // Chat would reject `reasoning`, `store` and function tools (Chat on
    // those models refuses tools alongside reasoning effort).
    let closed_gpt = crate::backend::bedrock::is_mantle_closed_gpt_model(model);
    let mut profile = generation_profile(
        if closed_gpt {
            DeliveryMode::Native
        } else {
            DeliveryMode::Translated
        },
        DeliveryMode::Translated,
        DeliveryMode::Unsupported,
    );
    if closed_gpt {
        // Response IDs are scoped to the Mantle endpoint that created them.
        // A continuation names its model, which selects that endpoint again;
        // the ID-only operations (retrieve, cancel, delete) do not, and the
        // response locator does not record the endpoint, so those and
        // background (which needs retrieve) stay unsupported.
        let create = profile
            .interfaces
            .get_mut(&Interface::OpenaiResponses)
            .and_then(|interface| interface.operations.get_mut(&InterfaceOperation::Create))
            .expect("generation profile has Responses create");
        for feature in [
            EndpointFeature::StoredContinuation,
            EndpointFeature::ToolCalling,
            EndpointFeature::ImageInput,
            // Verified directly against Mantle's `/openai/v1/responses`.
            EndpointFeature::ServerToolWebSearch,
        ] {
            create.features.insert(feature, Support::Supported);
        }
        // Server tools that were not verified are declared, not left unknown:
        // an unknown feature passes eligibility and the resulting Mantle 400
        // ends the fallback walk, whereas an unsupported one skips to the next
        // candidate before dispatch.
        for feature in [
            EndpointFeature::ServerToolFileSearch,
            EndpointFeature::ServerToolCodeInterpreter,
            EndpointFeature::ServerToolComputerUse,
            EndpointFeature::ServerToolMcp,
        ] {
            create.features.insert(feature, Support::Unsupported);
        }
    }
    if crate::backend::bedrock::is_bedrock_anthropic_model(model) {
        // Anthropic is the one Mantle dialect that does not use OpenAI Chat
        // upstream: Chat is adapted to Messages, and Messages is native.
        profile
            .interfaces
            .get_mut(&Interface::OpenaiChatCompletions)
            .and_then(|interface| interface.operations.get_mut(&InterfaceOperation::Create))
            .expect("generation profile has Chat Completions create")
            .mode = DeliveryMode::Adapted;
        profile
            .interfaces
            .get_mut(&Interface::AnthropicMessages)
            .and_then(|interface| interface.operations.get_mut(&InterfaceOperation::Create))
            .expect("generation profile has Messages create")
            .mode = DeliveryMode::Native;
        profile
            .interfaces
            .get_mut(&Interface::AnthropicMessages)
            .and_then(|interface| {
                interface
                    .operations
                    .get_mut(&InterfaceOperation::CountTokens)
            })
            .expect("generation profile has Messages count_tokens")
            .mode = DeliveryMode::Native;
    }
    apply_registry_model_facts(&mut profile, "bedrock", model);
    profile
}

pub(crate) fn bedrock_runtime_capabilities(_: &str) -> CapabilityProfile {
    let mut profile = generation_profile(
        DeliveryMode::Adapted,
        DeliveryMode::Translated,
        DeliveryMode::Adapted,
    );
    if let Some(create) = profile
        .interfaces
        .get_mut(&Interface::OpenaiChatCompletions)
        .and_then(|interface| interface.operations.get_mut(&InterfaceOperation::Create))
    {
        create.mode = DeliveryMode::Adapted;
    }
    profile
}

pub(crate) fn bedrock_runtime_patch() -> CapabilityFactPatch {
    let profile = bedrock_runtime_capabilities("");
    CapabilityFactPatch {
        schema_version: CAPABILITY_SCHEMA_VERSION,
        interfaces: profile
            .interfaces
            .into_iter()
            .map(|(interface, profile)| {
                (
                    interface,
                    InterfaceFactPatch {
                        operations: profile
                            .operations
                            .into_iter()
                            .map(|(operation, profile)| {
                                (
                                    operation,
                                    OperationPatch {
                                        mode: Some(profile.mode),
                                        support: Some(profile.support),
                                        features: profile.features,
                                        parameters: profile.parameters,
                                    },
                                )
                            })
                            .collect(),
                    },
                )
            })
            .collect(),
        ..CapabilityFactPatch::default()
    }
}

pub(crate) fn voyage_capabilities(model: &str) -> CapabilityProfile {
    let mut profile = CapabilityProfile::default();
    profile
        .surfaces
        .insert(ModelSurface::Embeddings, Support::Supported);
    profile
        .modalities
        .input
        .insert(InputModality::Text, Support::Supported);
    profile
        .modalities
        .output
        .insert(OutputModality::Embeddings, Support::Supported);
    profile.interfaces.insert(
        Interface::OpenaiResponses,
        InterfaceProfile {
            operations: BTreeMap::from([(
                InterfaceOperation::Create,
                OperationProfile::new(DeliveryMode::Unsupported),
            )]),
        },
    );
    set_decision_operations(
        &mut profile,
        DeliveryMode::Unsupported,
        DeliveryMode::Unsupported,
    );
    apply_registry_model_facts(&mut profile, "voyage", model);
    profile
}

pub(crate) fn typesafe_capabilities(model: &str) -> CapabilityProfile {
    let mut profile = CapabilityProfile::default();
    set_decision_operations(&mut profile, DeliveryMode::Adapted, DeliveryMode::Native);
    // Jev prompting: chat completions are adapted onto `POST /v1/systemone`
    // by `jev_prompting`. Streaming is synthesized from the whole answer;
    // tools and images have no System One equivalent, so they are refused at
    // routing rather than after a paid upstream call.
    profile.interfaces.insert(
        Interface::OpenaiChatCompletions,
        InterfaceProfile {
            operations: BTreeMap::from([(
                InterfaceOperation::Create,
                OperationProfile::new(DeliveryMode::Adapted)
                    .feature(EndpointFeature::Streaming, Support::Supported)
                    .feature(EndpointFeature::ToolCalling, Support::Unsupported)
                    .feature(EndpointFeature::ImageInput, Support::Unsupported),
            )]),
        },
    );
    for interface in [Interface::OpenaiResponses, Interface::AnthropicMessages] {
        insert_operation(
            &mut profile,
            interface,
            InterfaceOperation::Create,
            DeliveryMode::Unsupported,
        );
    }
    apply_registry_model_facts(&mut profile, "typesafe", model);
    profile
}

pub(crate) fn openrouter_capabilities(model: &str) -> CapabilityProfile {
    // Anthropic models served through OpenRouter use Anthropic's native
    // Messages format rather than OpenRouter's OpenAI-compatible translation
    // layer. Delegate the "which nested provider is this?" question to the
    // registry's shared helper so this never disagrees with
    // `model_capability_registry::lookup`'s own nested-provider handling.
    let messages = match crate::model_capability_registry::openrouter_nested_provider(model) {
        Some(("anthropic", _)) => DeliveryMode::Native,
        _ => DeliveryMode::Translated,
    };
    let mut profile = generation_profile(DeliveryMode::Native, messages, DeliveryMode::Unknown);
    apply_registry_model_facts(&mut profile, "openrouter", model);
    if crate::decisions::is_openrouter_jev_version(model) {
        set_decision_operations(&mut profile, DeliveryMode::Native, DeliveryMode::Native);
    } else if crate::decisions::is_openrouter_decision_model(model) {
        // System One ingress names TypeSafe-native models only.
        set_decision_operations(
            &mut profile,
            DeliveryMode::Native,
            DeliveryMode::Unsupported,
        );
    }
    profile
}

pub(crate) fn fireworks_capabilities(model: &str) -> CapabilityProfile {
    let mut profile = generation_profile(
        DeliveryMode::Native,
        DeliveryMode::Translated,
        DeliveryMode::Unsupported,
    );
    apply_registry_model_facts(&mut profile, "fireworks", model);
    profile
}

pub(crate) fn qwen_capabilities(model: &str) -> CapabilityProfile {
    let mut profile = native_responses_and_messages_capabilities(model);
    apply_registry_model_facts(&mut profile, "qwen", model);
    profile
}

/// Conservative defaults for arbitrary OpenAI-compatible endpoints.
pub fn openai_compatible_capabilities(model: &str) -> CapabilityProfile {
    unknown_generation_capabilities(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_patch_is_field_wise_and_unknown_masks_defaults() {
        let mut profile = openai_capabilities("gpt-5");
        profile.limits.context_window = Some(400_000);
        let patch = CapabilityFactPatch {
            surfaces: BTreeMap::from([(ModelSurface::Chat, Support::Unknown)]),
            limits: ModelLimitsPatch {
                context_window: Some(None),
                max_output_tokens: Some(Some(128_000)),
                ..Default::default()
            },
            interfaces: BTreeMap::from([(
                Interface::OpenaiResponses,
                InterfaceFactPatch {
                    operations: BTreeMap::from([(
                        InterfaceOperation::Create,
                        OperationPatch {
                            features: BTreeMap::from([(
                                EndpointFeature::Background,
                                Support::Unsupported,
                            )]),
                            ..Default::default()
                        },
                    )]),
                },
            )]),
            ..Default::default()
        };

        profile.overlay(&patch).unwrap();
        assert_eq!(profile.surfaces[&ModelSurface::Chat], Support::Unknown);
        assert_eq!(profile.limits.context_window, None);
        assert_eq!(profile.limits.max_output_tokens, Some(128_000));
        let create = profile
            .operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
            .unwrap();
        assert_eq!(create.mode, DeliveryMode::Native);
        assert_eq!(
            create.features[&EndpointFeature::Background],
            Support::Unsupported
        );
        assert_eq!(
            create.features[&EndpointFeature::Streaming],
            Support::Supported
        );
    }

    #[test]
    fn enforcement_rejects_only_explicitly_unsupported_requirements() {
        let requirements = CapabilityRequirements::for_operation(
            Protocol::OpenaiChatCompletions,
            ProtocolOperation::Create,
        )
        .surface(ModelSurface::Chat)
        .input(InputModality::Image)
        .output(OutputModality::Text)
        .feature(ModelFeature::ToolCalling)
        .endpoint_feature(EndpointFeature::Streaming);

        let mut unknown = CapabilityProfile::default();
        unknown
            .surfaces
            .insert(ModelSurface::Chat, Support::Unknown);
        unknown
            .modalities
            .input
            .insert(InputModality::Image, Support::Unknown);
        assert!(unknown.satisfies(&requirements));
        assert_eq!(unknown.support_for(&requirements), Support::Unknown);

        unknown
            .features
            .insert(ModelFeature::ToolCalling, Support::Unsupported);
        assert!(!unknown.satisfies(&requirements));
        assert_eq!(unknown.support_for(&requirements), Support::Unsupported);
    }

    #[test]
    fn schema_version_is_validated_before_merge() {
        let mut profile = CapabilityProfile::default();
        let patch = CapabilityFactPatch {
            schema_version: 2,
            surfaces: BTreeMap::from([(ModelSurface::Chat, Support::Supported)]),
            ..Default::default()
        };

        assert_eq!(
            profile.overlay(&patch),
            Err(CapabilitySchemaError {
                schema_version: 2,
                detail: None,
            })
        );
        assert!(profile.surfaces.is_empty());
    }

    #[test]
    fn internal_fact_parameters_are_validated_before_merge() {
        let patch: CapabilityFactPatch = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "interfaces": {
                "openai_images": {
                    "operations": {
                        "generate": {
                            "parameters": {
                                "size": {"support": "supported"}
                            }
                        }
                    }
                }
            }
        }))
        .unwrap();
        let error = CapabilityProfile::default().overlay(&patch).unwrap_err();
        assert_eq!(
            error.detail.as_deref(),
            Some("supported parameter 'size' requires a constraint")
        );
    }

    #[test]
    fn json_uses_stable_snake_case_keys() {
        let profile = anthropic_capabilities("claude-sonnet");
        let json = serde_json::to_value(profile).unwrap();
        assert_eq!(json["surfaces"]["chat"], "supported");
        assert_eq!(json["modalities"]["input"]["text"], "supported");
        assert_eq!(
            json["interfaces"]["anthropic_messages"]["operations"]["create"]["support"],
            "supported"
        );
        assert_eq!(
            json["interfaces"]["anthropic_messages"]["operations"]["count_tokens"]["support"],
            "supported"
        );
        assert!(
            json["interfaces"]["anthropic_messages"]["operations"]["create"]
                .get("mode")
                .is_none()
        );
    }

    #[test]
    fn limit_patch_distinguishes_inherit_clear_and_value() {
        let inherited: ModelLimitsPatch = serde_json::from_str("{}").unwrap();
        let cleared: ModelLimitsPatch = serde_json::from_str(r#"{"context_window":null}"#).unwrap();
        let set: ModelLimitsPatch = serde_json::from_str(r#"{"context_window":200000}"#).unwrap();

        assert_eq!(inherited.context_window, None);
        assert_eq!(cleared.context_window, Some(None));
        assert_eq!(set.context_window, Some(Some(200_000)));
    }

    #[test]
    fn embedding_limit_patch_inherits_clears_and_replaces_the_whole_record() {
        let input = serde_json::json!({
            "max_images_per_document": 6,
            "image_mime_types": ["image/png", "image/jpeg"]
        });
        let mut profile = CapabilityProfile::default();
        for (limits, expected) in [
            (
                serde_json::json!({"embedding_input": input.clone()}),
                input.clone(),
            ),
            (serde_json::json!({}), input),
            (
                serde_json::json!({"embedding_input": {"max_images_per_document": 2}}),
                serde_json::json!({"max_images_per_document": 2}),
            ),
            (serde_json::json!({"embedding_input": null}), Value::Null),
        ] {
            let patch: CapabilityFactPatch = serde_json::from_value(serde_json::json!({
                "schema_version": 1, "limits": limits
            }))
            .unwrap();
            profile.overlay(&patch).unwrap();
            let actual = serde_json::to_value(&profile).unwrap();
            assert_eq!(actual["limits"]["embedding_input"], expected);
        }
        let omitted: ModelLimitsPatch = serde_json::from_value(serde_json::json!({})).unwrap();
        let cleared: ModelLimitsPatch =
            serde_json::from_value(serde_json::json!({"embedding_input": null})).unwrap();
        assert_eq!(omitted.embedding_input, None);
        assert_eq!(cleared.embedding_input, Some(None));
        assert!(limits_patch_is_empty(&omitted));
        assert!(!limits_patch_is_empty(&cleared));
        assert_eq!(
            serde_json::to_value(cleared).unwrap(),
            serde_json::json!({"embedding_input": null})
        );
        assert!(serde_json::to_value(profile).unwrap()["limits"]
            .get("embedding_input")
            .is_none());
    }

    #[test]
    fn embedding_limit_validation_covers_facts_and_declarations() {
        for input in [
            serde_json::json!({}),
            serde_json::json!({"max_images_per_document": 0}),
            serde_json::json!({"image_mime_types": []}),
            serde_json::json!({"image_mime_types": [""]}),
            serde_json::json!({"image_mime_types": ["image/png", "image/png"]}),
            serde_json::json!({"image_mime_types": ["image/PNG"]}),
            serde_json::json!({"image_mime_types": ["text/plain"]}),
            serde_json::json!({"image_mime_types": ["image/*"]}),
            serde_json::json!({"image_mime_types": ["image/png; charset=utf-8"]}),
        ] {
            let limits = serde_json::json!({"embedding_input": input});
            let facts: CapabilityFactPatch = serde_json::from_value(serde_json::json!({
                "schema_version": 1, "limits": limits.clone()
            }))
            .unwrap();
            assert!(facts.validate().is_err(), "{limits}");
            let declaration: CapabilityPatch = serde_json::from_value(serde_json::json!({
                "schema_version": 1, "model_defaults": {"limits": limits.clone()}
            }))
            .unwrap();
            assert!(declaration.validate().is_err(), "{limits}");
        }
        let unknown = serde_json::json!({"embedding_input": {"max_images": 6}});
        assert!(serde_json::from_value::<ModelLimitsPatch>(unknown).is_err());
        for input in [
            Value::Null,
            serde_json::json!({"max_images_per_document": 1}),
            serde_json::json!({"image_mime_types": ["image/svg+xml"]}),
        ] {
            let facts: CapabilityFactPatch = serde_json::from_value(serde_json::json!({
                "schema_version": 1, "limits": {"embedding_input": input}
            }))
            .unwrap();
            facts.validate().unwrap();
        }
    }

    #[test]
    fn embedding_limit_declarations_replace_defaults_and_record_conflicts() {
        let declared: CapabilityPatch = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "model_defaults": {"limits": {"embedding_input": {"max_images_per_document": 2}}},
            "models": {"gemini-embedding-2": {"limits": {"embedding_input": null}}}
        }))
        .unwrap();
        let resolved = resolve("gemini", "gemini-embedding-2", None, Some(&declared)).unwrap();
        assert!(resolved.profile.limits.embedding_input.is_none());
        let conflicts: Vec<_> = resolved
            .conflicts
            .iter()
            .filter(|conflict| {
                conflict.path.0.last()
                    == Some(&CapabilityPathSegment::Field("embedding_input".into()))
            })
            .collect();
        assert_eq!(conflicts.len(), 2);
        assert_eq!(
            conflicts[0].preferred,
            serde_json::json!({"max_images_per_document": 2})
        );
        assert_eq!(
            conflicts[0].overridden,
            serde_json::json!({"max_images_per_document": 6, "image_mime_types": ["image/png", "image/jpeg"]})
        );
        assert_eq!(conflicts[1].preferred, Value::Null);
        assert_eq!(
            conflicts[1].overridden,
            serde_json::json!({"max_images_per_document": 2})
        );
    }

    #[test]
    fn resolution_tracks_precedence_and_declaration_conflicts() {
        let discovered: CapabilityFactPatch = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "features": {"tool_calling": "supported"}
        }))
        .unwrap();
        let declared: CapabilityPatch = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "model_defaults": {
                "features": {"tool_calling": "unsupported"},
                "modalities": {"input": {"image": "unknown"}}
            }
        }))
        .unwrap();

        let resolved = resolve(
            "openai-compatible",
            "custom-model",
            Some(&discovered),
            Some(&declared),
        )
        .unwrap();

        assert_eq!(
            resolved.profile.features[&ModelFeature::ToolCalling],
            Support::Unsupported
        );
        assert!(resolved
            .evidence
            .iter()
            .any(|evidence| evidence.source == CapabilitySource::ConnectionDeclaration));
        assert!(!resolved.conflicts.is_empty());
    }

    #[test]
    fn support_and_parameter_intersections_follow_the_v1_contract() {
        for (left, right, expected) in [
            (Support::Supported, Support::Supported, Support::Supported),
            (Support::Supported, Support::Unknown, Support::Unknown),
            (
                Support::Supported,
                Support::Unsupported,
                Support::Unsupported,
            ),
            (Support::Unknown, Support::Unknown, Support::Unknown),
            (Support::Unknown, Support::Unsupported, Support::Unsupported),
            (
                Support::Unsupported,
                Support::Supported,
                Support::Unsupported,
            ),
        ] {
            assert_eq!(intersect_support(left, right), expected);
        }

        let values = intersect_constraint(
            &ParameterConstraint::StringEnum {
                values: vec!["square".to_string(), "wide".to_string()],
            },
            &ParameterConstraint::StringEnum {
                values: vec!["wide".to_string(), "tall".to_string()],
            },
        );
        assert_eq!(
            values,
            Some(ParameterConstraint::StringEnum {
                values: vec!["wide".to_string()]
            })
        );
        assert_eq!(
            intersect_constraint(
                &ParameterConstraint::IntegerRange { min: 1, max: 10 },
                &ParameterConstraint::IntegerRange { min: 3, max: 4 }
            ),
            Some(ParameterConstraint::IntegerRange { min: 3, max: 4 })
        );

        let known = ParameterCapability {
            support: Support::Supported,
            constraint: Some(ParameterConstraint::StringEnum {
                values: vec!["wide".to_string()],
            }),
        };
        let unconstrained = ParameterCapability {
            support: Support::Supported,
            constraint: None,
        };
        for (left, right) in [(&known, &unconstrained), (&unconstrained, &known)] {
            assert_eq!(
                intersect_parameter_capability(Some(left), Some(right)),
                known
            );
        }
        assert_eq!(
            intersect_parameter_capability(Some(&unconstrained), Some(&unconstrained)),
            ParameterCapability {
                support: Support::Supported,
                constraint: Some(ParameterConstraint::Unknown),
            }
        );

        let disjoint = ParameterCapability {
            support: Support::Supported,
            constraint: Some(ParameterConstraint::StringEnum {
                values: vec!["square".to_string()],
            }),
        };
        assert_eq!(
            intersect_parameter_capability(Some(&known), Some(&disjoint)),
            ParameterCapability {
                support: Support::Unsupported,
                constraint: None,
            }
        );
    }

    #[test]
    fn effective_image_models_include_the_generation_interface() {
        for (provider, model) in [
            ("openai", "gpt-image-2"),
            ("gemini", "models/gemini-3-pro-image"),
        ] {
            let resolved = resolve(provider, model, None, None).unwrap();
            assert_eq!(
                resolved.profile.surfaces[&ModelSurface::ImageGeneration],
                Support::Supported
            );
            assert_eq!(
                resolved.profile.interfaces[&Interface::OpenaiImages].operations
                    [&InterfaceOperation::Generate]
                    .support,
                Support::Supported
            );
        }
    }

    #[test]
    fn imagen_excludes_interactions_while_gemini_image_models_keep_native_support() {
        for (model, native_support) in [
            ("imagen-4.0-generate-001", Support::Unsupported),
            ("models/imagen-4.0-generate-001", Support::Unsupported),
            ("models/imagen-3.0-generate-002", Support::Unsupported),
            ("models/gemini-2.5-flash-image", Support::Supported),
            ("models/gemini-3-pro-image", Support::Supported),
            ("models/gemini-3-pro-image-preview", Support::Supported),
            ("models/gemini-3.1-flash-image", Support::Supported),
            ("models/nano-banana-pro-preview", Support::Supported),
        ] {
            let resolved = resolve("gemini", model, None, None).unwrap();
            assert_eq!(
                resolved.profile.interfaces[&Interface::GeminiInteractions].operations
                    [&InterfaceOperation::Create]
                    .support,
                native_support,
                "{model}",
            );
            assert_eq!(
                resolved.profile.interfaces[&Interface::OpenaiImages].operations
                    [&InterfaceOperation::Generate]
                    .support,
                Support::Supported,
                "{model}",
            );
        }
    }

    #[test]
    fn catalog_reuses_discovery_only_for_the_same_connection_fingerprint() {
        let catalog = CapabilityCatalog::new();
        let discovered = CapabilityFactPatch {
            surfaces: BTreeMap::from([(ModelSurface::ImageGeneration, Support::Supported)]),
            modalities: ModalitiesPatch {
                input: BTreeMap::from([(InputModality::Text, Support::Supported)]),
                output: BTreeMap::from([(OutputModality::Image, Support::Supported)]),
            },
            ..Default::default()
        };
        catalog
            .resolve(
                "connection-v1",
                "openai-compatible",
                "private-image-model",
                None,
                Some(&discovered),
                None,
            )
            .unwrap();

        let stale = catalog
            .resolve(
                "connection-v1",
                "openai-compatible",
                "private-image-model",
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            stale.profile.surfaces[&ModelSurface::ImageGeneration],
            Support::Supported
        );

        let changed_connection = catalog
            .resolve(
                "connection-v2",
                "openai-compatible",
                "private-image-model",
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            changed_connection.profile.surfaces[&ModelSurface::ImageGeneration],
            Support::Unknown
        );
    }

    #[test]
    fn capability_discovery_cache_is_bounded_and_expires() {
        let catalog = CapabilityCatalog::with_config(2, Duration::from_secs(300));
        let discovered = CapabilityFactPatch {
            surfaces: BTreeMap::from([(ModelSurface::Chat, Support::Supported)]),
            ..Default::default()
        };
        for model in ["one", "two", "three"] {
            catalog
                .resolve(
                    "connection",
                    "openai-compatible",
                    model,
                    None,
                    Some(&discovered),
                    None,
                )
                .unwrap();
        }
        assert_eq!(catalog.discovery_len(), 2);

        for index in 0..100 {
            catalog
                .resolve(
                    "connection",
                    "openai-compatible",
                    &format!("caller-controlled-{index}"),
                    None,
                    None,
                    None,
                )
                .unwrap();
        }
        assert_eq!(catalog.discovery_len(), 2);

        let expiring = CapabilityCatalog::with_config(2, Duration::ZERO);
        expiring
            .resolve(
                "connection",
                "openai-compatible",
                "model",
                None,
                Some(&discovered),
                None,
            )
            .unwrap();
        let expired = expiring
            .resolve("connection", "openai-compatible", "model", None, None, None)
            .unwrap();
        assert_eq!(
            expired.profile.surfaces[&ModelSurface::Chat],
            Support::Unknown
        );
        assert_eq!(expiring.discovery_len(), 0);
    }

    #[test]
    fn bedrock_and_openrouter_delivery_is_model_and_transport_scoped() {
        let haiku = resolve("bedrock", "anthropic.claude-haiku-4-5", None, None).unwrap();
        assert_eq!(
            haiku
                .profile
                .operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
                .unwrap()
                .mode,
            DeliveryMode::Translated
        );

        let runtime = bedrock_runtime_patch();
        let runtime =
            resolve("bedrock", "anthropic.claude-sonnet-4", Some(&runtime), None).unwrap();
        assert_eq!(
            runtime
                .profile
                .operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
                .unwrap()
                .mode,
            DeliveryMode::Adapted
        );

        let anthropic = resolve("openrouter", "anthropic/claude-sonnet-4", None, None).unwrap();
        let openai = resolve("openrouter", "openai/gpt-5", None, None).unwrap();
        assert_eq!(
            anthropic
                .profile
                .operation(Protocol::AnthropicMessages, ProtocolOperation::Create)
                .unwrap()
                .mode,
            DeliveryMode::Native
        );
        assert_eq!(
            openai
                .profile
                .operation(Protocol::AnthropicMessages, ProtocolOperation::Create)
                .unwrap()
                .mode,
            DeliveryMode::Translated
        );
    }

    #[test]
    fn concrete_image_parameter_violations_are_rejected_locally() {
        let resolved = resolve("openai", "gpt-image-2", None, None).unwrap();
        let allowed = CapabilityRequirements::for_interface_operation(
            Interface::OpenaiImages,
            InterfaceOperation::Generate,
        )
        .surface(ModelSurface::ImageGeneration)
        .parameter("size", "1024x1024");
        assert!(resolved.profile.satisfies(&allowed));

        let rejected = CapabilityRequirements::for_interface_operation(
            Interface::OpenaiImages,
            InterfaceOperation::Generate,
        )
        .surface(ModelSurface::ImageGeneration)
        .parameter("size", "1792x1024");
        assert_eq!(
            resolved.profile.first_failure(&rejected).as_deref(),
            Some("parameter 'size' value \"1792x1024\" is unsupported")
        );
    }

    #[test]
    fn image_parameter_constraints_are_model_scoped() {
        for (provider, model, parameter, value) in [
            ("openai", "dall-e-3", "size", "1792x1024"),
            ("openai", "dall-e-3", "quality", "hd"),
            ("openai", "dall-e-2", "size", "512x512"),
            ("openai", "gpt-image-1", "size", "auto"),
            ("gemini", "models/gemini-3-pro-image", "size", "1024x1024"),
        ] {
            let resolved = resolve(provider, model, None, None).unwrap();
            let requirements = CapabilityRequirements::for_interface_operation(
                Interface::OpenaiImages,
                InterfaceOperation::Generate,
            )
            .surface(ModelSurface::ImageGeneration)
            .parameter(parameter, value);
            assert!(
                resolved.profile.satisfies(&requirements),
                "{provider}/{model} should accept {parameter}={value}"
            );
        }
    }

    #[test]
    fn openrouter_known_namespaces_have_cold_chat_and_responses_support() {
        for model in [
            "google/gemini-2.5-pro",
            "x-ai/grok-4",
            "mistralai/mistral-large",
            "meta-llama/llama-4-maverick",
            "deepseek/deepseek-v3",
        ] {
            let resolved = resolve("openrouter", model, None, None).unwrap();
            assert_eq!(
                resolved.profile.surfaces[&ModelSurface::Chat],
                Support::Supported
            );
            let responses = resolved
                .profile
                .operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
                .unwrap();
            assert_eq!(responses.support, Support::Supported);
            assert_eq!(responses.mode, DeliveryMode::Native);
        }

        let vllm = resolve("vllm", "private-model", None, None).unwrap();
        assert_eq!(vllm.profile.surfaces[&ModelSurface::Chat], Support::Unknown);
    }

    #[test]
    fn declaration_v1_is_strict_and_scoped() {
        let fixture = include_str!("../tests/fixtures/capabilities/declaration-v1.json");
        let declaration: CapabilityPatch = serde_json::from_str(fixture).unwrap();
        declaration.validate().unwrap();
        assert!(declaration.models.contains_key("gpt-image-2"));

        let old_shape = r#"{"schema_version":1,"features":{"tool_calling":"supported"}}"#;
        assert!(serde_json::from_str::<CapabilityPatch>(old_shape).is_err());
        let unknown = r#"{"schema_version":1,"model_defaults":{},"typo":{}}"#;
        assert!(serde_json::from_str::<CapabilityPatch>(unknown).is_err());
    }

    #[test]
    fn model_conflicts_record_only_disagreeing_declared_leaves() {
        let mut inherited = CapabilityProfile::default();
        inherited
            .surfaces
            .insert(ModelSurface::Chat, Support::Supported);
        inherited
            .surfaces
            .insert(ModelSurface::Embeddings, Support::Unknown);
        inherited
            .modalities
            .input
            .insert(InputModality::Text, Support::Supported);
        inherited.limits.context_window = Some(128_000);
        let declared = ModelCapabilityPatch {
            surfaces: BTreeMap::from([
                (ModelSurface::Chat, Support::Supported),
                (ModelSurface::Embeddings, Support::Unsupported),
            ]),
            modalities: ModalitiesPatch {
                input: BTreeMap::from([(InputModality::Text, Support::Supported)]),
                ..Default::default()
            },
            limits: ModelLimitsPatch {
                context_window: Some(None),
                ..Default::default()
            },
            ..Default::default()
        };

        let mut conflicts = Vec::new();
        record_model_conflicts(
            &inherited,
            &declared,
            CapabilityPath::fields(&["model_defaults"]),
            &mut conflicts,
        );

        assert_eq!(conflicts.len(), 2);
        assert_eq!(
            conflicts[0].path,
            CapabilityPath(vec![
                CapabilityPathSegment::Field("model_defaults".to_string()),
                CapabilityPathSegment::Field("surfaces".to_string()),
                CapabilityPathSegment::Key("embeddings".to_string()),
            ])
        );
        assert_eq!(conflicts[0].preferred, serde_json::json!("unsupported"));
        assert_eq!(conflicts[0].overridden, serde_json::json!("unknown"));
        assert_eq!(
            conflicts[1].path,
            CapabilityPath::fields(&["model_defaults", "limits", "context_window"])
        );
        assert_eq!(conflicts[1].preferred, Value::Null);
        assert_eq!(conflicts[1].overridden, serde_json::json!(128_000));
    }

    #[test]
    fn effective_image_profile_matches_the_v1_golden_fixture() {
        let actual = serde_json::to_value(
            &resolve("openai", "gpt-image-2", None, None)
                .unwrap()
                .profile,
        )
        .unwrap();
        let expected: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/capabilities/effective-image-v1.json"
        ))
        .unwrap();
        assert_eq!(
            actual,
            expected,
            "effective image fixture drifted:\n{}",
            serde_json::to_string_pretty(&actual).unwrap()
        );
    }

    #[test]
    fn vscode_projection_requires_model_and_responses_endpoint_support() {
        let mut profile = openai_capabilities("gpt-5");
        profile
            .features
            .insert(ModelFeature::ToolCalling, Support::Supported);
        profile
            .modalities
            .input
            .insert(InputModality::Image, Support::Supported);
        let create = profile
            .interfaces
            .get_mut(&Interface::OpenaiResponses)
            .and_then(|interface| interface.operations.get_mut(&InterfaceOperation::Create))
            .expect("create profile");
        create
            .features
            .insert(EndpointFeature::ToolCalling, Support::Supported);
        create
            .features
            .insert(EndpointFeature::ImageInput, Support::Supported);
        let projected = ClientCapabilityProjection::vscode(&profile);
        assert!(projected.tool_calling);
        assert!(projected.vision);

        profile
            .modalities
            .input
            .insert(InputModality::Image, Support::Unknown);
        let projected = ClientCapabilityProjection::vscode(&profile);
        assert!(projected.tool_calling);
        assert!(!projected.vision);
    }

    #[test]
    fn playground_requires_text_in_text_out_and_a_streaming_endpoint() {
        let mut profile = resolve("openai", "gpt-4o-mini", None, None)
            .unwrap()
            .profile;
        // Another supported surface does not invalidate a usable text route.
        profile
            .surfaces
            .insert(ModelSurface::ImageGeneration, Support::Supported);
        assert_eq!(
            ClientCapabilityProjection::playground(&profile).chat_protocol,
            Some(Protocol::OpenaiResponses)
        );
        for support in [Support::Unknown, Support::Unsupported] {
            let mut input_missing = profile.clone();
            input_missing
                .modalities
                .input
                .insert(InputModality::Text, support);
            assert_eq!(
                ClientCapabilityProjection::playground(&input_missing).chat_protocol,
                None
            );
            assert_eq!(
                ClientCapabilityProjection::playground(&input_missing).chat_support,
                support
            );
            let mut output_missing = profile.clone();
            output_missing
                .modalities
                .output
                .insert(OutputModality::Text, support);
            assert_eq!(
                ClientCapabilityProjection::playground(&output_missing).chat_protocol,
                None
            );
            assert_eq!(
                ClientCapabilityProjection::playground(&output_missing).chat_support,
                support
            );
        }
        profile
            .interfaces
            .get_mut(&Interface::OpenaiResponses)
            .unwrap()
            .operations
            .get_mut(&InterfaceOperation::Create)
            .unwrap()
            .features
            .insert(EndpointFeature::Streaming, Support::Unsupported);
        assert_eq!(
            ClientCapabilityProjection::playground(&profile).chat_protocol,
            Some(Protocol::OpenaiChatCompletions)
        );
        profile
            .interfaces
            .get_mut(&Interface::OpenaiChatCompletions)
            .unwrap()
            .operations
            .get_mut(&InterfaceOperation::Create)
            .unwrap()
            .support = Support::Unknown;
        assert_eq!(
            ClientCapabilityProjection::playground(&profile).chat_protocol,
            None
        );
    }

    #[test]
    fn openai_stateful_responses_operations_are_profiled_independently() {
        let profile = openai_capabilities("gpt-5");
        for operation in [
            ProtocolOperation::Retrieve,
            ProtocolOperation::Delete,
            ProtocolOperation::Compact,
            ProtocolOperation::Cancel,
        ] {
            assert_eq!(
                profile
                    .operation(Protocol::OpenaiResponses, operation)
                    .expect("operation profile")
                    .mode,
                DeliveryMode::Native
            );
        }
        let openrouter = native_responses_and_messages_capabilities("model");
        assert!(openrouter
            .operation(Protocol::OpenaiResponses, ProtocolOperation::Retrieve)
            .is_none());
    }

    #[test]
    fn anthropic_ingress_profiles_distinguish_native_adapted_and_translated() {
        let operation = |profile: &CapabilityProfile, operation| {
            profile
                .operation(Protocol::AnthropicMessages, operation)
                .unwrap()
                .mode
        };
        let bedrock_openai_dialect = bedrock_capabilities("new-provider.future-model");
        assert_eq!(
            operation(&bedrock_openai_dialect, ProtocolOperation::Create),
            DeliveryMode::Translated
        );
        assert_eq!(
            operation(&bedrock_openai_dialect, ProtocolOperation::CountTokens),
            DeliveryMode::Unsupported
        );

        let adapted = vertex_capabilities("anthropic/claude");
        assert_eq!(
            operation(&adapted, ProtocolOperation::Create),
            DeliveryMode::Adapted
        );
        assert_eq!(
            operation(&adapted, ProtocolOperation::CountTokens),
            DeliveryMode::Adapted
        );

        let translated = translated_generation_capabilities("model");
        assert_eq!(
            operation(&translated, ProtocolOperation::Create),
            DeliveryMode::Translated
        );
        assert_eq!(
            operation(&translated, ProtocolOperation::CountTokens),
            DeliveryMode::Unsupported
        );
    }

    #[test]
    fn bedrock_chooses_a_transport_by_dialect_not_model_allowlist() {
        let anthropic = bedrock_capabilities("us.anthropic.claude-sonnet-5");
        assert_eq!(
            anthropic
                .operation(Protocol::OpenaiChatCompletions, ProtocolOperation::Create)
                .expect("Chat Completions create")
                .mode,
            DeliveryMode::Adapted
        );
        assert_eq!(
            anthropic
                .operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
                .expect("Responses create")
                .mode,
            DeliveryMode::Translated
        );
        assert_eq!(
            anthropic
                .operation(Protocol::AnthropicMessages, ProtocolOperation::Create)
                .expect("Messages create")
                .mode,
            DeliveryMode::Native
        );
        assert_eq!(
            bedrock_capabilities("new-provider.future-model")
                .operation(Protocol::OpenaiChatCompletions, ProtocolOperation::Create)
                .expect("Chat Completions create")
                .mode,
            DeliveryMode::Native
        );
        assert_eq!(
            bedrock_capabilities("new-provider.future-model")
                .operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
                .expect("Responses create")
                .mode,
            DeliveryMode::Translated
        );
    }
}
