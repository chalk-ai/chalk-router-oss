//! Validate the Chat controls and message shapes each translated backend can retain.

use serde_json::Value;

use crate::{backend::BackendError, types::ChatCompletionRequest};

fn unsupported(path: impl AsRef<str>) -> BackendError {
    BackendError::api_status(
        400,
        format!(
            "this translated Chat route cannot represent {} losslessly",
            path.as_ref()
        ),
    )
}

fn reject_extra(value: &Value, allowed: &[&str], path: &str) -> Result<(), BackendError> {
    let object = value.as_object().ok_or_else(|| unsupported(path))?;
    if let Some((name, _)) = object
        .iter()
        .find(|(name, value)| !allowed.contains(&name.as_str()) && !value.is_null())
    {
        return Err(unsupported(format!("{path}.{name}")));
    }
    Ok(())
}

/// Tool-call arguments are a model-written string and OpenAI clients replay
/// them verbatim, including empty or truncated ones. Both backends need a JSON
/// object for the tool input, so anything else becomes an empty object.
pub(crate) fn tool_call_input(arguments: &str) -> Value {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Default::default()))
}

fn validate_common(request: &ChatCompletionRequest) -> Result<(), BackendError> {
    if request.n.is_some_and(|n| n != 1) {
        return Err(unsupported("n"));
    }
    for (name, value) in [
        ("presence_penalty", request.presence_penalty),
        ("frequency_penalty", request.frequency_penalty),
    ] {
        if value.is_some_and(|value| value != 0.0) {
            return Err(unsupported(name));
        }
    }
    if request.logit_bias.as_ref().is_some_and(|bias| {
        !bias.is_null() && !bias.as_object().is_some_and(|object| object.is_empty())
    }) {
        return Err(unsupported("logit_bias"));
    }
    for (index, tool) in request
        .tools
        .as_deref()
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        if tool.tool_type != "function" || tool.function.name.is_empty() {
            return Err(unsupported(format!("tools[{index}]")));
        }
        if tool
            .function
            .parameters
            .as_ref()
            .is_some_and(|schema| !schema.is_object())
        {
            return Err(unsupported(format!("tools[{index}].function.parameters")));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Anthropic,
    Converse,
}

fn validate_messages(request: &ChatCompletionRequest, target: Target) -> Result<(), BackendError> {
    for (index, message) in request.messages.iter().enumerate() {
        let path = format!("messages[{index}]");
        if !matches!(
            message.role.as_str(),
            "system" | "developer" | "user" | "assistant" | "tool"
        ) {
            return Err(unsupported(format!("{path}.role")));
        }
        if message.name.is_some() {
            return Err(unsupported(format!("{path}.name")));
        }
        if message.role == "tool" {
            if !message
                .tool_call_id
                .as_ref()
                .is_some_and(|id| !id.is_empty())
            {
                return Err(unsupported(format!("{path}.tool_call_id")));
            }
        } else if message.tool_call_id.is_some() {
            return Err(unsupported(format!("{path}.tool_call_id")));
        }
        let calls = message.tool_calls.as_deref().unwrap_or_default();
        if !calls.is_empty() && message.role != "assistant" {
            return Err(unsupported(format!("{path}.tool_calls")));
        }
        for (call_index, call) in calls.iter().enumerate() {
            if call.tool_type != "function" || call.id.is_empty() || call.function.name.is_empty() {
                return Err(unsupported(format!("{path}.tool_calls[{call_index}]")));
            }
        }
        match message.content.as_ref() {
            None | Some(Value::Null) if message.role == "assistant" => {}
            Some(Value::String(_)) => {}
            Some(Value::Array(parts))
                if target == Target::Anthropic
                    || !matches!(message.role.as_str(), "system" | "developer" | "tool") =>
            {
                for (part_index, part) in parts.iter().enumerate() {
                    let part_path = format!("{path}.content[{part_index}]");
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") if part.get("text").is_some_and(Value::is_string) => {
                            reject_extra(part, &["type", "text"], &part_path)?;
                        }
                        Some("image_url")
                            if target == Target::Converse && message.role == "user" =>
                        {
                            reject_extra(part, &["type", "image_url"], &part_path)?;
                            let image = &part["image_url"];
                            reject_extra(
                                image,
                                &["url", "detail"],
                                &format!("{part_path}.image_url"),
                            )?;
                            if !image.get("url").is_some_and(Value::is_string)
                                || image
                                    .get("detail")
                                    .is_some_and(|detail| !detail.is_null() && detail != "auto")
                            {
                                return Err(unsupported(format!("{part_path}.image_url")));
                            }
                        }
                        _ => return Err(unsupported(part_path)),
                    }
                }
            }
            _ => return Err(unsupported(format!("{path}.content"))),
        }
    }
    Ok(())
}

fn has_tools(request: &ChatCompletionRequest) -> bool {
    request
        .tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty())
}

pub(crate) fn validate_converse_chat_request(
    request: &ChatCompletionRequest,
) -> Result<(), BackendError> {
    validate_common(request)?;
    validate_messages(request, Target::Converse)?;
    if request.parallel_tool_calls == Some(false) && has_tools(request) {
        return Err(unsupported("parallel_tool_calls"));
    }
    // Converse has no reasoning control the adapter maps; refuse rather than
    // drop it, including explicit `none`, which must disable reasoning.
    if request.reasoning_effort.is_some() {
        return Err(unsupported("reasoning_effort"));
    }
    if let Some(choice) = request
        .tool_choice
        .as_ref()
        .filter(|choice| !choice.is_null())
    {
        if choice != "auto" && !(choice == "none" && !has_tools(request)) {
            return Err(unsupported("tool_choice"));
        }
    }
    if let Some(format) = request
        .response_format
        .as_ref()
        .filter(|format| !format.is_null())
    {
        reject_extra(format, &["type"], "response_format")?;
        if format.get("type").and_then(Value::as_str) != Some("text") {
            return Err(unsupported("response_format"));
        }
    }
    Ok(())
}

pub(crate) fn validate_anthropic_chat_request(
    request: &ChatCompletionRequest,
) -> Result<(), BackendError> {
    validate_common(request)?;
    validate_messages(request, Target::Anthropic)?;
    if let Some(choice) = request
        .tool_choice
        .as_ref()
        .filter(|choice| !choice.is_null())
    {
        match choice {
            Value::String(mode) if matches!(mode.as_str(), "auto" | "none") => {}
            Value::String(mode) if mode == "required" && has_tools(request) => {}
            Value::Object(_) if has_tools(request) && choice["type"] == "function" => {
                reject_extra(choice, &["type", "function"], "tool_choice")?;
                reject_extra(&choice["function"], &["name"], "tool_choice.function")?;
                if !choice["function"]
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| !name.is_empty())
                {
                    return Err(unsupported("tool_choice.function.name"));
                }
            }
            _ => return Err(unsupported("tool_choice")),
        }
    }
    if let Some(format) = request
        .response_format
        .as_ref()
        .filter(|format| !format.is_null())
    {
        match format.get("type").and_then(Value::as_str) {
            Some("text" | "json_object") => reject_extra(format, &["type"], "response_format")?,
            Some("json_schema") => {
                reject_extra(format, &["type", "json_schema"], "response_format")?;
                let schema = &format["json_schema"];
                reject_extra(
                    schema,
                    &["name", "schema", "strict", "description"],
                    "response_format.json_schema",
                )?;
                if !schema.get("schema").is_some_and(Value::is_object)
                    || !schema
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| !name.is_empty())
                {
                    return Err(unsupported("response_format.json_schema"));
                }
                for (field, valid) in [
                    (
                        "strict",
                        schema
                            .get("strict")
                            .is_none_or(|value| value.is_null() || value.is_boolean()),
                    ),
                    (
                        "description",
                        schema
                            .get("description")
                            .is_none_or(|value| value.is_null() || value.is_string()),
                    ),
                ] {
                    if !valid {
                        return Err(unsupported(format!("response_format.json_schema.{field}")));
                    }
                }
            }
            _ => return Err(unsupported("response_format")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(options: Value) -> ChatCompletionRequest {
        let mut body = json!({"model":"test","messages":[{"role":"user","content":"hello"}]});
        body.as_object_mut()
            .unwrap()
            .extend(options.as_object().unwrap().clone());
        serde_json::from_slice(&serde_json::to_vec(&body).unwrap()).unwrap()
    }

    #[test]
    fn translated_chat_rejects_modeled_controls_that_neither_backend_can_retain() {
        for (field, value) in [
            ("n", json!(2)),
            ("n", json!(0)),
            ("presence_penalty", json!(0.5)),
            ("frequency_penalty", json!(-0.5)),
            ("logit_bias", json!({"1":10})),
            ("logit_bias", json!([])),
        ] {
            let request = request(json!({field:value}));
            for validate in [
                validate_anthropic_chat_request,
                validate_converse_chat_request,
            ] {
                let error = validate(&request).unwrap_err();
                assert!(matches!(&error, BackendError::ApiError { status: 400, .. }));
                assert!(error.to_string().contains(field));
            }
        }
    }

    #[test]
    fn converse_refuses_reasoning_effort_that_anthropic_maps() {
        // Anthropic maps `reasoning_effort` onto thinking; Converse has no
        // equivalent in the adapter, so it must refuse rather than drop it.
        for effort in ["low", "high", "minimal", "none"] {
            let request = request(json!({"reasoning_effort": effort}));
            validate_anthropic_chat_request(&request).unwrap();
            let error = validate_converse_chat_request(&request).unwrap_err();
            assert!(error.to_string().contains("reasoning_effort"), "{error}");
        }
        // Omitted effort keeps the default; explicit `none` needs a real mapping.
        validate_converse_chat_request(&request(json!({}))).unwrap();
    }

    #[test]
    fn translated_chat_keeps_valid_noop_defaults_and_supported_token_controls() {
        let request = request(
            json!({"n":1,"presence_penalty":0,"frequency_penalty":-0.0,"logit_bias":{},"user":"","tools":[],"tool_choice":"none","parallel_tool_calls":false,"response_format":{"type":"text"},"max_completion_tokens":17,"temperature":0.3,"top_p":0.8,"stop":["END"]}),
        );
        validate_anthropic_chat_request(&request).unwrap();
        validate_converse_chat_request(&request).unwrap();
    }

    #[test]
    fn anthropic_keeps_supported_tool_choices_parallel_control_and_raw_schema() {
        for choice in [
            json!("auto"),
            json!("none"),
            json!("required"),
            json!({"type":"function","function":{"name":"lookup"}}),
        ] {
            let request = request(
                json!({"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}],"tool_choice":choice,"parallel_tool_calls":false,"response_format":{"type":"json_schema","json_schema":{"name":"reply","schema":{"type":"object"},"strict":true}}}),
            );
            validate_anthropic_chat_request(&request).unwrap();
            assert!(validate_converse_chat_request(&request).is_err());
        }
        validate_anthropic_chat_request(&request(
            json!({"response_format":{"type":"json_object"}}),
        ))
        .unwrap();
    }

    #[test]
    fn translated_chat_rejects_invalid_selectors_and_formats_instead_of_defaulting() {
        for options in [
            json!({"tool_choice":"unknown"}),
            json!({"tool_choice":{"type":"function","function":{}}}),
            json!({"tool_choice":"required"}),
            json!({"response_format":{"type":"unknown"}}),
            json!({"response_format":{"type":"json_schema","json_schema":{"name":"reply"}}}),
            json!({"response_format":{"type":"text","future":true}}),
        ] {
            let request = request(options);
            assert!(validate_anthropic_chat_request(&request).is_err());
            assert!(validate_converse_chat_request(&request).is_err());
        }
    }

    #[test]
    fn translated_chat_rejects_lost_message_and_tool_semantics() {
        for message in [
            json!({"role":"user","content":"hello","name":"speaker"}),
            json!({"role":"user","content":[{"type":"input_audio","input_audio":{"data":"audio"}}]}),
            json!({"role":"user","content":[{"type":"text","text":"hello","future":true}]}),
        ] {
            let request = request(json!({"messages":[message]}));
            assert!(validate_anthropic_chat_request(&request).is_err());
            assert!(validate_converse_chat_request(&request).is_err());
        }
        let request = request(
            json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,aGk="}}]}]}),
        );
        assert!(validate_anthropic_chat_request(&request).is_err());
        validate_converse_chat_request(&request).unwrap();
    }

    #[test]
    fn translated_chat_accepts_developer_messages_user_ids_and_replayed_tool_arguments() {
        for arguments in ["", "not-json", "[1]", r#"{"path":"/tmp"}"#] {
            let request = request(json!({
                "user":"tracking-id",
                "messages":[
                    {"role":"developer","content":"system instruction"},
                    {"role":"assistant","tool_calls":[{"id":"call","type":"function","function":{"name":"lookup","arguments":arguments}}]},
                    {"role":"tool","tool_call_id":"call","content":"ok"},
                ]
            }));
            validate_anthropic_chat_request(&request).unwrap();
            validate_converse_chat_request(&request).unwrap();
        }
    }

    #[test]
    fn tool_call_input_keeps_objects_and_defaults_everything_else() {
        assert_eq!(tool_call_input(r#"{"a":1}"#), json!({"a":1}));
        for arguments in ["", "not-json", "[1]", "3", "null", r#""text""#] {
            assert_eq!(tool_call_input(arguments), json!({}), "{arguments}");
        }
    }
}
