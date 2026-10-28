//! Wire types and lossless request envelopes for OpenAI's Responses API.
//! Chat compatibility conversion lives in [`crate::translation::responses_chat`].
//!
//! The Responses API evolves more quickly than the chat-completions API.  The
//! types in this module therefore model fields used for routing, translation,
//! and usage accounting, while retaining all unknown fields in flattened maps.
//! Native Responses backends can deserialize and reserialize a request without
//! dropping fields that this router does not understand yet.

use std::sync::{Arc, OnceLock};

use crate::schema_json::{SchemaJson, SchemaObject};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use crate::translation::responses_chat::TranslationError;
use crate::types::{ChatCompletionRequest, ChatCompletionResponse};

/// A `POST /v1/responses` request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateResponseRequest {
    pub model: String,
    pub input: ResponseInput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponseTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<ResponseTextConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl CreateResponseRequest {
    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
    }

    /// Whether the caller requested an SSE response.  An omitted `stream`
    /// field has the API default of `false`.
    pub fn stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }

    pub fn set_stream(&mut self, stream: bool) {
        self.stream = Some(stream);
    }
}

/// Lossless inbound request envelope.
///
/// Native providers forward the original JSON object after validating only the
/// routing fields the router needs. The stricter compatibility codec is parsed
/// lazily when an OpenAI-compatible backend must translate the request to chat
/// completions. This keeps native forwarding resilient to new Responses fields
/// and preserves explicit nulls and exact JSON number values.
#[derive(Debug, Clone)]
pub struct ResponsesRequest {
    background_accounting_id: Option<uuid::Uuid>,
    raw: SchemaObject,
    model: String,
    stream: bool,
    compatibility: Arc<OnceLock<Result<CompatibilityRequest, TranslationError>>>,
}

#[derive(Debug, Clone)]
struct CompatibilityRequest {
    request: CreateResponseRequest,
    chat: ChatCompletionRequest,
}

impl PartialEq for ResponsesRequest {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw && self.model == other.model && self.stream == other.stream
    }
}

impl ResponsesRequest {
    pub(crate) fn background_accounting_id(&self) -> Option<uuid::Uuid> {
        self.background_accounting_id
    }
    pub(crate) fn set_background_accounting_id(&mut self, event_id: Option<uuid::Uuid>) {
        self.background_accounting_id = event_id;
    }
    pub fn raw(&self) -> &Value {
        self.raw.value()
    }

    pub(crate) fn retained(&self) -> SchemaJson {
        self.raw.retained()
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn set_model(&mut self, model: impl Into<String>) {
        let model = model.into();
        self.update_field("model", Value::String(model.clone()));
        self.model = model;
    }

    pub fn stream(&self) -> bool {
        self.stream
    }

    /// Whether the provider should continue executing after the create call
    /// returns. Chalk treats this as provider-owned lifecycle state.
    pub fn background(&self) -> bool {
        self.raw
            .field("background")
            .and_then(|value| value.as_bool())
            == Some(true)
    }

    pub fn set_stream(&mut self, stream: bool) {
        self.update_field("stream", Value::Bool(stream));
        self.stream = stream;
    }

    pub fn previous_response_id(&self) -> Option<&str> {
        self.raw
            .field("previous_response_id")
            .and_then(|value| value.as_str())
    }

    pub fn user(&self) -> Result<Option<&str>, &'static str> {
        match self.raw.field("user").map(SchemaJson::value) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(user)) => Ok(Some(user)),
            Some(_) => Err("Responses user must be a string or null"),
        }
    }

    pub fn set_previous_response_id(&mut self, response_id: impl Into<String>) {
        self.update_field("previous_response_id", Value::String(response_id.into()));
    }

    fn update_field(&mut self, field: &str, value: Value) {
        self.raw.set_member(field, value.into());
        // Model and stream are projected onto the shared translation below.
        // Semantic changes must invalidate both cached successes and failures.
        if !matches!(field, "model" | "stream") {
            self.compatibility = Arc::new(OnceLock::new());
        }
    }

    pub fn to_chat_completion(&self) -> Result<ChatCompletionRequest, TranslationError> {
        let mut chat = self.compatibility()?.chat.clone();
        chat.model.clone_from(&self.model);
        chat.stream = self.raw.field("stream").and_then(|value| value.as_bool());
        Ok(chat)
    }

    /// Build the canonical Responses-shaped result for a request translated to
    /// an OpenAI-compatible chat-completions backend.
    pub fn to_compatibility_response(
        &self,
        response: ChatCompletionResponse,
    ) -> Result<CreateResponseResponse, TranslationError> {
        CreateResponseResponse::from_chat_completion_for_request(
            response,
            &self.compatibility()?.request,
        )
    }

    fn compatibility(&self) -> Result<&CompatibilityRequest, TranslationError> {
        match self.compatibility.get_or_init(|| {
            let encoded = serde_json::to_vec(&self.raw)
                .map_err(|error| TranslationError::invalid("request", error.to_string()))?;
            let request: CreateResponseRequest =
                serde_json::from_slice(&encoded).map_err(|error| {
                    TranslationError::invalid(
                        "request",
                        format!("invalid Responses request: {error}"),
                    )
                })?;
            let chat = request.to_chat_completion()?;
            Ok(CompatibilityRequest { request, chat })
        }) {
            Ok(compatibility) => Ok(compatibility),
            Err(error) => Err(error.clone()),
        }
    }
}

impl<'de> Deserialize<'de> for ResponsesRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = SchemaJson::deserialize(deserializer)?;
        let object = raw
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("Responses request must be a JSON object"))?;

        let model = object
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                serde::de::Error::custom("Responses request field `model` must be a string")
            })?
            .to_string();
        let stream = match object.get("stream") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(stream)) => *stream,
            Some(_) => {
                return Err(serde::de::Error::custom(
                    "Responses request field `stream` must be a boolean when present",
                ));
            }
        };
        Ok(Self {
            background_accounting_id: None,
            raw: raw.into_object().map_err(serde::de::Error::custom)?,
            model,
            stream,
            compatibility: Arc::new(OnceLock::new()),
        })
    }
}

impl Serialize for ResponsesRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.raw.serialize(serializer)
    }
}

/// A Responses request accepts either a shorthand user string or a list of
/// fully-typed input items.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ResponseInput {
    Text(String),
    Items(Vec<ResponseItem>),
}

/// A forward-compatible Responses input or output item.
///
/// Keeping the discriminant optional also supports the API's "easy input
/// message" shape, where `{role, content}` is accepted without `type: message`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResponseItem {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub item_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ResponseContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ResponseContent {
    Text(String),
    Parts(Vec<ResponseContentPart>),
}

/// A forward-compatible Responses message content part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResponseContentPart {
    #[serde(rename = "type")]
    pub part_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Responses tools use a flat function definition rather than chat's nested
/// `{type, function: {...}}` shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResponseTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<SchemaJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResponseTextConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<SchemaJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A completed (or incomplete/failed) Responses API object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateResponseResponse {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub status: String,
    pub model: String,
    #[serde(default)]
    pub output: Vec<ResponseItem>,
    pub error: Option<Value>,
    pub incomplete_details: Option<Value>,
    pub instructions: Option<Value>,
    pub max_output_tokens: Option<i32>,
    pub parallel_tool_calls: Option<bool>,
    pub previous_response_id: Option<String>,
    pub reasoning: Option<Value>,
    pub store: Option<bool>,
    pub temperature: Option<f32>,
    pub text: Option<ResponseTextConfig>,
    pub tool_choice: Option<Value>,
    pub tools: Option<Vec<ResponseTool>>,
    pub top_p: Option<f32>,
    pub truncation: Option<String>,
    pub usage: Option<ResponseUsage>,
    pub metadata: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub type ResponsesResponse = CreateResponseResponse;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResponseUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub input_tokens_details: ResponseInputTokenDetails,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub output_tokens_details: ResponseOutputTokenDetails,
    #[serde(default)]
    pub total_tokens: i64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResponseInputTokenDetails {
    #[serde(default)]
    pub cached_tokens: i64,
    #[serde(
        default,
        alias = "cache_creation_tokens",
        alias = "cache_write_input_tokens"
    )]
    pub cache_write_tokens: i64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResponseOutputTokenDetails {
    #[serde(default)]
    pub reasoning_tokens: i64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatChoice, ChatMessage, Usage};
    use serde_json::json;

    fn completed_chat_response() -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "chatcmpl_123".to_string(),
            object: "chat.completion".to_string(),
            created: 1234,
            model: "fallback-model".to_string(),
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: Some(Value::String("done".to_string())),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: Some(4),
                total_tokens: 14,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn non_object_requests_keep_the_public_error_message() {
        for raw in ["[]", "null", "false", "42", r#""text""#] {
            let error = serde_json::from_str::<ResponsesRequest>(raw).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Responses request must be a JSON object"),
                "{error}"
            );
        }
    }

    #[test]
    fn mutations_invalidate_warm_compatibility_and_leave_clones_intact() {
        for warm in [false, true] {
            let mut request: ResponsesRequest = serde_json::from_str(r#"{"model":"public","input":"hi","text":{"format":{"type":"json_schema","name":"ordered","schema":{"type":"object","properties":{"z":{},"a":{}}}}}}"#).unwrap();
            if warm {
                request.to_chat_completion().unwrap();
            }
            let original = request.clone();
            request.set_model("upstream");
            request.set_stream(true);
            assert_eq!(request.raw()["model"], "upstream");
            assert_eq!(request.to_chat_completion().unwrap().model, "upstream");
            assert_eq!(request.to_chat_completion().unwrap().stream, Some(true));
            assert_eq!(original.to_chat_completion().unwrap().model, "public");
            request.set_previous_response_id("resp_previous");
            assert_eq!(request.raw()["previous_response_id"], "resp_previous");
            // This field makes chat translation unsupported: an old cached
            // success must not survive the mutation.
            assert!(request.to_chat_completion().is_err());
            assert!(serde_json::to_string(&request)
                .unwrap()
                .contains(r#""properties":{"z":{},"a":{}}"#));
        }
    }

    #[test]
    fn unknown_fields_round_trip_without_loss() {
        let value = json!({
            "model": "gpt-5-codex",
            "stream": true,
            "input": [{
                "type": "reasoning",
                "id": "rs_123",
                "encrypted_content": "opaque",
                "new_item_field": {"nested": true}
            }],
            "reasoning": {"effort": "high", "summary": "auto"},
            "include": ["reasoning.encrypted_content"],
            "new_request_field": 42
        });

        let request: CreateResponseRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(request.model(), "gpt-5-codex");
        assert!(request.stream());
        assert_eq!(request.extra["new_request_field"], json!(42));
        assert_eq!(serde_json::to_value(request).unwrap(), value);
    }

    #[test]
    fn setters_change_only_routing_fields() {
        let mut request: CreateResponseRequest = serde_json::from_value(json!({
            "model": "requested",
            "input": "hello",
            "custom": "preserved"
        }))
        .unwrap();

        request.set_model("resolved");
        request.set_stream(true);

        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["model"], "resolved");
        assert_eq!(value["stream"], true);
        assert_eq!(value["custom"], "preserved");
    }

    #[test]
    fn unedited_request_forwards_compact_last_value_wins_json() {
        let request: ResponsesRequest = serde_json::from_str(
            r#"{
            "model":"a", "m\u006fdel":"b", "input":"hello",
            "future": { "z":0, "a":null, "z":1.2300e-4 }
        }"#,
        )
        .unwrap();
        assert_eq!(request.model(), "b");
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"model":"b","input":"hello","future":{"a":null,"z":1.2300e-4}}"#
        );
    }

    #[test]
    fn native_setters_preserve_schemas_nulls_and_exact_nested_numbers() {
        let mut request: ResponsesRequest = serde_json::from_str(r#"{"model":"public","stream":false,"input":"hi","text":{"format":{"type":"json_schema","name":"ordered","schema":{"properties":{"z":{"minimum":1.2300e-4},"a":{}}}}},"tools":[{"type":"function","name":"lookup","parameters":{"properties":{"yy":{},"bb":{}}}}],"future":{"z":9007199254740993,"a":null}}"#).unwrap();
        // Populate the cached compatibility view before authoritative mutations.
        assert!(request.to_chat_completion().is_err());
        request.set_model("upstream");
        request.set_stream(true);
        request.set_previous_response_id("resp_upstream");
        let wire = serde_json::to_string(&request).unwrap();
        assert!(wire.contains(r#""properties":{"z":{"minimum":1.2300e-4},"a":{}}"#));
        assert!(wire.contains(r#""properties":{"yy":{},"bb":{}}"#));
        assert!(wire.contains(r#""future":{"z":9007199254740993,"a":null}"#));
        assert!(wire.contains(r#""model":"upstream""#));
        assert!(wire.contains(r#""previous_response_id":"resp_upstream""#));
        assert!(wire.contains(r#""stream":true"#));
    }

    #[test]
    fn routed_request_preserves_native_json_details() {
        let value = json!({
            "model": "requested",
            "input": "hello",
            "instructions": [{"type": "input_text", "text": "developer guidance"}],
            "temperature": 0.12345678901234567,
            "metadata": null,
            "future": {"nullable": null}
        });
        let mut request: ResponsesRequest = serde_json::from_value(value.clone()).unwrap();

        assert_eq!(serde_json::to_value(&request).unwrap(), value);
        request.set_model("resolved");
        request.set_stream(true);
        let routed = serde_json::to_value(request).unwrap();
        assert_eq!(routed["model"], "resolved");
        assert_eq!(routed["stream"], true);
        assert_eq!(routed["temperature"], value["temperature"]);
        assert!(routed["metadata"].is_null());
        assert!(routed["future"]["nullable"].is_null());
    }

    #[test]
    fn native_request_validation_is_limited_to_routing_fields() {
        let value = json!({
            "model": "gpt-future",
            "input": {"future_native_shape": true},
            "metadata": null,
            "stream": null,
            "future_number": 0.12345678901234567
        });
        let request: ResponsesRequest = serde_json::from_value(value.clone()).unwrap();

        assert_eq!(request.model(), "gpt-future");
        assert!(!request.stream());
        assert_eq!(request.raw(), &value);
        assert_eq!(serde_json::to_value(&request).unwrap(), value);
        assert!(matches!(
            request.to_chat_completion(),
            Err(TranslationError::InvalidField { ref path, .. }) if path == "request"
        ));
    }

    #[test]
    fn response_user_is_an_optional_string_agent_dimension() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-future",
            "input": "hello",
            "user": "agent-1"
        }))
        .unwrap();
        assert_eq!(request.user().unwrap(), Some("agent-1"));

        let invalid: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-future",
            "input": "hello",
            "user": {"unexpected": true}
        }))
        .unwrap();
        assert!(invalid.user().is_err());
    }

    #[test]
    fn native_request_rejects_invalid_routing_fields() {
        for value in [
            json!([]),
            json!({"input": "hello"}),
            json!({"model": null, "input": "hello"}),
            json!({"model": "gpt", "input": "hello", "stream": "true"}),
        ] {
            assert!(serde_json::from_value::<ResponsesRequest>(value).is_err());
        }
    }

    #[test]
    fn cold_routing_clones_do_not_poison_each_others_stream_controls() {
        for initial_stream in [
            None,
            Some(Value::Null),
            Some(json!(false)),
            Some(json!(true)),
        ] {
            let mut value = json!({"model":"public","input":"hello"});
            if let Some(stream) = initial_stream {
                value["stream"] = stream;
            }
            let request: ResponsesRequest = serde_json::from_value(value.clone()).unwrap();
            let expected = value.get("stream").and_then(Value::as_bool);
            let mut candidate = request.clone();
            candidate.set_model("alternate");
            candidate.set_stream(expected != Some(true));
            candidate.to_chat_completion().unwrap();
            assert!(Arc::ptr_eq(
                &request.compatibility,
                &candidate.compatibility
            ));
            let original = request.to_chat_completion().unwrap();
            assert_eq!(original.model, "public");
            assert_eq!(original.stream, expected);
            for index in 0..8 {
                let mut next = request.clone();
                next.set_model(format!("route-{index}"));
                next.set_stream(index % 2 == 0);
                let chat = next.to_chat_completion().unwrap();
                assert_eq!(chat.model, format!("route-{index}"));
                assert_eq!(chat.stream, Some(index % 2 == 0));
                assert!(Arc::ptr_eq(&request.compatibility, &next.compatibility));
            }
        }
    }

    #[test]
    fn compatibility_translation_cache_is_shared_across_routing_mutations() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "requested",
            "input": "hello"
        }))
        .unwrap();

        assert!(request.compatibility.get().is_none());
        assert_eq!(request.to_chat_completion().unwrap().model, "requested");
        assert!(request.compatibility.get().is_some());

        let mut routed = request.clone();
        assert!(Arc::ptr_eq(&request.compatibility, &routed.compatibility));
        routed.set_model("resolved");
        routed.set_stream(true);

        assert!(Arc::ptr_eq(&request.compatibility, &routed.compatibility));
        let original_chat = request.to_chat_completion().unwrap();
        assert_eq!(original_chat.model, "requested");
        assert_eq!(original_chat.stream, None);
        let routed_chat = routed.to_chat_completion().unwrap();
        assert_eq!(routed_chat.model, "resolved");
        assert_eq!(routed_chat.stream, Some(true));
    }

    #[test]
    fn compatibility_response_echoes_request_fields_and_canonical_defaults() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "compatible-model",
            "input": "hello",
            "instructions": "Be concise",
            "max_output_tokens": 128,
            "temperature": 0.25,
            "top_p": 0.5,
            "store": false,
            "metadata": {},
            "tools": [{
                "type": "function",
                "name": "lookup",
                "description": "Look up a value",
                "parameters": {"type": "object"},
                "strict": false
            }],
            "tool_choice": {"type": "function", "name": "lookup"},
            "text": {"format": {"type": "json_object"}}
        }))
        .unwrap();

        let response = request
            .to_compatibility_response(completed_chat_response())
            .unwrap();
        let body = serde_json::to_value(response).unwrap();

        assert_eq!(body["error"], Value::Null);
        assert_eq!(body["incomplete_details"], Value::Null);
        assert_eq!(body["instructions"], "Be concise");
        assert_eq!(body["max_output_tokens"], 128);
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["metadata"], json!({}));
        assert_eq!(body["store"], false);
        assert_eq!(body["temperature"], 0.25);
        assert_eq!(body["top_p"], 0.5);
        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "name": "lookup"})
        );
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["text"]["format"], json!({"type": "json_object"}));
        assert!(body.get("previous_response_id").is_some());
        assert!(body.get("reasoning").is_some());
        assert!(body.get("truncation").is_some());
        assert!(body.get("usage").is_some());
    }

    #[test]
    fn response_usage_preserves_new_detail_fields() {
        let usage: ResponseUsage = serde_json::from_value(json!({
            "input_tokens": 100,
            "input_tokens_details": {
                "cached_tokens": 80,
                "cache_write_tokens": 10,
                "future_input": 3
            },
            "output_tokens": 20,
            "output_tokens_details": {"reasoning_tokens": 12, "future_output": 4},
            "total_tokens": 120,
            "future_usage": 5
        }))
        .unwrap();

        assert_eq!(usage.input_tokens_details.cached_tokens, 80);
        assert_eq!(usage.input_tokens_details.cache_write_tokens, 10);
        assert_eq!(usage.output_tokens_details.reasoning_tokens, 12);
        assert_eq!(usage.extra["future_usage"], json!(5));
        assert_eq!(usage.input_tokens_details.extra["future_input"], json!(3));
        assert_eq!(usage.output_tokens_details.extra["future_output"], json!(4));
    }
}
