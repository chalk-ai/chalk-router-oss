//! Compatibility conversion between OpenAI Responses and Chat Completions.
//!
//! Converts the supported request/reply subset and synthesizes Responses SSE
//! events for Chat backends. Wire types and the native request envelope remain
//! in [`crate::responses`]; native provider transport does not use this adapter.
//!
//! OpenAI's currently documented `include` selectors are accepted but omitted
//! when translating to Chat Completions, with a debug log naming the selectors.
//! Chat backends cannot supply those supplemental response fields; in
//! particular, `reasoning.encrypted_content` will be absent and cannot provide
//! stateless reasoning continuity. Unknown selectors are still rejected so a
//! newly introduced semantic is not silently discarded.
//!
//! Empty reasoning is a no-op; `reasoning.effort` maps to Chat's `reasoning_effort`.
//! Reasoning summaries remain unsupported. Client telemetry (`client_metadata`)
//! and the `prompt_cache_key` routing hint are accepted but omitted with a debug
//! log; this does not promise upstream caching.
//!
//! Native Chat replies additionally map refusals, URL citations, text token
//! log probabilities, service tier, and known token detail counters. Chat-only
//! fields (including fingerprints, token bytes, and refusal log probabilities)
//! have no Responses equivalent and are omitted rather than copied blindly.

use std::error::Error as StdError;
use std::fmt;

use crate::schema_json::SchemaJson;
use serde_json::{json, Map, Value};

use crate::backend::{BackendError, SseByteStream};
use crate::responses::{
    CreateResponseRequest, CreateResponseResponse, ResponseContent, ResponseContentPart,
    ResponseInput, ResponseInputTokenDetails, ResponseItem, ResponseOutputTokenDetails,
    ResponseTool, ResponseUsage,
};
use crate::types::{
    ChatCompletionRequest, ChatCompletionResponse, ChatMessage, FunctionCall, ImageUrl, Tool,
    ToolCall, ToolFunction,
};

const KNOWN_RESPONSE_INCLUDE_SELECTORS: &[&str] = &[
    "web_search_call.action.sources",
    "code_interpreter_call.outputs",
    "computer_call_output.output.image_url",
    "file_search_call.results",
    "message.input_image.image_url",
    "message.output_text.logprobs",
    "reasoning.encrypted_content",
];

/// Parse effort without interpreting other native Responses reasoning controls.
pub(crate) fn reasoning_effort(reasoning: &Value) -> Result<Option<&str>, TranslationError> {
    if reasoning.is_null() {
        return Ok(None);
    }
    let fields = reasoning
        .as_object()
        .ok_or_else(|| TranslationError::unsupported("reasoning"))?;
    match fields.get("effort") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(effort)) => Ok(Some(effort)),
        Some(_) => Err(TranslationError::invalid(
            "reasoning.effort",
            "must be a string",
        )),
    }
}

impl CreateResponseRequest {
    /// Translate the subset of Responses requests that chat completions can
    /// represent without losing state or changing tool semantics.
    pub fn to_chat_completion(&self) -> Result<ChatCompletionRequest, TranslationError> {
        let effort = self.validate_chat_compatibility()?;

        let mut messages = Vec::new();
        if let Some(instructions) = &self.instructions {
            let instructions = instructions
                .as_str()
                .ok_or_else(|| TranslationError::unsupported("instructions"))?;
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(Value::String(instructions.to_string())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }

        match &self.input {
            ResponseInput::Text(text) => messages.push(ChatMessage {
                role: "user".to_string(),
                content: Some(Value::String(text.clone())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            }),
            ResponseInput::Items(items) => {
                for (index, item) in items.iter().enumerate() {
                    append_input_item(&mut messages, item, index)?;
                }
            }
        }

        let tools = self
            .tools
            .as_ref()
            .map(|tools| tools.iter().map(response_tool_to_chat).collect())
            .transpose()?;

        Ok(ChatCompletionRequest {
            fallbacks: Default::default(),
            model: self.model.clone(),
            messages,
            temperature: self.temperature,
            top_p: self.top_p,
            n: None,
            stream: self.stream,
            stop: None,
            max_tokens: None,
            max_completion_tokens: self.max_output_tokens,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
            user: self.user.clone(),
            tools,
            tool_choice: self
                .tool_choice
                .as_ref()
                .map(response_tool_choice_to_chat)
                .transpose()?,
            response_format: self
                .text
                .as_ref()
                .and_then(|text| text.format.as_ref())
                .map(response_format_to_chat)
                .transpose()?
                .flatten(),
            stream_options: None,
            parallel_tool_calls: self.parallel_tool_calls,
            reasoning_effort: effort.map(str::to_owned),
        })
    }

    /// Reject semantic controls the Chat path cannot honor; permit documented client hints.
    fn validate_chat_compatibility(&self) -> Result<Option<&str>, TranslationError> {
        // `reasoning.effort` has a Chat equivalent (`reasoning_effort`); the
        // summary controls do not, and Chat returns no reasoning items.
        if let Some(reasoning) = self.reasoning.as_ref().filter(|value| !value.is_null()) {
            let Some(fields) = reasoning.as_object() else {
                return Err(TranslationError::unsupported("reasoning"));
            };
            if let Some((name, _)) = fields
                .iter()
                .find(|(name, value)| name.as_str() != "effort" && !value.is_null())
            {
                return Err(TranslationError::unsupported(format!("reasoning.{name}")));
            }
        }
        let effort = self
            .reasoning
            .as_ref()
            .map(reasoning_effort)
            .transpose()?
            .flatten();
        if self.previous_response_id.is_some() {
            return Err(TranslationError::unsupported("previous_response_id"));
        }
        if self.background == Some(true) {
            return Err(TranslationError::unsupported("background=true"));
        }
        if self.store == Some(true) {
            return Err(TranslationError::unsupported("store=true"));
        }
        if self.metadata.as_ref().is_some_and(is_significant_value) {
            return Err(TranslationError::unsupported("metadata"));
        }
        if self.service_tier.is_some() {
            return Err(TranslationError::unsupported("service_tier"));
        }
        if self.truncation.is_some() {
            return Err(TranslationError::unsupported("truncation"));
        }
        if let Some(metadata) = self.extra.get("client_metadata") {
            if !metadata.is_null()
                && !metadata
                    .as_object()
                    .is_some_and(|fields| fields.values().all(Value::is_string))
            {
                return Err(TranslationError::invalid(
                    "client_metadata",
                    "expected a string-valued object",
                ));
            }
        }
        reject_object_fields(&self.extra, &["client_metadata"], "request")?;
        if self.prompt_cache_key.is_some() || self.extra.contains_key("client_metadata") {
            tracing::debug!(
                "Responses cache hint and client telemetry omitted translating to chat completions"
            );
        }

        if let Some(text) = &self.text {
            if text.verbosity.is_some() {
                return Err(TranslationError::unsupported("text.verbosity"));
            }
            reject_significant_extra(&text.extra, "text")?;
        }
        self.validate_and_log_omitted_include()?;
        Ok(effort)
    }

    fn validate_and_log_omitted_include(&self) -> Result<(), TranslationError> {
        let Some(include) = self.include.as_ref().filter(|include| !include.is_empty()) else {
            return Ok(());
        };
        if let Some((index, _)) = include
            .iter()
            .enumerate()
            .find(|(_, selector)| !KNOWN_RESPONSE_INCLUDE_SELECTORS.contains(&selector.as_str()))
        {
            return Err(TranslationError::unsupported(format!("include[{index}]")));
        }
        tracing::debug!(
            include = ?include,
            "Responses include selectors omitted translating to chat completions; supplemental fields will be absent"
        );
        Ok(())
    }
}

impl CreateResponseResponse {
    /// Convert one chat-completions choice into an equivalent Responses object.
    /// Responses has no `n`/multi-choice representation, so multiple choices
    /// are rejected rather than arbitrarily selecting one.
    pub fn from_chat_completion(
        response: ChatCompletionResponse,
    ) -> Result<Self, TranslationError> {
        Self::from_chat_completion_with_request(response, None)
    }

    /// Convert a Chat reply while retaining the originating Responses request fields.
    pub(crate) fn from_chat_completion_for_request(
        response: ChatCompletionResponse,
        request: &CreateResponseRequest,
    ) -> Result<Self, TranslationError> {
        Self::from_chat_completion_with_request(response, Some(request))
    }

    fn from_chat_completion_with_request(
        response: ChatCompletionResponse,
        request: Option<&CreateResponseRequest>,
    ) -> Result<Self, TranslationError> {
        if response.choices.len() != 1 {
            return Err(TranslationError::InvalidField {
                path: "choices".to_string(),
                message: format!(
                    "Responses translation requires exactly one chat choice, got {}",
                    response.choices.len()
                ),
            });
        }

        let choice = response.choices.into_iter().next().expect("length checked");
        if choice.message.role != "assistant" {
            return Err(TranslationError::InvalidField {
                path: "choices[0].message.role".to_string(),
                message: format!("expected assistant, got {}", choice.message.role),
            });
        }
        if choice.message.name.is_some() {
            return Err(TranslationError::unsupported("choices[0].message.name"));
        }
        if choice.message.tool_call_id.is_some() {
            return Err(TranslationError::unsupported(
                "choices[0].message.tool_call_id",
            ));
        }

        let (status, incomplete_details) =
            finish_reason_to_status(choice.finish_reason.as_deref())?;
        let mut output = Vec::new();
        if let Some(content) = choice.message.content {
            let parts = chat_output_to_response_parts(content)?;
            if !parts.is_empty() {
                output.push(message_output_item(&response.id, choice.index, parts));
            }
        }

        for tool_call in choice.message.tool_calls.unwrap_or_default() {
            if tool_call.tool_type != "function" {
                return Err(TranslationError::UnsupportedFeature {
                    path: format!("chat tool call type {}", tool_call.tool_type),
                });
            }
            output.push(ResponseItem {
                item_type: Some("function_call".to_string()),
                id: Some(format!("fc_{}", tool_call.id)),
                status: Some("completed".to_string()),
                role: None,
                content: None,
                call_id: Some(tool_call.id),
                name: Some(tool_call.function.name),
                arguments: Some(tool_call.function.arguments),
                output: None,
                summary: None,
                encrypted_content: None,
                extra: Map::new(),
            });
        }

        let usage = response.usage.map(|usage| ResponseUsage {
            input_tokens: i64::from(usage.prompt_tokens),
            input_tokens_details: ResponseInputTokenDetails {
                cached_tokens: usage
                    .prompt_tokens_details
                    .as_ref()
                    .map_or(0, |details| i64::from(details.cached_tokens)),
                cache_write_tokens: usage
                    .prompt_tokens_details
                    .as_ref()
                    .map_or(0, |details| i64::from(details.cache_write_tokens)),
                ..Default::default()
            },
            output_tokens: i64::from(usage.completion_tokens.unwrap_or(0)),
            output_tokens_details: ResponseOutputTokenDetails {
                reasoning_tokens: usage
                    .completion_tokens_details
                    .as_ref()
                    .map_or(0, |details| i64::from(details.reasoning_tokens)),
                ..Default::default()
            },
            total_tokens: i64::from(usage.total_tokens),
            extra: Map::new(),
        });

        Ok(Self {
            id: response.id,
            object: "response".to_string(),
            created_at: response.created,
            status: status.to_string(),
            model: response.model,
            output,
            error: None,
            incomplete_details,
            instructions: request.and_then(|request| request.instructions.clone()),
            max_output_tokens: request.and_then(|request| request.max_output_tokens),
            parallel_tool_calls: Some(
                request
                    .and_then(|request| request.parallel_tool_calls)
                    .unwrap_or(true),
            ),
            previous_response_id: request.and_then(|request| request.previous_response_id.clone()),
            reasoning: request.and_then(|request| request.reasoning.clone()),
            store: request.and_then(|request| request.store),
            temperature: request.and_then(|request| request.temperature),
            text: request.and_then(|request| request.text.clone()),
            tool_choice: Some(
                request
                    .and_then(|request| request.tool_choice.clone())
                    .unwrap_or_else(|| Value::String("auto".to_string())),
            ),
            tools: Some(
                request
                    .and_then(|request| request.tools.clone())
                    .unwrap_or_default(),
            ),
            top_p: request.and_then(|request| request.top_p),
            truncation: request.and_then(|request| request.truncation.clone()),
            usage,
            metadata: Some(
                request
                    .and_then(|request| request.metadata.clone())
                    .unwrap_or_else(|| Value::Object(Map::new())),
            ),
            extra: Map::new(),
        })
    }
}

fn message_output_item(id: &str, index: i32, parts: Vec<ResponseContentPart>) -> ResponseItem {
    ResponseItem {
        item_type: Some("message".to_string()),
        id: Some(format!("msg_{id}_{index}")),
        status: Some("completed".to_string()),
        role: Some("assistant".to_string()),
        content: Some(ResponseContent::Parts(parts)),
        call_id: None,
        name: None,
        arguments: None,
        output: None,
        summary: None,
        encrypted_content: None,
        extra: Map::new(),
    }
}

/// Retain only native Chat fields with an explicit Responses representation.
/// The typed converter has already checked choice count, role and finish reason.
pub(crate) fn retain_native_chat_reply_fields(
    response: &mut CreateResponseResponse,
    raw: &Value,
) -> Result<(), TranslationError> {
    let choice = &raw["choices"][0];
    let message = &choice["message"];
    let refusal = message.get("refusal").filter(|value| !value.is_null());
    let refusal = refusal
        .map(|value| {
            value.as_str().ok_or_else(|| {
                TranslationError::invalid("choices[0].message.refusal", "expected a string")
            })
        })
        .transpose()?;
    let mut message_index = response
        .output
        .iter()
        .position(|item| item.item_type.as_deref() == Some("message"));
    if refusal.is_some() && message_index.is_none() {
        let index = choice["index"].as_i64().unwrap_or(0) as i32;
        response
            .output
            .insert(0, message_output_item(&response.id, index, Vec::new()));
        message_index = Some(0);
    }
    if let Some(index) = message_index {
        if let Some(ResponseContent::Parts(parts)) = &mut response.output[index].content {
            if let Some(refusal) = refusal {
                // Refusal-only replies use both null and empty text in Chat.
                parts.retain(|part| {
                    part.part_type != "output_text" || part.text.as_deref() != Some("")
                });
                parts.push(ResponseContentPart {
                    part_type: "refusal".to_string(),
                    refusal: Some(refusal.to_string()),
                    text: None,
                    image_url: None,
                    file_id: None,
                    detail: None,
                    annotations: Vec::new(),
                    extra: Map::new(),
                });
            }
            let mut text_parts = parts
                .iter_mut()
                .filter(|part| part.part_type == "output_text");
            let first = text_parts.next();
            if text_parts.next().is_none() {
                if let Some(part) = first {
                    if let Some(annotations) = message.get("annotations").and_then(Value::as_array)
                    {
                        part.annotations =
                            annotations.iter().filter_map(chat_url_citation).collect();
                    }
                    if let Some(logprobs) =
                        choice["logprobs"].get("content").and_then(Value::as_array)
                    {
                        part.extra.insert(
                            "logprobs".to_string(),
                            Value::Array(logprobs.iter().filter_map(chat_text_logprob).collect()),
                        );
                    }
                }
            } else {
                tracing::debug!(
                    "omitting Chat annotations/logprobs with ambiguous content-part offsets"
                );
            }
        }
    }
    if let Some(usage) = &mut response.usage {
        for (source, target, names) in [
            (
                "prompt_tokens_details",
                &mut usage.input_tokens_details.extra,
                &["audio_tokens"][..],
            ),
            (
                "completion_tokens_details",
                &mut usage.output_tokens_details.extra,
                &[
                    "audio_tokens",
                    "accepted_prediction_tokens",
                    "rejected_prediction_tokens",
                ][..],
            ),
        ] {
            for name in names {
                if let Some(value) = raw["usage"][source]
                    .get(*name)
                    .filter(|value| value.as_u64().is_some())
                {
                    target.insert((*name).to_string(), value.clone());
                }
            }
        }
    }
    if let Some(tier) = raw.get("service_tier").filter(|value| value.is_string()) {
        response
            .extra
            .insert("service_tier".to_string(), tier.clone());
    }
    for field in ["system_fingerprint", "logprobs.refusal"] {
        let value = if field == "logprobs.refusal" {
            &choice["logprobs"]["refusal"]
        } else {
            &raw[field]
        };
        if is_significant_value(value) {
            tracing::debug!(
                field,
                "omitting Chat reply field without a Responses representation"
            );
        }
    }
    Ok(())
}

fn chat_url_citation(annotation: &Value) -> Option<Value> {
    if annotation["type"] != "url_citation" {
        tracing::debug!(annotation_type = ?annotation.get("type"), "omitting unsupported Chat annotation");
        return None;
    }
    let citation = &annotation["url_citation"];
    Some(json!({
        "type": "url_citation", "start_index": citation.get("start_index")?.as_u64()?,
        "end_index": citation.get("end_index")?.as_u64()?,
        "title": citation.get("title")?.as_str()?, "url": citation.get("url")?.as_str()?,
    }))
}

fn chat_text_logprob(logprob: &Value) -> Option<Value> {
    let mut mapped = json!({"token":logprob.get("token")?.as_str()?, "logprob":logprob.get("logprob")?.as_number()?, "top_logprobs":[]});
    if let Some(top) = logprob.get("top_logprobs").and_then(Value::as_array) {
        mapped["top_logprobs"] = Value::Array(top.iter().filter_map(|entry| {
            Some(json!({"token":entry.get("token")?.as_str()?, "logprob":entry.get("logprob")?.as_number()?}))
        }).collect());
    }
    Some(mapped)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslationError {
    UnsupportedFeature { path: String },
    InvalidField { path: String, message: String },
}

impl TranslationError {
    fn unsupported(path: impl Into<String>) -> Self {
        Self::UnsupportedFeature { path: path.into() }
    }

    /// Describe an invalid field while lazily parsing a compatibility request.
    pub(crate) fn invalid(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self::InvalidField {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for TranslationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFeature { path } => {
                write!(
                    f,
                    "Responses field `{path}` cannot be represented losslessly by chat completions"
                )
            }
            Self::InvalidField { path, message } => write!(f, "invalid `{path}`: {message}"),
        }
    }
}

impl StdError for TranslationError {}

impl TryFrom<&CreateResponseRequest> for ChatCompletionRequest {
    type Error = TranslationError;

    fn try_from(request: &CreateResponseRequest) -> Result<Self, Self::Error> {
        request.to_chat_completion()
    }
}

impl TryFrom<ChatCompletionResponse> for CreateResponseResponse {
    type Error = TranslationError;

    fn try_from(response: ChatCompletionResponse) -> Result<Self, Self::Error> {
        Self::from_chat_completion(response)
    }
}

fn append_input_item(
    messages: &mut Vec<ChatMessage>,
    item: &ResponseItem,
    index: usize,
) -> Result<(), TranslationError> {
    let item_type =
        item.item_type.as_deref().unwrap_or_else(
            || {
                if item.role.is_some() {
                    "message"
                } else {
                    ""
                }
            },
        );
    let path = format!("input[{index}]");
    reject_significant_extra(&item.extra, &path)?;

    match item_type {
        "message" => {
            if item.call_id.is_some()
                || item.arguments.is_some()
                || item.output.is_some()
                || item.summary.is_some()
                || item.encrypted_content.is_some()
            {
                return Err(TranslationError::unsupported(&path));
            }
            let role = item
                .role
                .as_deref()
                .ok_or_else(|| TranslationError::invalid(format!("{path}.role"), "missing role"))?;
            if !matches!(role, "system" | "developer" | "user" | "assistant") {
                return Err(TranslationError::invalid(
                    format!("{path}.role"),
                    format!("unsupported role {role}"),
                ));
            }
            let content = item
                .content
                .as_ref()
                .ok_or_else(|| {
                    TranslationError::invalid(format!("{path}.content"), "missing content")
                })
                .and_then(|content| response_content_to_chat(content, &path))?;
            messages.push(ChatMessage {
                role: role.to_string(),
                content: Some(content),
                name: item.name.clone(),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        "function_call" => {
            if item.role.is_some()
                || item.content.is_some()
                || item.encrypted_content.is_some()
                || item.summary.is_some()
                || item.output.is_some()
            {
                return Err(TranslationError::unsupported(&path));
            }
            let call_id = required_string(item.call_id.as_ref(), format!("{path}.call_id"))?;
            let name = required_string(item.name.as_ref(), format!("{path}.name"))?;
            let arguments = required_string(item.arguments.as_ref(), format!("{path}.arguments"))?;
            let tool_call = ToolCall {
                id: call_id.clone(),
                tool_type: "function".to_string(),
                function: FunctionCall {
                    name: name.clone(),
                    arguments: arguments.clone(),
                },
            };

            // Responses represents each function call as an item. Chat groups
            // adjacent calls into a single assistant message.
            if let Some(last) = messages.last_mut() {
                if last.role == "assistant" && last.tool_call_id.is_none() {
                    last.tool_calls.get_or_insert_with(Vec::new).push(tool_call);
                    return Ok(());
                }
            }
            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: None,
                name: None,
                tool_calls: Some(vec![tool_call]),
                tool_call_id: None,
            });
        }
        "function_call_output" => {
            if item.role.is_some()
                || item.content.is_some()
                || item.name.is_some()
                || item.arguments.is_some()
                || item.encrypted_content.is_some()
                || item.summary.is_some()
            {
                return Err(TranslationError::unsupported(&path));
            }
            let call_id = required_string(item.call_id.as_ref(), format!("{path}.call_id"))?;
            let output = item.output.as_ref().ok_or_else(|| {
                TranslationError::invalid(format!("{path}.output"), "missing output")
            })?;
            messages.push(ChatMessage {
                role: "tool".to_string(),
                content: Some(function_output_to_chat(output, &path)?),
                name: None,
                tool_calls: None,
                tool_call_id: Some(call_id.clone()),
            });
        }
        "reasoning" => return Err(TranslationError::unsupported(format!("{path}.reasoning"))),
        "" => {
            return Err(TranslationError::invalid(
                format!("{path}.type"),
                "missing type and role",
            ))
        }
        other => {
            return Err(TranslationError::unsupported(format!(
                "{path}.type={other}"
            )))
        }
    }
    Ok(())
}

fn response_content_to_chat(
    content: &ResponseContent,
    item_path: &str,
) -> Result<Value, TranslationError> {
    match content {
        ResponseContent::Text(text) => Ok(Value::String(text.clone())),
        ResponseContent::Parts(parts) => parts
            .iter()
            .enumerate()
            .map(|(index, part)| {
                content_part_to_chat(part, &format!("{item_path}.content[{index}]"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
    }
}

fn content_part_to_chat(part: &ResponseContentPart, path: &str) -> Result<Value, TranslationError> {
    reject_significant_extra(&part.extra, path)?;
    if !part.annotations.is_empty() {
        return Err(TranslationError::unsupported(format!("{path}.annotations")));
    }
    match part.part_type.as_str() {
        "input_text" | "output_text" => {
            if part.image_url.is_some()
                || part.file_id.is_some()
                || part.detail.is_some()
                || part.refusal.is_some()
            {
                return Err(TranslationError::unsupported(path));
            }
            let text = required_string(part.text.as_ref(), format!("{path}.text"))?;
            Ok(json!({"type": "text", "text": text}))
        }
        "input_image" => {
            if part.text.is_some() || part.refusal.is_some() {
                return Err(TranslationError::unsupported(path));
            }
            if part.file_id.is_some() {
                return Err(TranslationError::unsupported(format!("{path}.file_id")));
            }
            let url = required_string(part.image_url.as_ref(), format!("{path}.image_url"))?;
            Ok(json!({
                "type": "image_url",
                "image_url": ImageUrl {
                    url: url.clone(),
                    detail: part.detail.clone(),
                },
            }))
        }
        other => Err(TranslationError::unsupported(format!(
            "{path}.type={other}"
        ))),
    }
}

fn function_output_to_chat(output: &Value, item_path: &str) -> Result<Value, TranslationError> {
    match output {
        Value::String(text) => Ok(Value::String(text.clone())),
        Value::Array(_) => {
            let parts: Vec<ResponseContentPart> =
                serde_json::from_value(output.clone()).map_err(|e| {
                    TranslationError::invalid(format!("{item_path}.output"), e.to_string())
                })?;
            parts
                .iter()
                .enumerate()
                .map(|(index, part)| {
                    content_part_to_chat(part, &format!("{item_path}.output[{index}]"))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
        _ => Err(TranslationError::invalid(
            format!("{item_path}.output"),
            "expected a string or content-part array",
        )),
    }
}

fn response_tool_to_chat(tool: &ResponseTool) -> Result<Tool, TranslationError> {
    if tool.tool_type != "function" {
        return Err(TranslationError::unsupported(format!(
            "tools.type={}",
            tool.tool_type
        )));
    }
    if tool.strict == Some(true) {
        return Err(TranslationError::unsupported("tools[].strict=true"));
    }
    reject_significant_extra(&tool.extra, "tools[]")?;
    let name = required_string(tool.name.as_ref(), "tools[].name")?;
    Ok(Tool {
        tool_type: "function".to_string(),
        function: ToolFunction {
            name: name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
        },
    })
}

fn response_tool_choice_to_chat(choice: &Value) -> Result<Value, TranslationError> {
    match choice {
        Value::String(choice) if matches!(choice.as_str(), "none" | "auto" | "required") => {
            Ok(Value::String(choice.clone()))
        }
        Value::Object(object) if object.get("type").and_then(Value::as_str) == Some("function") => {
            let name = object.get("name").and_then(Value::as_str).ok_or_else(|| {
                TranslationError::invalid("tool_choice.name", "missing function name")
            })?;
            if object
                .iter()
                .any(|(key, value)| key != "type" && key != "name" && is_significant_value(value))
            {
                return Err(TranslationError::unsupported("tool_choice"));
            }
            Ok(json!({"type": "function", "function": {"name": name}}))
        }
        _ => Err(TranslationError::unsupported("tool_choice")),
    }
}

fn response_format_to_chat(format: &SchemaJson) -> Result<Option<SchemaJson>, TranslationError> {
    let object = format
        .as_object()
        .ok_or_else(|| TranslationError::invalid("text.format", "expected an object"))?;
    match object.get("type").and_then(Value::as_str) {
        Some("text") => {
            reject_object_fields(object, &["type"], "text.format")?;
            Ok(None)
        }
        Some("json_object") => {
            reject_object_fields(object, &["type"], "text.format")?;
            Ok(Some(json!({"type": "json_object"}).into()))
        }
        Some("json_schema") => {
            reject_object_fields(
                object,
                &["type", "name", "schema", "strict", "description"],
                "text.format",
            )?;
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| TranslationError::invalid("text.format.name", "missing name"))?;
            let schema = format
                .member("schema")
                .map_err(|error| TranslationError::invalid("text.format", error.to_string()))?
                .ok_or_else(|| TranslationError::invalid("text.format.schema", "missing schema"))?;
            let mut fields = vec![
                ("name".to_string(), Value::String(name.to_string()).into()),
                ("schema".to_string(), schema),
            ];
            for key in ["strict", "description"] {
                if let Some(value) = format
                    .member(key)
                    .map_err(|error| TranslationError::invalid("text.format", error.to_string()))?
                {
                    fields.push((key.to_string(), value));
                }
            }
            Ok(Some(SchemaJson::object([
                ("type", json!("json_schema").into()),
                ("json_schema", SchemaJson::object(fields)),
            ])))
        }
        Some(other) => Err(TranslationError::unsupported(format!(
            "text.format.type={other}"
        ))),
        None => Err(TranslationError::invalid(
            "text.format.type",
            "missing type",
        )),
    }
}

fn chat_output_to_response_parts(
    content: Value,
) -> Result<Vec<ResponseContentPart>, TranslationError> {
    match content {
        Value::Null => Ok(Vec::new()),
        Value::String(text) => Ok(vec![output_text_part(text)]),
        Value::Array(parts) => parts
            .into_iter()
            .enumerate()
            .map(|(index, part)| {
                let part = part.as_object().ok_or_else(|| {
                    TranslationError::invalid(
                        format!("choices[0].message.content[{index}]"),
                        "expected an object",
                    )
                })?;
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => part
                        .get("text")
                        .and_then(Value::as_str)
                        .map(|text| output_text_part(text.to_string()))
                        .ok_or_else(|| {
                            TranslationError::invalid(
                                format!("choices[0].message.content[{index}].text"),
                                "missing text",
                            )
                        }),
                    Some(other) => Err(TranslationError::unsupported(format!(
                        "choices[0].message.content[{index}].type={other}"
                    ))),
                    None => Err(TranslationError::invalid(
                        format!("choices[0].message.content[{index}].type"),
                        "missing type",
                    )),
                }
            })
            .collect(),
        _ => Err(TranslationError::invalid(
            "choices[0].message.content",
            "expected a string, null, or content-part array",
        )),
    }
}

fn output_text_part(text: String) -> ResponseContentPart {
    ResponseContentPart {
        part_type: "output_text".to_string(),
        text: Some(text),
        image_url: None,
        file_id: None,
        detail: None,
        refusal: None,
        annotations: Vec::new(),
        extra: Map::new(),
    }
}

fn finish_reason_to_status(
    finish_reason: Option<&str>,
) -> Result<(&'static str, Option<Value>), TranslationError> {
    match finish_reason {
        None | Some("stop" | "tool_calls" | "function_call") => Ok(("completed", None)),
        Some("length") => Ok(("incomplete", Some(json!({"reason": "max_output_tokens"})))),
        Some("content_filter") => Ok(("incomplete", Some(json!({"reason": "content_filter"})))),
        Some(other) => Err(TranslationError::unsupported(format!(
            "choices[0].finish_reason={other}"
        ))),
    }
}

fn required_string(
    value: Option<&String>,
    path: impl Into<String>,
) -> Result<&String, TranslationError> {
    value.ok_or_else(|| TranslationError::invalid(path, "missing string"))
}

fn is_significant_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
        _ => true,
    }
}

fn reject_significant_extra(
    extra: &Map<String, Value>,
    path: &str,
) -> Result<(), TranslationError> {
    if let Some((key, _)) = extra.iter().find(|(_, value)| is_significant_value(value)) {
        return Err(TranslationError::unsupported(format!("{path}.{key}")));
    }
    Ok(())
}

fn reject_object_fields(
    object: &Map<String, Value>,
    allowed: &[&str],
    path: &str,
) -> Result<(), TranslationError> {
    if let Some((key, _)) = object
        .iter()
        .find(|(key, value)| !allowed.contains(&key.as_str()) && is_significant_value(value))
    {
        return Err(TranslationError::unsupported(format!("{path}.{key}")));
    }
    Ok(())
}

fn sse_event(event: &str, data: serde_json::Value) -> Result<bytes::Bytes, BackendError> {
    let data = serde_json::to_string(&data).map_err(|error| {
        BackendError::Other(format!("failed to serialize Responses SSE event: {error}"))
    })?;
    Ok(bytes::Bytes::from(format!(
        "event: {event}\ndata: {data}\n\n"
    )))
}

/// Turn a completed Responses object into a standards-compliant SSE lifecycle.
/// This is intentionally a fallback for chat-compatible providers; native
/// Responses providers stream their upstream bytes as they arrive.
pub fn synthesize_response_stream(
    response: serde_json::Value,
) -> Result<SseByteStream, BackendError> {
    Ok(Box::pin(futures::stream::iter(
        SynthesizedResponseEvents::new(response),
    )))
}

#[derive(Debug, Clone, Copy)]
enum SynthesizedEventStage {
    Created,
    InProgress,
    ItemAdded,
    ContentPartAdded,
    TextDelta,
    AnnotationAdded,
    TextDone,
    ContentPartDone,
    FunctionArgumentsDelta,
    FunctionArgumentsDone,
    ItemDone,
    Terminal,
    Done,
}

/// Lazily serializes one synthesized event at a time. The completed response is
/// retained once for the terminal event instead of materializing every SSE
/// frame—and every repeated output string—before the first byte is polled.
struct SynthesizedResponseEvents {
    response: serde_json::Value,
    in_progress: serde_json::Value,
    sequence_number: u64,
    output_index: usize,
    content_index: usize,
    annotation_index: usize,
    stage: SynthesizedEventStage,
}

impl SynthesizedResponseEvents {
    fn new(response: serde_json::Value) -> Self {
        let mut in_progress = response.clone();
        if let Some(object) = in_progress.as_object_mut() {
            object.insert(
                "status".to_string(),
                serde_json::Value::String("in_progress".to_string()),
            );
            object.insert("output".to_string(), serde_json::Value::Array(Vec::new()));
            object.insert("usage".to_string(), serde_json::Value::Null);
        }
        Self {
            response,
            in_progress,
            sequence_number: 0,
            output_index: 0,
            content_index: 0,
            annotation_index: 0,
            stage: SynthesizedEventStage::Created,
        }
    }

    fn next_sequence_number(&mut self) -> u64 {
        let sequence_number = self.sequence_number;
        self.sequence_number += 1;
        sequence_number
    }

    fn item(&self) -> Option<&serde_json::Value> {
        self.response
            .get("output")
            .and_then(serde_json::Value::as_array)
            .and_then(|output| output.get(self.output_index))
    }

    fn item_id(&self) -> String {
        self.item()
            .and_then(|item| item.get("id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    fn content_part(&self) -> Option<&Value> {
        self.item()?
            .get("content")?
            .as_array()?
            .get(self.content_index)
    }

    fn is_refusal(&self) -> bool {
        self.content_part()
            .is_some_and(|part| part["type"] == "refusal")
    }

    fn seek_next_content_part(&mut self) -> bool {
        let next_index = self
            .item()
            .and_then(|item| item.get("content"))
            .and_then(serde_json::Value::as_array)
            .and_then(|content| {
                content
                    .iter()
                    .enumerate()
                    .skip(self.content_index)
                    .find(|(_, part)| {
                        let field = if part["type"] == "refusal" {
                            "refusal"
                        } else {
                            "text"
                        };
                        part.get(field).and_then(Value::as_str).is_some()
                    })
                    .map(|(index, _)| index)
            });
        if let Some(index) = next_index {
            self.content_index = index;
            true
        } else {
            false
        }
    }
}

impl Iterator for SynthesizedResponseEvents {
    type Item = Result<bytes::Bytes, BackendError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.stage {
            SynthesizedEventStage::Created => {
                self.stage = SynthesizedEventStage::InProgress;
                let sequence_number = self.next_sequence_number();
                Some(sse_event(
                    "response.created",
                    serde_json::json!({
                        "type": "response.created",
                        "sequence_number": sequence_number,
                        "response": self.in_progress.clone(),
                    }),
                ))
            }
            SynthesizedEventStage::InProgress => {
                self.stage = if self.item().is_some() {
                    SynthesizedEventStage::ItemAdded
                } else {
                    SynthesizedEventStage::Terminal
                };
                let sequence_number = self.next_sequence_number();
                Some(sse_event(
                    "response.in_progress",
                    serde_json::json!({
                        "type": "response.in_progress",
                        "sequence_number": sequence_number,
                        "response": self.in_progress.clone(),
                    }),
                ))
            }
            SynthesizedEventStage::ItemAdded => {
                let mut added_item = self.item()?.clone();
                let item_type = added_item
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                if let Some(object) = added_item.as_object_mut() {
                    object.insert(
                        "status".to_string(),
                        serde_json::Value::String("in_progress".to_string()),
                    );
                    match item_type.as_deref() {
                        Some("message") => {
                            object.insert("content".to_string(), serde_json::json!([]));
                        }
                        Some("function_call") => {
                            object.insert(
                                "arguments".to_string(),
                                serde_json::Value::String(String::new()),
                            );
                        }
                        _ => {}
                    }
                }
                self.content_index = 0;
                self.stage = match item_type.as_deref() {
                    Some("message") if self.seek_next_content_part() => {
                        SynthesizedEventStage::ContentPartAdded
                    }
                    Some("function_call") => SynthesizedEventStage::FunctionArgumentsDelta,
                    _ => SynthesizedEventStage::ItemDone,
                };
                let sequence_number = self.next_sequence_number();
                Some(sse_event(
                    "response.output_item.added",
                    serde_json::json!({
                        "type": "response.output_item.added",
                        "sequence_number": sequence_number,
                        "output_index": self.output_index,
                        "item": added_item,
                    }),
                ))
            }
            SynthesizedEventStage::ContentPartAdded => {
                self.stage = SynthesizedEventStage::TextDelta;
                self.annotation_index = 0;
                let mut part = self.content_part()?.clone();
                if self.is_refusal() {
                    part["refusal"] = json!("");
                } else {
                    part["text"] = json!("");
                    part["annotations"] = json!([]);
                    if part.get("logprobs").is_some() {
                        part["logprobs"] = json!([]);
                    }
                }
                let sequence_number = self.next_sequence_number();
                Some(sse_event(
                    "response.content_part.added",
                    serde_json::json!({
                        "type": "response.content_part.added",
                        "sequence_number": sequence_number,
                        "item_id": self.item_id(),
                        "output_index": self.output_index,
                        "content_index": self.content_index,
                        "part": part,
                    }),
                ))
            }
            SynthesizedEventStage::TextDelta => {
                let refusal = self.is_refusal();
                let part = self.content_part()?.clone();
                self.stage = if !refusal
                    && part
                        .get("annotations")
                        .and_then(Value::as_array)
                        .is_some_and(|annotations| !annotations.is_empty())
                {
                    SynthesizedEventStage::AnnotationAdded
                } else {
                    SynthesizedEventStage::TextDone
                };
                let text = part[if refusal { "refusal" } else { "text" }]
                    .as_str()
                    .unwrap_or_default();
                let event_type = if refusal {
                    "response.refusal.delta"
                } else {
                    "response.output_text.delta"
                };
                let sequence_number = self.next_sequence_number();
                let mut event = json!({
                    "type": event_type,
                    "sequence_number": sequence_number,
                    "item_id": self.item_id(),
                    "output_index": self.output_index,
                    "content_index": self.content_index,
                    "delta": text,
                });
                if !refusal {
                    if let Some(logprobs) = part.get("logprobs") {
                        event["logprobs"] = logprobs.clone();
                    }
                }
                Some(sse_event(event_type, event))
            }
            SynthesizedEventStage::AnnotationAdded => {
                let annotations = self.content_part()?.get("annotations")?.as_array()?;
                let annotation = annotations.get(self.annotation_index)?.clone();
                let annotation_count = annotations.len();
                let annotation_index = self.annotation_index;
                self.annotation_index += 1;
                if self.annotation_index == annotation_count {
                    self.stage = SynthesizedEventStage::TextDone;
                }
                let sequence_number = self.next_sequence_number();
                Some(sse_event(
                    "response.output_text.annotation.added",
                    json!({
                        "type": "response.output_text.annotation.added", "sequence_number": sequence_number,
                        "item_id": self.item_id(), "output_index": self.output_index,
                        "content_index": self.content_index, "annotation_index": annotation_index,
                        "annotation": annotation,
                    }),
                ))
            }
            SynthesizedEventStage::TextDone => {
                self.stage = SynthesizedEventStage::ContentPartDone;
                let refusal = self.is_refusal();
                let field = if refusal { "refusal" } else { "text" };
                let part = self.content_part()?.clone();
                let event_type = if refusal {
                    "response.refusal.done"
                } else {
                    "response.output_text.done"
                };
                let sequence_number = self.next_sequence_number();
                let mut event = json!({
                    "type": event_type,
                    "sequence_number": sequence_number,
                    "item_id": self.item_id(),
                    "output_index": self.output_index,
                    "content_index": self.content_index,
                });
                event[field] = part[field].clone();
                if !refusal {
                    if let Some(logprobs) = part.get("logprobs") {
                        event["logprobs"] = logprobs.clone();
                    }
                }
                Some(sse_event(event_type, event))
            }
            SynthesizedEventStage::ContentPartDone => {
                let part = self
                    .item()
                    .and_then(|item| item.get("content"))
                    .and_then(serde_json::Value::as_array)
                    .and_then(|content| content.get(self.content_index))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let sequence_number = self.next_sequence_number();
                let content_index = self.content_index;
                self.content_index += 1;
                self.stage = if self.seek_next_content_part() {
                    SynthesizedEventStage::ContentPartAdded
                } else {
                    SynthesizedEventStage::ItemDone
                };
                Some(sse_event(
                    "response.content_part.done",
                    serde_json::json!({
                        "type": "response.content_part.done",
                        "sequence_number": sequence_number,
                        "item_id": self.item_id(),
                        "output_index": self.output_index,
                        "content_index": content_index,
                        "part": part,
                    }),
                ))
            }
            SynthesizedEventStage::FunctionArgumentsDelta => {
                self.stage = SynthesizedEventStage::FunctionArgumentsDone;
                let arguments = self
                    .item()
                    .and_then(|item| item.get("arguments"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let sequence_number = self.next_sequence_number();
                Some(sse_event(
                    "response.function_call_arguments.delta",
                    serde_json::json!({
                        "type": "response.function_call_arguments.delta",
                        "sequence_number": sequence_number,
                        "item_id": self.item_id(),
                        "output_index": self.output_index,
                        "delta": arguments,
                    }),
                ))
            }
            SynthesizedEventStage::FunctionArgumentsDone => {
                self.stage = SynthesizedEventStage::ItemDone;
                let arguments = self
                    .item()
                    .and_then(|item| item.get("arguments"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let sequence_number = self.next_sequence_number();
                Some(sse_event(
                    "response.function_call_arguments.done",
                    serde_json::json!({
                        "type": "response.function_call_arguments.done",
                        "sequence_number": sequence_number,
                        "item_id": self.item_id(),
                        "output_index": self.output_index,
                        "arguments": arguments,
                    }),
                ))
            }
            SynthesizedEventStage::ItemDone => {
                let item = self.item()?.clone();
                let output_index = self.output_index;
                self.output_index += 1;
                self.stage = if self.item().is_some() {
                    SynthesizedEventStage::ItemAdded
                } else {
                    SynthesizedEventStage::Terminal
                };
                let sequence_number = self.next_sequence_number();
                Some(sse_event(
                    "response.output_item.done",
                    serde_json::json!({
                        "type": "response.output_item.done",
                        "sequence_number": sequence_number,
                        "output_index": output_index,
                        "item": item,
                    }),
                ))
            }
            SynthesizedEventStage::Terminal => {
                self.stage = SynthesizedEventStage::Done;
                let response = std::mem::take(&mut self.response);
                let terminal_type = match response.get("status").and_then(serde_json::Value::as_str)
                {
                    Some("failed") => "response.failed",
                    Some("incomplete") => "response.incomplete",
                    _ => "response.completed",
                };
                let sequence_number = self.next_sequence_number();
                Some(sse_event(
                    terminal_type,
                    serde_json::json!({
                        "type": terminal_type,
                        "sequence_number": sequence_number,
                        "response": response,
                    }),
                ))
            }
            SynthesizedEventStage::Done => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatChoice, Usage};
    use futures::StreamExt;

    /// Client hints must not reach Chat providers or weaken semantic validation.
    #[test]
    fn compatibility_hints_do_not_weaken_semantic_validation() {
        let original = json!({
            "model": "compatible-model", "input": "hello", "reasoning": {},
            "prompt_cache_key": "session", "client_metadata": {"session_id": "test"}
        });
        let request: CreateResponseRequest = serde_json::from_value(original.clone()).unwrap();
        let translated = serde_json::to_value(request.to_chat_completion().unwrap()).unwrap();
        assert!(translated.get("reasoning").is_none());
        assert!(translated.get("prompt_cache_key").is_none());
        assert!(translated.get("client_metadata").is_none());
        for (field, value) in [
            ("reasoning", json!({"effort": "high", "summary": "auto"})),
            ("reasoning", json!(false)),
            ("client_metadata", json!({"session_id": 1})),
            ("client_metadata", json!("session")),
            ("future_control", json!(true)),
        ] {
            let mut raw = original.clone();
            raw[field] = value;
            let request: CreateResponseRequest = serde_json::from_value(raw).unwrap();
            assert!(
                request
                    .to_chat_completion()
                    .unwrap_err()
                    .to_string()
                    .contains(field),
                "{field}"
            );
        }
    }

    #[test]
    fn translates_text_images_functions_and_tool_outputs() {
        let request: CreateResponseRequest = serde_json::from_value(json!({
            "model": "compatible-model",
            "instructions": "Be concise",
            "input": [
                {
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "What is shown?"},
                        {"type": "input_image", "image_url": "https://example.test/cat.png", "detail": "low"}
                    ]
                },
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"id\":1}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "cat"}
            ],
            "max_output_tokens": 100,
            "temperature": 0.2,
            "tools": [{
                "type": "function",
                "name": "lookup",
                "description": "Look up an object",
                "parameters": {"type": "object"},
                "strict": false
            }],
            "tool_choice": {"type": "function", "name": "lookup"},
            "text": {"format": {"type": "json_schema", "name": "answer", "schema": {"type": "object"}, "strict": true}}
        }))
        .unwrap();

        let chat = request.to_chat_completion().unwrap();
        assert_eq!(chat.model, "compatible-model");
        assert_eq!(chat.messages.len(), 4);
        assert_eq!(chat.messages[0].role, "system");
        assert_eq!(chat.messages[1].role, "user");
        assert_eq!(
            chat.messages[2].tool_calls.as_ref().unwrap()[0].id,
            "call_1"
        );
        assert_eq!(chat.messages[3].role, "tool");
        assert_eq!(chat.messages[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(chat.max_completion_tokens, Some(100));
        assert_eq!(chat.tools.as_ref().unwrap()[0].function.name, "lookup");
        assert_eq!(
            chat.tool_choice.unwrap(),
            json!({"type": "function", "function": {"name": "lookup"}})
        );
        assert_eq!(
            chat.response_format.unwrap(),
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "answer",
                    "schema": {"type": "object"},
                    "strict": true
                }
            })
        );
    }

    #[test]
    fn groups_adjacent_function_calls_into_one_assistant_message() {
        let request: CreateResponseRequest = serde_json::from_value(json!({
            "model": "model",
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "first", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_2", "name": "second", "arguments": "{}"}
            ]
        }))
        .unwrap();

        let chat = request.to_chat_completion().unwrap();
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].tool_calls.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn preserves_parallel_tool_calls() {
        for parallel_tool_calls in [true, false] {
            let request: CreateResponseRequest = serde_json::from_value(json!({
                "model": "model",
                "input": "hello",
                "parallel_tool_calls": parallel_tool_calls
            }))
            .unwrap();

            let chat = request.to_chat_completion().unwrap();
            assert_eq!(chat.parallel_tool_calls, Some(parallel_tool_calls));
        }
    }

    #[test]
    fn accepts_known_include_selectors_and_rejects_unknown_ones() {
        let known: CreateResponseRequest = serde_json::from_value(json!({
            "model": "model",
            "input": "hello",
            "include": KNOWN_RESPONSE_INCLUDE_SELECTORS
        }))
        .unwrap();
        assert!(known.to_chat_completion().is_ok());

        let unknown: CreateResponseRequest = serde_json::from_value(json!({
            "model": "model",
            "input": "hello",
            "include": ["future.supplemental_field"]
        }))
        .unwrap();
        assert_eq!(
            unknown.to_chat_completion().unwrap_err(),
            TranslationError::unsupported("include[0]")
        );
    }

    #[test]
    fn reasoning_effort_translates_and_other_reasoning_controls_are_rejected() {
        let reasoning: CreateResponseRequest = serde_json::from_value(json!({
            "model": "gpt-5-codex",
            "input": "hello",
            "reasoning": {"effort": "high", "summary": null}
        }))
        .unwrap();
        assert_eq!(
            reasoning
                .to_chat_completion()
                .unwrap()
                .reasoning_effort
                .as_deref(),
            Some("high")
        );

        let summary: CreateResponseRequest = serde_json::from_value(json!({
            "model": "gpt-5-codex",
            "input": "hello",
            "reasoning": {"effort": "high", "summary": "auto"}
        }))
        .unwrap();
        assert_eq!(
            summary.to_chat_completion().unwrap_err(),
            TranslationError::unsupported("reasoning.summary")
        );

        let not_a_string: CreateResponseRequest = serde_json::from_value(json!({
            "model": "gpt-5-codex",
            "input": "hello",
            "reasoning": {"effort": 3}
        }))
        .unwrap();
        assert!(not_a_string
            .to_chat_completion()
            .unwrap_err()
            .to_string()
            .contains("reasoning.effort"));
    }

    #[test]
    fn rejects_native_tools_instead_of_dropping_them() {
        let native_tool: CreateResponseRequest = serde_json::from_value(json!({
            "model": "gpt-5-codex",
            "input": "hello",
            "tools": [{"type": "web_search_preview"}]
        }))
        .unwrap();
        assert!(native_tool
            .to_chat_completion()
            .unwrap_err()
            .to_string()
            .contains("web_search_preview"));
    }

    #[test]
    fn rejects_file_images_and_encrypted_reasoning_items() {
        let file_image: CreateResponseRequest = serde_json::from_value(json!({
            "model": "model",
            "input": [{"role": "user", "content": [{"type": "input_image", "file_id": "file_1"}]}]
        }))
        .unwrap();
        assert!(file_image
            .to_chat_completion()
            .unwrap_err()
            .to_string()
            .contains("file_id"));

        let reasoning_item: CreateResponseRequest = serde_json::from_value(json!({
            "model": "model",
            "input": [{"type": "reasoning", "encrypted_content": "opaque"}]
        }))
        .unwrap();
        assert!(reasoning_item
            .to_chat_completion()
            .unwrap_err()
            .to_string()
            .contains("reasoning"));
    }

    #[test]
    fn translates_chat_text_tools_usage_and_incomplete_status() {
        let chat = ChatCompletionResponse {
            id: "chatcmpl_123".to_string(),
            object: "chat.completion".to_string(),
            created: 1234,
            model: "fallback-model".to_string(),
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: Some(Value::String("partial".to_string())),
                    name: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".to_string(),
                        tool_type: "function".to_string(),
                        function: FunctionCall {
                            name: "lookup".to_string(),
                            arguments: "{\"id\":1}".to_string(),
                        },
                    }]),
                    tool_call_id: None,
                },
                finish_reason: Some("length".to_string()),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: Some(4),
                total_tokens: 14,
                ..Default::default()
            }),
        };

        let response = CreateResponseResponse::from_chat_completion(chat).unwrap();
        assert_eq!(response.object, "response");
        assert_eq!(response.status, "incomplete");
        assert_eq!(
            response.incomplete_details,
            Some(json!({"reason": "max_output_tokens"}))
        );
        assert_eq!(response.output.len(), 2);
        assert_eq!(response.output[0].item_type.as_deref(), Some("message"));
        assert_eq!(
            response.output[1].item_type.as_deref(),
            Some("function_call")
        );
        assert_eq!(response.output[1].call_id.as_deref(), Some("call_1"));
        let usage = response.usage.unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 4);
        assert_eq!(usage.total_tokens, 14);
    }

    #[test]
    fn rejects_multiple_chat_choices() {
        let response = ChatCompletionResponse {
            id: "chatcmpl_123".to_string(),
            object: "chat.completion".to_string(),
            created: 1234,
            model: "model".to_string(),
            choices: Vec::new(),
            usage: None,
        };

        assert!(CreateResponseResponse::from_chat_completion(response)
            .unwrap_err()
            .to_string()
            .contains("exactly one"));
    }

    #[tokio::test]
    async fn synthesized_responses_stream_has_named_lifecycle_events() {
        let response = serde_json::json!({
            "id": "resp_123",
            "object": "response",
            "status": "completed",
            "output": [
                {
                    "id": "msg_123",
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hello", "annotations": []}]
                },
                {
                    "id": "fc_123",
                    "type": "function_call",
                    "status": "completed",
                    "call_id": "call_123",
                    "name": "lookup",
                    "arguments": "{\"id\":1}"
                }
            ],
        });
        let mut stream = synthesize_response_stream(response).unwrap();
        let mut body = String::new();
        while let Some(chunk) = stream.next().await {
            body.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }

        let events = body
            .lines()
            .filter_map(|line| line.strip_prefix("event: "))
            .collect::<Vec<_>>();
        assert_eq!(
            events,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let sequence_numbers = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| {
                serde_json::from_str::<serde_json::Value>(data).unwrap()["sequence_number"]
                    .as_u64()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            sequence_numbers,
            (0..events.len() as u64).collect::<Vec<_>>()
        );
        assert!(body.contains("\"id\":\"msg_123\""));
        assert!(body.contains("\"id\":\"fc_123\""));
        assert!(!body.contains("[DONE]"));
    }
}
