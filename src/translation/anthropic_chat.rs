//! Strict compatibility codec between Anthropic Messages and OpenAI Chat.
//!
//! This is intentionally a documented subset. Any field or content block that
//! cannot be represented faithfully is rejected before an upstream call.
//!
//! A few fields are accepted (never rejected, since real Anthropic clients
//! send them routinely) but cannot be carried through to an OpenAI-style
//! backend, and are silently dropped with a one-line `tracing::debug!` per
//! request rather than a hard error:
//!
//! - `cache_control` on system blocks, content blocks, and tool definitions:
//!   OpenAI's chat-completions request has no prompt-caching-hint mechanism,
//!   so the hint is dropped.
//! - `tool_result.is_error`: `ChatCompletionRequest`'s `tool` message has no
//!   analogous flag; the tool result text is still forwarded, just without
//!   the error marker.
//!
//! On the response side, `stop_reason` never comes back as `"stop_sequence"`:
//! `types::ChatChoice::finish_reason` only carries OpenAI's finish-reason
//! enum (`"stop"`, `"length"`, `"tool_calls"`, ...), which collapses "the
//! model hit a caller-supplied stop sequence" and "the model finished
//! naturally" into the same `"stop"` value and never exposes which string
//! (if any) matched. Since Anthropic's `stop_sequence` field must be either
//! null or the exact matched string, guessing would be worse than the
//! current `"end_turn"` mapping, so `"stop"` keeps mapping to `"end_turn"`
//! with `stop_sequence` left `null` until a backend surfaces the matched
//! text.

use bytes::Bytes;
use serde_json::{json, Value};

use crate::backend::BackendError;
use crate::types::{
    ChatCompletionRequest, ChatCompletionResponse, ChatMessage, FunctionCall, Tool, ToolCall,
    ToolFunction,
};

const ALLOWED_FIELDS: &[&str] = &[
    "model",
    "messages",
    "max_tokens",
    "system",
    "temperature",
    "top_p",
    "stop_sequences",
    "tools",
    "tool_choice",
    "stream",
];

fn bad_request(message: impl Into<String>) -> BackendError {
    BackendError::api_status(400, message.into())
}

fn text_blocks(value: &Value, context: &str) -> Result<String, BackendError> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Array(blocks) => {
            let mut text = Vec::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("text") {
                    return Err(bad_request(format!(
                        "{context} contains a block that cannot be translated"
                    )));
                }
                text.push(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| bad_request(format!("{context} text block requires text")))?
                        .to_string(),
                );
            }
            Ok(text.join("\n"))
        }
        _ => Err(bad_request(format!(
            "{context} must be a string or text block array"
        ))),
    }
}

fn image_part(block: &Value) -> Result<Value, BackendError> {
    let source = block
        .get("source")
        .and_then(Value::as_object)
        .ok_or_else(|| bad_request("Anthropic image block requires source"))?;
    let url = match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .ok_or_else(|| bad_request("base64 image source requires media_type"))?;
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| bad_request("base64 image source requires data"))?;
            format!("data:{media_type};base64,{data}")
        }
        Some("url") => source
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| bad_request("URL image source requires url"))?
            .to_string(),
        Some(kind) => {
            return Err(bad_request(format!(
                "unsupported image source type '{kind}'"
            )))
        }
        None => return Err(bad_request("image source requires type")),
    };
    Ok(json!({"type": "image_url", "image_url": {"url": url}}))
}

fn flush_user_parts(parts: &mut Vec<Value>, messages: &mut Vec<ChatMessage>) {
    if parts.is_empty() {
        return;
    }
    let content =
        if parts.len() == 1 && parts[0].get("type").and_then(Value::as_str) == Some("text") {
            parts[0].get("text").cloned()
        } else {
            Some(Value::Array(std::mem::take(parts)))
        };
    messages.push(ChatMessage {
        role: "user".to_string(),
        content,
        name: None,
        tool_calls: None,
        tool_call_id: None,
    });
    parts.clear();
}

fn convert_messages(raw: &Value) -> Result<Vec<ChatMessage>, BackendError> {
    let raw = raw
        .as_array()
        .ok_or_else(|| bad_request("Anthropic messages must be an array"))?;
    let mut messages = Vec::new();
    for message in raw {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| bad_request("Anthropic message requires role"))?;
        let content = message
            .get("content")
            .ok_or_else(|| bad_request("Anthropic message requires content"))?;
        if let Value::String(text) = content {
            messages.push(ChatMessage {
                role: role.to_string(),
                content: Some(Value::String(text.clone())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            });
            continue;
        }
        let blocks = content
            .as_array()
            .ok_or_else(|| bad_request("Anthropic message content must be a string or array"))?;
        match role {
            "user" => {
                let mut parts = Vec::new();
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => parts.push(json!({
                            "type": "text",
                            "text": block.get("text").and_then(Value::as_str)
                                .ok_or_else(|| bad_request("text block requires text"))?
                        })),
                        Some("image") => parts.push(image_part(block)?),
                        Some("tool_result") => {
                            flush_user_parts(&mut parts, &mut messages);
                            let tool_call_id = block
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .ok_or_else(|| bad_request("tool_result requires tool_use_id"))?;
                            let result = text_blocks(
                                block
                                    .get("content")
                                    .unwrap_or(&Value::String(String::new())),
                                "tool_result content",
                            )?;
                            messages.push(ChatMessage {
                                role: "tool".to_string(),
                                content: Some(Value::String(result)),
                                name: None,
                                tool_calls: None,
                                tool_call_id: Some(tool_call_id.to_string()),
                            });
                        }
                        Some(kind) => {
                            return Err(bad_request(format!(
                                "Anthropic content block '{kind}' cannot be translated"
                            )))
                        }
                        None => return Err(bad_request("Anthropic content block requires type")),
                    }
                }
                flush_user_parts(&mut parts, &mut messages);
            }
            "assistant" => {
                let mut text = Vec::new();
                let mut tool_calls = Vec::new();
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => text.push(
                            block
                                .get("text")
                                .and_then(Value::as_str)
                                .ok_or_else(|| bad_request("text block requires text"))?
                                .to_string(),
                        ),
                        Some("tool_use") => tool_calls.push(ToolCall {
                            id: block
                                .get("id")
                                .and_then(Value::as_str)
                                .ok_or_else(|| bad_request("tool_use requires id"))?
                                .to_string(),
                            tool_type: "function".to_string(),
                            function: FunctionCall {
                                name: block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| bad_request("tool_use requires name"))?
                                    .to_string(),
                                arguments: serde_json::to_string(
                                    block.get("input").unwrap_or(&json!({})),
                                )
                                .map_err(|error| bad_request(error.to_string()))?,
                            },
                        }),
                        Some(kind) => {
                            return Err(bad_request(format!(
                                "Anthropic assistant block '{kind}' cannot be translated"
                            )))
                        }
                        None => return Err(bad_request("Anthropic content block requires type")),
                    }
                }
                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: (!text.is_empty()).then(|| Value::String(text.join("\n"))),
                    name: None,
                    tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                    tool_call_id: None,
                });
            }
            other => return Err(bad_request(format!("unsupported Anthropic role '{other}'"))),
        }
    }
    Ok(messages)
}

fn convert_tools(
    value: Option<crate::schema_json::SchemaJson>,
) -> Result<Option<Vec<Tool>>, BackendError> {
    let Some(tools) = value else {
        return Ok(None);
    };
    if !tools.is_array() {
        return Err(bad_request("Anthropic tools must be an array"));
    }
    let tools = tools.elements().map_err(|error| {
        BackendError::Other(format!(
            "failed to index validated Anthropic tools: {error}"
        ))
    })?;
    tools
        .iter()
        .map(|tool| {
            Ok(Tool {
                tool_type: "function".to_string(),
                function: ToolFunction {
                    name: tool
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| bad_request("Anthropic tool requires name"))?
                        .to_string(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    parameters: Some(
                        tool.member("input_schema")
                            .map_err(|error| {
                                bad_request(format!("invalid Anthropic tool: {error}"))
                            })?
                            .ok_or_else(|| bad_request("Anthropic tool requires input_schema"))?,
                    ),
                },
            })
        })
        .collect::<Result<Vec<_>, BackendError>>()
        .map(Some)
}

fn convert_tool_choice(value: Option<&Value>) -> Result<Option<Value>, BackendError> {
    let Some(value) = value else { return Ok(None) };
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_request("Anthropic tool_choice requires type"))?;
    match kind {
        "auto" => Ok(Some(Value::String("auto".to_string()))),
        "any" => Ok(Some(Value::String("required".to_string()))),
        "none" => Ok(Some(Value::String("none".to_string()))),
        "tool" => Ok(Some(json!({
            "type": "function",
            "function": {"name": value.get("name").and_then(Value::as_str)
                .ok_or_else(|| bad_request("tool tool_choice requires name"))?}
        }))),
        other => Err(bad_request(format!(
            "unsupported tool_choice type '{other}'"
        ))),
    }
}

/// Reads `tool_choice.disable_parallel_tool_use`, defaulting to `false` when
/// absent or not a boolean. Maps to OpenAI's `parallel_tool_calls: false`.
fn disable_parallel_tool_use(tool_choice: Option<&Value>) -> bool {
    tool_choice
        .and_then(|value| value.get("disable_parallel_tool_use"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Recursively searches a JSON value for an object key at any depth. Used to
/// log (once per request) that an accepted-but-unrepresentable field was
/// present, without threading a flag through every block-conversion helper.
fn contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => map.contains_key(key) || map.values().any(|v| contains_key(v, key)),
        Value::Array(items) => items.iter().any(|v| contains_key(v, key)),
        _ => false,
    }
}

/// Logs the accepted-but-dropped fields documented in the module doc, once
/// per request, so operators can see how often real clients rely on them.
fn log_dropped_fields(value: &Value) {
    if contains_key(value, "cache_control") {
        tracing::debug!(
            "Anthropic request used cache_control; dropped translating to an OpenAI-compatible backend (no equivalent field)"
        );
    }
    if contains_key(value, "is_error") {
        tracing::debug!(
            "Anthropic tool_result.is_error dropped translating to an OpenAI-compatible backend (no equivalent field)"
        );
    }
}

fn request_from_value(
    value: &crate::schema_json::SchemaJson,
) -> Result<(ChatCompletionRequest, bool), BackendError> {
    let object = value
        .as_object()
        .ok_or_else(|| bad_request("Anthropic request body must be a JSON object"))?;
    if let Some(field) = object
        .keys()
        .find(|field| !ALLOWED_FIELDS.contains(&field.as_str()))
    {
        return Err(bad_request(format!(
            "Anthropic field '{field}' cannot be translated without changing semantics"
        )));
    }
    log_dropped_fields(value);
    let mut messages = Vec::new();
    if let Some(system) = object.get("system") {
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: Some(Value::String(text_blocks(system, "system")?)),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        });
    }
    messages.extend(convert_messages(
        object
            .get("messages")
            .ok_or_else(|| bad_request("Anthropic request requires messages"))?,
    )?);
    let stream = object
        .get("stream")
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| bad_request("stream must be a boolean"))
        })
        .transpose()?
        .unwrap_or(false);
    let max_tokens = object
        .get("max_tokens")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0 && *value <= i32::MAX as i64)
        .ok_or_else(|| bad_request("max_tokens must be a positive integer"))?
        as i32;
    let number = |field: &str| -> Result<Option<f32>, BackendError> {
        object
            .get(field)
            .map(|value| {
                value
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .map(|value| value as f32)
                    .ok_or_else(|| bad_request(format!("{field} must be a finite number")))
            })
            .transpose()
    };
    Ok((
        ChatCompletionRequest {
            fallbacks: Default::default(),
            model: object
                .get("model")
                .and_then(Value::as_str)
                .ok_or_else(|| bad_request("Anthropic request requires model"))?
                .to_string(),
            messages,
            temperature: number("temperature")?,
            top_p: number("top_p")?,
            n: None,
            stream: Some(false),
            stop: object
                .get("stop_sequences")
                .map(|value| {
                    serde_json::from_value(value.clone())
                        .map_err(|_| bad_request("stop_sequences must be an array of strings"))
                })
                .transpose()?,
            max_tokens: Some(max_tokens),
            max_completion_tokens: None,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
            user: None,
            tools: convert_tools(if object.contains_key("tools") {
                value.member("tools").map_err(|error| {
                    BackendError::Other(format!(
                        "failed to index validated Anthropic request: {error}"
                    ))
                })?
            } else {
                None
            })?,
            tool_choice: convert_tool_choice(object.get("tool_choice"))?,
            response_format: None,
            stream_options: None,
            parallel_tool_calls: disable_parallel_tool_use(object.get("tool_choice"))
                .then_some(false),
            reasoning_effort: None,
        },
        stream,
    ))
}

/// Parse Anthropic JSON and return its translated Chat request.
pub(crate) fn translate_request(body: &[u8]) -> Result<ChatCompletionRequest, BackendError> {
    parse_request(body).map(|(request, _)| request)
}

/// Parse and validate Anthropic JSON, returning the Chat request and requested streaming mode.
/// Invalid JSON or unsupported content returns HTTP 400 before backend execution.
pub(crate) fn parse_request(body: &[u8]) -> Result<(ChatCompletionRequest, bool), BackendError> {
    let value: crate::schema_json::SchemaJson = serde_json::from_slice(body)
        .map_err(|error| bad_request(format!("invalid Anthropic request JSON: {error}")))?;
    request_from_value(&value)
}

fn stop_reason(reason: Option<&str>) -> Option<&'static str> {
    match reason {
        Some("length") => Some("max_tokens"),
        Some("tool_calls") | Some("function_call") => Some("tool_use"),
        Some("stop") => Some("end_turn"),
        Some(_) => Some("end_turn"),
        None => None,
    }
}

/// Convert the first Chat choice into an Anthropic reply; no choices returns HTTP 502.
pub(crate) fn response_value(response: ChatCompletionResponse) -> Result<Value, BackendError> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| BackendError::api_status(502, "Chat provider returned no choices"))?;
    let mut content = Vec::new();
    if let Some(value) = choice.message.content {
        let text = match value {
            Value::String(text) => text,
            other => text_blocks(&other, "Chat response content")?,
        };
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
    }
    if let Some(tool_calls) = choice.message.tool_calls {
        for tool in tool_calls {
            let input: Value = serde_json::from_str(&tool.function.arguments).map_err(|error| {
                BackendError::api_status(502, format!("Chat tool arguments are not JSON: {error}"))
            })?;
            content.push(json!({
                "type": "tool_use",
                "id": tool.id,
                "name": tool.function.name,
                "input": input
            }));
        }
    }
    let usage = response.usage.unwrap_or(crate::types::Usage {
        prompt_tokens: 0,
        completion_tokens: Some(0),
        total_tokens: 0,
        ..Default::default()
    });
    Ok(json!({
        "id": response.id,
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": response.model,
        "stop_reason": stop_reason(choice.finish_reason.as_deref()),
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": usage.prompt_tokens,
            "output_tokens": usage.completion_tokens.unwrap_or(0)
        }
    }))
}

fn sse_event(name: &str, value: Value, output: &mut String) {
    output.push_str("event: ");
    output.push_str(name);
    output.push_str("\ndata: ");
    output.push_str(&value.to_string());
    output.push_str("\n\n");
}

/// Encode a completed Anthropic reply as a synthesized SSE event sequence.
pub(crate) fn streaming_body(message: &Value) -> Bytes {
    let mut output = String::new();
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["usage"]["output_tokens"] = json!(0);
    sse_event(
        "message_start",
        json!({"type":"message_start", "message":start}),
        &mut output,
    );
    for (index, block) in message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let empty = match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => json!({
                "type":"tool_use", "id":block["id"], "name":block["name"], "input":{}
            }),
            _ => json!({"type":"text", "text":""}),
        };
        sse_event(
            "content_block_start",
            json!({
                "type":"content_block_start", "index":index, "content_block":empty
            }),
            &mut output,
        );
        let delta = if block.get("type").and_then(Value::as_str) == Some("tool_use") {
            json!({"type":"input_json_delta", "partial_json":block["input"].to_string()})
        } else {
            json!({"type":"text_delta", "text":block["text"]})
        };
        sse_event(
            "content_block_delta",
            json!({
                "type":"content_block_delta", "index":index, "delta":delta
            }),
            &mut output,
        );
        sse_event(
            "content_block_stop",
            json!({
                "type":"content_block_stop", "index":index
            }),
            &mut output,
        );
    }
    sse_event(
        "message_delta",
        json!({
            "type":"message_delta",
            "delta":{"stop_reason":message["stop_reason"], "stop_sequence":Value::Null},
            "usage":{"output_tokens":message["usage"]["output_tokens"]}
        }),
        &mut output,
    );
    sse_event("message_stop", json!({"type":"message_stop"}), &mut output);
    Bytes::from(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_wrong_type_has_a_clear_client_error_and_malformed_json_keeps_parse_detail() {
        for raw in ["null", "{}", "1", r#""tools""#] {
            let value = serde_json::from_str(raw).unwrap();
            let error = convert_tools(Some(value)).unwrap_err();
            assert!(
                matches!(error, BackendError::ApiError { status: 400, ref message, .. }
                if message == "Anthropic tools must be an array")
            );
        }
        let error = parse_request(br#"{"model":"model","max_tokens":64,"messages":[],"tools":[}"#)
            .unwrap_err();
        assert!(
            matches!(error, BackendError::ApiError { status: 400, ref message, .. }
            if message.contains("invalid Anthropic request JSON")
                && message.contains("line 1 column"))
        );
    }

    #[test]
    fn translates_system_images_tools_and_results() {
        let value = json!({
            "model":"model",
            "max_tokens":100,
            "system":[{"type":"text","text":"be useful"}],
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"weather","input":{"city":"SF"}}]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_1","content":"sunny"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}},
                    {"type":"text","text":"describe it"}
                ]}
            ],
            "tools":[{"name":"weather","input_schema":{"type":"object"}}]
        });
        let (request, streaming) = request_from_value(&value.into()).unwrap();
        assert!(!streaming);
        assert_eq!(request.messages[0].role, "system");
        assert_eq!(
            request.messages[1].tool_calls.as_ref().unwrap()[0].id,
            "call_1"
        );
        assert_eq!(request.messages[2].role, "tool");
        assert_eq!(request.messages[3].role, "user");
        assert!(request.messages[3]
            .content
            .as_ref()
            .unwrap()
            .to_string()
            .contains("data:image/png;base64,AA=="));
    }

    #[test]
    fn rejects_unknown_fields_and_blocks() {
        let unknown_field = json!({"model":"m","messages":[],"thinking":{}});
        assert!(request_from_value(&unknown_field.into()).is_err());
        let unknown_block = json!({
            "model":"m", "messages":[{"role":"user","content":[{"type":"document"}]}]
        });
        assert!(request_from_value(&unknown_block.into()).is_err());
    }

    #[test]
    fn synthesized_stream_has_anthropic_event_order() {
        let message = json!({
            "id":"msg_1", "type":"message", "role":"assistant", "content":[{"type":"text","text":"hi"}],
            "model":"m", "stop_reason":"end_turn", "stop_sequence":null,
            "usage":{"input_tokens":1,"output_tokens":2}
        });
        let body = String::from_utf8(streaming_body(&message).to_vec()).unwrap();
        let names = body
            .lines()
            .filter_map(|line| line.strip_prefix("event: "))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
    }

    #[test]
    fn cache_control_is_accepted_and_dropped() {
        let value = json!({
            "model":"model",
            "max_tokens":100,
            "system":[{"type":"text","text":"be useful","cache_control":{"type":"ephemeral"}}],
            "messages":[
                {"role":"user","content":[
                    {"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}
                ]}
            ],
            "tools":[{"name":"weather","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"}}]
        });
        assert!(contains_key(&value, "cache_control"));
        let (request, _) = request_from_value(&value.into()).unwrap();
        // Accepted without error; the hint has no OpenAI-side representation
        // and is not present anywhere on the translated request.
        assert_eq!(request.messages.len(), 2);
    }

    #[test]
    fn tool_result_is_error_is_accepted_and_dropped() {
        let value = json!({
            "model":"model",
            "max_tokens":100,
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"weather","input":{}}]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_1","content":"boom","is_error":true}
                ]}
            ]
        });
        assert!(contains_key(&value, "is_error"));
        let (request, _) = request_from_value(&value.into()).unwrap();
        let tool_message = &request.messages[1];
        assert_eq!(tool_message.role, "tool");
        // The failure text is preserved; only the is_error marker is dropped.
        assert!(tool_message
            .content
            .as_ref()
            .unwrap()
            .to_string()
            .contains("boom"));
    }

    #[test]
    fn disable_parallel_tool_use_maps_to_parallel_tool_calls_false() {
        let value = json!({
            "model":"model",
            "max_tokens":100,
            "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],
            "tools":[{"name":"weather","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"auto","disable_parallel_tool_use":true}
        });
        let (request, _) = request_from_value(&value.into()).unwrap();
        assert_eq!(request.tool_choice, Some(json!("auto")));
        assert_eq!(request.parallel_tool_calls, Some(false));
    }

    #[test]
    fn disable_parallel_tool_use_absent_leaves_parallel_tool_calls_unset() {
        let value = json!({
            "model":"model",
            "max_tokens":100,
            "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],
            "tools":[{"name":"weather","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"auto","disable_parallel_tool_use":false}
        });
        let (request, _) = request_from_value(&value.into()).unwrap();
        assert_eq!(request.parallel_tool_calls, None);

        let value_without_flag = json!({
            "model":"model",
            "max_tokens":100,
            "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],
            "tools":[{"name":"weather","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"auto"}
        });
        let (request, _) = request_from_value(&value_without_flag.into()).unwrap();
        assert_eq!(request.parallel_tool_calls, None);
    }

    #[test]
    fn stop_reason_never_reports_stop_sequence() {
        let response = ChatCompletionResponse {
            id: "resp_1".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "model".to_string(),
            choices: vec![crate::types::ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: Some(Value::String("hi".to_string())),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        };
        let message = response_value(response).unwrap();
        // A plain "stop" finish reason (which may or may not have matched a
        // caller-supplied stop sequence upstream) keeps mapping to
        // "end_turn" with a null stop_sequence, since finish_reason never
        // exposes which literal string (if any) matched.
        assert_eq!(message["stop_reason"], json!("end_turn"));
        assert_eq!(message["stop_sequence"], Value::Null);
    }

    #[test]
    fn assistant_text_and_tool_use_keep_within_category_order() {
        let value = json!({
            "model":"model",
            "max_tokens":100,
            "messages":[
                {"role":"assistant","content":[
                    {"type":"text","text":"first"},
                    {"type":"tool_use","id":"call_1","name":"a","input":{}},
                    {"type":"text","text":"second"},
                    {"type":"tool_use","id":"call_2","name":"b","input":{}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_1","content":"a-result"},
                    {"type":"tool_result","tool_use_id":"call_2","content":"b-result"}
                ]}
            ]
        });
        let (request, _) = request_from_value(&value.into()).unwrap();
        let assistant = &request.messages[0];
        assert_eq!(assistant.role, "assistant");
        // Text blocks are joined in their original order; OpenAI's
        // ChatMessage has no way to interleave text and tool_calls, so
        // within-category order is the strongest guarantee available.
        assert_eq!(
            assistant.content.as_ref().unwrap(),
            &Value::String("first\nsecond".to_string())
        );
        let tool_calls = assistant.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls[0].id, "call_1");
        assert_eq!(tool_calls[1].id, "call_2");
    }
}
