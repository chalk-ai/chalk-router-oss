//! Classification views of conversation requests. These never replace the
//! original request used for capability checks or provider dispatch.

use serde::Serialize;
use serde_json::{json, Value};

use crate::responses::ResponsesRequest;
use crate::route_grid::HeuristicJudgeBackend;
use crate::types::{ChatCompletionRequest, ChatMessage};

#[derive(Clone, Serialize)]
pub(crate) struct JudgeInput {
    pub model: String,
    surface: &'static str,
    request: Value,
}

impl JudgeInput {
    pub fn chat(request: &ChatCompletionRequest) -> Self {
        Self {
            model: request.model.clone(),
            surface: "chat_completions",
            request: serde_json::to_value(request).expect("chat request is JSON serializable"),
        }
    }

    pub fn messages(model: &str, request: &Value) -> Self {
        Self {
            model: model.into(),
            surface: "messages",
            request: request.clone(),
        }
    }

    pub fn responses(request: &ResponsesRequest) -> Self {
        Self {
            model: request.model().into(),
            surface: "responses",
            request: request.raw().clone(),
        }
    }

    /// Stored history and reusable prompts are not available to the classifier.
    /// Preserve normal route/continuation handling instead of judging a fragment.
    pub fn can_classify(&self) -> bool {
        self.surface != "responses"
            || !["previous_response_id", "conversation", "prompt"]
                .iter()
                .any(|field| self.request.get(field).is_some_and(|v| !v.is_null()))
    }

    pub fn looks_hard(&self, config: &HeuristicJudgeBackend) -> bool {
        if self.surface == "chat_completions" {
            let messages: Vec<ChatMessage> =
                serde_json::from_value(self.request["messages"].clone()).unwrap_or_default();
            return super::looks_hard_with_config(&messages, config);
        }
        let mut messages = Vec::new();
        let instructions = if self.surface == "messages" {
            "system"
        } else {
            "instructions"
        };
        if let Some(value) = self.request.get(instructions) {
            messages.push(text_message("system", value));
        }
        let input = if self.surface == "messages" {
            "messages"
        } else {
            "input"
        };
        match self.request.get(input) {
            Some(Value::Array(items)) => {
                for item in items {
                    let role =
                        item.get("role").and_then(Value::as_str).unwrap_or_else(|| {
                            match item.get("type").and_then(Value::as_str) {
                                Some("function_call") => "assistant",
                                Some("function_call_output") => "tool",
                                _ => "user",
                            }
                        });
                    messages.push(text_message(role, item));
                }
            }
            Some(value) => messages.push(text_message("user", value)),
            None => {}
        }
        super::looks_hard_with_config(&messages, config)
    }

    /// Keep native conversation/tool/output shapes for remote classification;
    /// no lossy conversion through a provider's Chat compatibility codec.
    pub fn prompt(&self) -> Value {
        let mut budget = 24_000;
        let mut content = serde_json::Map::new();
        for field in [
            "system",
            "instructions",
            "messages",
            "input",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "response_format",
            "text",
            "thinking",
            "reasoning",
            "output_config",
        ] {
            if let Some(value) = self.request.get(field) {
                let value = if matches!(field, "messages" | "input") {
                    conversation_tail(value)
                } else {
                    value.clone()
                };
                content.insert(field.into(), bounded_value(&value, &mut budget, 0));
            }
        }
        json!({"surface": self.surface, "request": content})
    }
}

fn text_message(role: &str, value: &Value) -> ChatMessage {
    fn text(value: &Value, output: &mut String) {
        match value {
            Value::String(s) => {
                output.push_str(s);
                output.push(' ');
            }
            Value::Array(items) => items.iter().for_each(|item| text(item, output)),
            Value::Object(object) => {
                for field in ["text", "content", "output", "arguments"] {
                    if let Some(value) = object.get(field) {
                        text(value, output);
                    }
                }
            }
            _ => {}
        }
    }
    let mut content = String::new();
    text(value, &mut content);
    ChatMessage {
        role: role.into(),
        content: Some(Value::String(content)),
        name: None,
        tool_calls: None,
        tool_call_id: None,
    }
}

fn conversation_tail(value: &Value) -> Value {
    let Some(items) = value.as_array() else {
        return value.clone();
    };
    let start = items.len().saturating_sub(super::DEFAULT_JUDGE_MAX_TURNS);
    let mut selected = Vec::new();
    // Preserve leading instructions when clipping Chat history.
    if start > 0
        && items.first().is_some_and(|item| {
            matches!(
                item.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
        })
    {
        selected.push(items[0].clone());
    }
    selected.extend_from_slice(&items[start..]);
    Value::Array(selected)
}

fn bounded_value(value: &Value, budget: &mut usize, depth: usize) -> Value {
    if *budget == 0 || depth >= 16 {
        return Value::String("[truncated]".into());
    }
    *budget -= 1;
    match value {
        Value::String(s) => {
            let clipped: String = s
                .chars()
                .take((*budget).min(super::DEFAULT_JUDGE_MAX_CHARS_PER_MSG))
                .collect();
            *budget = budget.saturating_sub(clipped.chars().count());
            Value::String(clipped)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(32)
                .map(|v| bounded_value(v, budget, depth + 1))
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .take(32)
                .map(|(key, value)| {
                    let clipped = if matches!(key.as_str(), "data" | "encrypted_content") {
                        Value::String("[omitted]".into())
                    } else {
                        bounded_value(value, budget, depth + 1)
                    };
                    (key.chars().take(128).collect(), clipped)
                })
                .collect(),
        ),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_text_blocks_and_response_items_reach_the_local_heuristic() {
        let messages = JudgeInput::messages(
            "route",
            &json!({"messages":[
                {"role":"user","content":[{"type":"text","text":"debug this algorithm"}]}
            ]}),
        );
        assert!(messages.looks_hard(&HeuristicJudgeBackend::default()));
        let responses: ResponsesRequest = serde_json::from_value(json!({"model":"route","input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"prove this theorem"}]}
        ]})).unwrap();
        assert!(JudgeInput::responses(&responses).looks_hard(&HeuristicJudgeBackend::default()));
        let responses: ResponsesRequest =
            serde_json::from_value(json!({"model":"route","input":"hello"})).unwrap();
        assert!(!JudgeInput::responses(&responses).looks_hard(&HeuristicJudgeBackend::default()));
    }

    #[test]
    fn classification_view_bounds_native_payloads_without_mutating_them() {
        let text = "🦀".repeat(20_000);
        let body = json!({"system":"keep this instruction", "messages":[{"role":"user","content":[
            {"type":"text","text":text},
            {"type":"image","source":{"type":"base64","data":"A".repeat(100_000),"media_type":"image/png"}}
        ]}]});
        let input = JudgeInput::messages("route", &body);
        let prompt = input.prompt();
        assert_eq!(prompt["request"]["system"], "keep this instruction");
        assert_eq!(
            prompt["request"]["messages"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            super::super::DEFAULT_JUDGE_MAX_CHARS_PER_MSG
        );
        assert_eq!(
            prompt["request"]["messages"][0]["content"][1]["source"]["data"],
            "[omitted]"
        );
        assert!(prompt.to_string().len() < 12_000);
        assert_eq!(input.request, body);
    }

    #[test]
    fn clipped_conversation_keeps_instructions_and_latest_turn() {
        let mut messages = vec![json!({"role":"system","content":"instructions"})];
        messages.extend((0..20).map(|i| json!({"role":"user","content":i.to_string()})));
        let chat: ChatCompletionRequest =
            serde_json::from_value(json!({"model":"route","messages":messages})).unwrap();
        let prompt = JudgeInput::chat(&chat).prompt();
        let messages = prompt["request"]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 9);
        assert_eq!(messages[0]["content"], "instructions");
        assert_eq!(messages.last().unwrap()["content"], "19");
    }
}
