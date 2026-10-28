use axum::{http::StatusCode, Json};
use serde::{Deserialize, Serialize};

// Common types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResponse<T> {
    // OpenRouter's catalog omits `object` on the envelope and on every model.
    #[serde(default = "list_object")]
    pub object: String,
    pub data: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_id: Option<String>,
    #[serde(default)]
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: Error,
}

impl ErrorResponse {
    /// Build the `(StatusCode, Json<ErrorResponse>)` tuple handlers return for
    /// fallible paths that don't go through `ServiceError`/`BackendError`
    /// (whose own `into_http`/`backend_error_response` cover that shape).
    /// `kind` fills the OpenAI-style `error.type` field; `param` and `code`
    /// are left unset, matching every inline call site this replaces.
    pub fn new(
        status: StatusCode,
        kind: impl Into<String>,
        message: impl Into<String>,
    ) -> (StatusCode, Json<ErrorResponse>) {
        (
            status,
            Json(ErrorResponse {
                error: Error {
                    message: message.into(),
                    error_type: kind.into(),
                    param: None,
                    code: None,
                },
            }),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Error {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
    pub param: Option<String>,
    pub code: Option<String>,
}

// Assistant types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assistant {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub name: Option<String>,
    pub description: Option<String>,
    pub model: String,
    pub instructions: Option<String>,
    pub tools: Vec<AssistantTool>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AssistantTool {
    #[serde(rename = "code_interpreter")]
    CodeInterpreter,
    #[serde(rename = "file_search")]
    FileSearch {
        file_search: Option<serde_json::Value>,
    },
    #[serde(rename = "function")]
    Function { function: FunctionDefinition },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAssistantRequest {
    pub model: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub instructions: Option<String>,
    pub tools: Option<Vec<AssistantTool>>,
    pub metadata: Option<serde_json::Value>,
}

// Chat types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    #[serde(default, skip_serializing)]
    pub fallbacks: crate::route_grid::FallbackMap, // per-request fallback override
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<crate::schema_json::SchemaJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Requested reasoning effort; interpretation depends on the upstream provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Options for streamed chat-completion responses.
///
/// Mirrors OpenAI's `stream_options` request field: when `include_usage` is
/// set, the terminal streamed chunk carries a token-usage summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<crate::schema_json::SchemaJson>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatChoice {
    pub index: i32,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<i32>,
    pub total_tokens: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokenDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokenDetails>,
    /// Observation-only billing metadata; never added to the provider wire format.
    #[serde(skip)]
    pub input_token_attribution: crate::pricing::InputTokenAttribution,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptTokenDetails {
    #[serde(default)]
    pub cached_tokens: i32,
    #[serde(
        default,
        alias = "cache_creation_tokens",
        alias = "cache_write_input_tokens"
    )]
    pub cache_write_tokens: i32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompletionTokenDetails {
    #[serde(default)]
    pub reasoning_tokens: i32,
}

fn list_object() -> String {
    "list".to_string()
}

fn model_object() -> String {
    "model".to_string()
}

// Model types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(default = "model_object")]
    pub object: String,
    // Some OpenAI-compatible providers (e.g. Gemini's /openai/models, vLLM) omit
    // these fields. Default them so deserialization doesn't drop the whole listing.
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub owned_by: String,
    /// Chalk's additive, effective capability profile. Upstream model objects
    /// omit this field; the router attaches it after resolving the public id.
    #[serde(
        default,
        skip_deserializing,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_model_capabilities"
    )]
    pub capabilities: Option<crate::capabilities::CapabilityProfile>,
    /// Provider-native catalog fields retained only long enough to map
    /// authoritative discovery facts. They are never echoed by Chalk's model
    /// APIs and are never deserialized directly into the public schema.
    #[serde(default, flatten, skip_serializing)]
    pub provider_metadata: std::collections::BTreeMap<String, serde_json::Value>,
}

fn serialize_model_capabilities<S: serde::Serializer>(
    profile: &Option<crate::capabilities::CapabilityProfile>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct PublishedCapabilities<'a> {
        #[serde(flatten)]
        profile: &'a crate::capabilities::CapabilityProfile,
        playground: crate::capabilities::PlaygroundCapabilityProjection,
    }
    profile
        .as_ref()
        .map(|profile| PublishedCapabilities {
            profile,
            playground: crate::capabilities::ClientCapabilityProjection::playground(profile),
        })
        .serialize(serializer)
}

#[cfg(test)]
mod model_tests {
    use super::Model;

    #[test]
    fn model_serialization_publishes_effective_capabilities_only() {
        let mut model = Model {
            id: "openai/gpt-image-2".to_string(),
            object: "model".to_string(),
            created: 0,
            owned_by: "openai".to_string(),
            capabilities: Some(
                crate::capabilities::resolve("openai", "gpt-image-2", None, None)
                    .unwrap()
                    .profile,
            ),
            provider_metadata: std::collections::BTreeMap::from([(
                "supported_parameters".to_string(),
                serde_json::json!(["tools"]),
            )]),
        };
        let json = serde_json::to_value(&model).unwrap();
        assert_eq!(
            json["capabilities"]["interfaces"]["openai_images"]["operations"]["generate"]
                ["support"],
            "supported"
        );
        assert!(json.get("supported_parameters").is_none());
        assert!(json["capabilities"]["playground"]["chat_protocol"].is_null());
        assert!(
            json["capabilities"]["interfaces"]["openai_images"]["operations"]["generate"]
                .get("mode")
                .is_none()
        );

        model.capabilities = None;
        let json = serde_json::to_value(model).unwrap();
        assert!(json.get("capabilities").is_none());
    }
}

// Embedding types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateEmbeddingRequest {
    pub model: String,
    pub input: EmbeddingInput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[cfg(test)]
mod embedding_tests {
    use super::CreateEmbeddingRequest;
    use serde_json::json;

    #[test]
    fn embedding_request_omits_absent_options_and_preserves_supplied_values() {
        let minimal = json!({"model": "gemini-embedding-001", "input": "hello"});
        for body in [
            minimal.clone(),
            json!({"model": "gemini-embedding-001", "input": "hello",
                "encoding_format": null, "dimensions": null, "user": null}),
        ] {
            let request: CreateEmbeddingRequest = serde_json::from_value(body).unwrap();
            assert_eq!(serde_json::to_value(request).unwrap(), minimal);
        }

        let supplied = json!({"model": "text-embedding-3-small", "input": ["hello"],
            "encoding_format": "base64", "dimensions": 256, "user": "test-user"});
        let request: CreateEmbeddingRequest = serde_json::from_value(supplied.clone()).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), supplied);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    Single(String),
    Multiple(Vec<String>),
    /// Model token IDs are already tokenized; preserve them unchanged.
    Tokens(Vec<u32>),
    TokenBatch(Vec<Vec<u32>>),
    /// One document combining text and image parts into a single embedding.
    Multimodal(Vec<EmbeddingInputItem>),
}

impl EmbeddingInput {
    pub fn token_count(&self) -> Option<u64> {
        match self {
            Self::Tokens(tokens) => Some(tokens.len() as u64),
            Self::TokenBatch(inputs) => Some(inputs.iter().map(|tokens| tokens.len() as u64).sum()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EmbeddingInputItem {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateEmbeddingResponse {
    pub object: String,
    #[serde(deserialize_with = "deserialize_embeddings")]
    pub data: Vec<Embedding>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

// Gemini's OpenAI-compatible API can omit embedding indexes.
// Keep supplied indexes when provided and use response order for entries without one.
fn deserialize_embeddings<'de, D>(deserializer: D) -> Result<Vec<Embedding>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Entry {
        object: String,
        index: Option<i32>,
        embedding: EmbeddingData,
    }

    Vec::<Entry>::deserialize(deserializer)?
        .into_iter()
        .enumerate()
        .map(|(position, entry)| {
            Ok(Embedding {
                object: entry.object,
                index: match entry.index {
                    Some(index) => index,
                    None => i32::try_from(position).map_err(serde::de::Error::custom)?,
                },
                embedding: entry.embedding,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Embedding {
    pub object: String,
    pub index: i32,
    pub embedding: EmbeddingData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingData {
    Float(Vec<f32>),
    Base64(String),
}

// Image types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateImageRequest {
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// `background` for the OpenAI gpt-image family ("auto"/"transparent"/
    /// "opaque"). Forwarded as-is to OpenAI's images endpoint; ignored by
    /// providers that don't accept it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    /// `output_format` for the OpenAI gpt-image family ("png"/"jpeg"/"webp").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_format: Option<String>,
    /// Provider-specific aspect ratio, e.g. "1:1" or "16:9". Not part of the
    /// OpenAI images API; Google's compatibility endpoint uses `size` instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    /// Provider-specific resolution tier. Google's OpenAI-compatible images
    /// endpoint rejects this top-level field; use `size` instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageResponse {
    // Google's OpenAI-compatible images response may omit `created`; default to
    // 0 rather than failing to deserialize the whole response.
    #[serde(default)]
    pub created: i64,
    pub data: Vec<Image>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub url: Option<String>,
    pub b64_json: Option<String>,
    pub revised_prompt: Option<String>,
}

// Audio types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSpeechRequest {
    pub model: String,
    pub input: String,
    pub voice: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionResponse {
    pub text: String,
}

// File types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct File {
    pub id: String,
    pub object: String,
    pub bytes: i64,
    pub created_at: i64,
    pub filename: String,
    pub purpose: String,
}

// Fine-tuning types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FineTuningJob {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    pub model: String,
    pub fine_tuned_model: Option<String>,
    pub organization_id: String,
    pub status: String,
    pub training_file: String,
    pub validation_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFineTuningJobRequest {
    pub model: String,
    pub training_file: String,
    pub validation_file: Option<String>,
    pub hyperparameters: Option<serde_json::Value>,
    pub suffix: Option<String>,
}

// Moderation types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateModerationRequest {
    pub input: ModerationInput,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ModerationInput {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateModerationResponse {
    pub id: String,
    pub model: String,
    pub results: Vec<ModerationResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModerationResult {
    pub flagged: bool,
    pub categories: ModerationCategories,
    pub category_scores: ModerationCategoryScores,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModerationCategories {
    pub sexual: bool,
    pub hate: bool,
    pub harassment: bool,
    pub violence: bool,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModerationCategoryScores {
    pub sexual: f32,
    pub hate: f32,
    pub harassment: f32,
    pub violence: f32,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, f32>,
}

// Batch types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Batch {
    pub id: String,
    pub object: String,
    pub endpoint: String,
    pub input_file_id: String,
    pub completion_window: String,
    pub status: String,
    pub created_at: i64,
    pub completed_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateBatchRequest {
    pub input_file_id: String,
    pub endpoint: String,
    pub completion_window: String,
    pub metadata: Option<serde_json::Value>,
}

// Streaming types for SSE
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatCompletionChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunkChoice {
    pub index: i32,
    pub delta: ChatCompletionDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub index: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub tool_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionCallDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCallDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}
