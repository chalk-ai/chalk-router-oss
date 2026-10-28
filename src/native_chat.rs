//! Native Chat bodies remain authoritative; typed views only observe them.
use std::{collections::BTreeMap, ops::Deref};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{value::RawValue, Value};

use crate::{backend::BackendError, types::ChatCompletionRequest};

#[derive(Debug, Clone)]
pub struct NativeChatRequest {
    fields: BTreeMap<String, Box<RawValue>>,
    view: ChatCompletionRequest,
    headers: Vec<(String, String)>,
}

impl Serialize for NativeChatRequest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.fields.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for NativeChatRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = crate::native_json::UniqueFields::deserialize(deserializer)?.0;
        for name in ["messages", "tools", "response_format", "stream_options"] {
            if let Some(raw) = fields.get(name) {
                crate::native_json::validate_unique_subtree(raw)
                    .map_err(serde::de::Error::custom)?;
            }
        }
        Self::from_fields(fields).map_err(serde::de::Error::custom)
    }
}

impl Deref for NativeChatRequest {
    type Target = ChatCompletionRequest;
    fn deref(&self) -> &Self::Target {
        &self.view
    }
}

impl NativeChatRequest {
    fn from_fields(mut fields: BTreeMap<String, Box<RawValue>>) -> Result<Self, String> {
        fn field<T: serde::de::DeserializeOwned>(
            fields: &BTreeMap<String, Box<RawValue>>,
            name: &str,
        ) -> Option<T> {
            serde_json::from_str(fields.get(name)?.get()).ok()
        }
        let model = field::<String>(&fields, "model").ok_or("model must be a string")?;
        // `fallbacks` is a router control, never part of the provider body.
        let fallbacks = match fields.remove("fallbacks") {
            Some(raw) if raw.get().trim() != "null" => serde_json::from_str(raw.get())
                .map_err(|error| format!("invalid fallbacks: {error}"))?,
            _ => Default::default(),
        };
        if fields.get("stream").is_some_and(|raw| {
            raw.get().trim() != "null" && serde_json::from_str::<bool>(raw.get()).is_err()
        }) {
            return Err("stream must be a boolean".into());
        }
        let messages =
            field::<Vec<Value>>(&fields, "messages").ok_or("messages must be an array")?;
        // A novel or malformed optional field must reach native validation. It
        // must never prevent observation of the remaining valid input.
        let messages = messages
            .into_iter()
            .map(|message| {
                serde_json::from_value(message.clone()).unwrap_or_else(|_| {
                    crate::types::ChatMessage {
                        role: message
                            .get("role")
                            .and_then(Value::as_str)
                            .unwrap_or("user")
                            .to_owned(),
                        content: message.get("content").cloned(),
                        name: None,
                        tool_calls: None,
                        tool_call_id: None,
                    }
                })
            })
            .collect();
        let view = ChatCompletionRequest {
            fallbacks,
            model,
            messages,
            temperature: field(&fields, "temperature"),
            top_p: field(&fields, "top_p"),
            n: field(&fields, "n"),
            stream: field(&fields, "stream"),
            stop: field(&fields, "stop"),
            max_tokens: field(&fields, "max_tokens"),
            max_completion_tokens: field(&fields, "max_completion_tokens"),
            presence_penalty: field(&fields, "presence_penalty"),
            frequency_penalty: field(&fields, "frequency_penalty"),
            logit_bias: field(&fields, "logit_bias"),
            user: field(&fields, "user"),
            tools: field(&fields, "tools"),
            tool_choice: field(&fields, "tool_choice"),
            response_format: field(&fields, "response_format"),
            stream_options: field(&fields, "stream_options"),
            parallel_tool_calls: field(&fields, "parallel_tool_calls"),
            reasoning_effort: field(&fields, "reasoning_effort"),
        };
        Ok(Self {
            fields,
            view,
            headers: Vec::new(),
        })
    }

    pub fn from_typed(request: ChatCompletionRequest) -> Result<Self, BackendError> {
        let fallbacks = request.fallbacks.clone();
        let mut native: Self = serde_json::from_slice(
            &serde_json::to_vec(&request).map_err(|e| BackendError::Other(e.to_string()))?,
        )
        .map_err(|e| BackendError::Other(e.to_string()))?;
        native.set_fallbacks(fallbacks);
        Ok(native)
    }

    pub fn set_fallbacks(&mut self, fallbacks: crate::route_grid::FallbackMap) {
        self.view.fallbacks = fallbacks;
    }

    /// Caller credentials are never forwarded. Beta flags can affect replies,
    /// so they participate in cache identity and exclude translated candidates.
    pub fn with_headers(mut self, headers: &axum::http::HeaderMap) -> Self {
        self.headers = headers
            .iter()
            .filter(|(name, _)| matches!(name.as_str(), "openai-beta" | "x-client-request-id"))
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        self
    }
    pub fn headers(&self) -> impl Iterator<Item = (&str, &str)> {
        self.headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub fn value(&self) -> Value {
        // Observational only. Never serialize this copy to the native provider.
        crate::native_json::observation_value(&self.fields)
    }

    pub fn set_field<T: Serialize>(&mut self, name: &str, value: &T) -> Result<(), BackendError> {
        self.fields.insert(
            name.to_owned(),
            serde_json::value::to_raw_value(value)
                .map_err(|e| BackendError::Other(e.to_string()))?,
        );
        Ok(())
    }

    pub fn set_model(&mut self, model: String) {
        self.set_field("model", &model).expect("model serializes");
        self.view.model = model;
    }

    pub fn set_stream(&mut self, stream: bool) {
        self.set_field("stream", &stream)
            .expect("stream serializes");
        self.view.stream = Some(stream);
    }

    pub fn translate_max_tokens(&mut self) {
        if self
            .fields
            .get("max_completion_tokens")
            .is_none_or(|raw| raw.get().trim() == "null")
        {
            if let Some(tokens) = self.fields.remove("max_tokens") {
                self.fields.insert("max_completion_tokens".into(), tokens);
                self.view.max_completion_tokens = self.view.max_tokens.take();
            }
        }
    }

    pub fn include_usage(&self) -> bool {
        self.fields
            .get("stream_options")
            .and_then(|raw| serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(raw.get()).ok())
            .and_then(|options| {
                options
                    .get("include_usage")
                    .and_then(|raw| serde_json::from_str::<bool>(raw.get()).ok())
            })
            == Some(true)
    }

    pub fn request_usage(&mut self) -> Result<(), BackendError> {
        let mut options = match self.fields.get("stream_options") {
            Some(raw) => match serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(raw.get()) {
                Ok(options) => options,
                // Preserve malformed input so the provider can reject it.
                Err(_) => return Ok(()),
            },
            None => BTreeMap::new(),
        };
        if options
            .get("include_usage")
            .is_some_and(|raw| serde_json::from_str::<bool>(raw.get()).is_err())
        {
            return Ok(());
        }
        options.insert(
            "include_usage".into(),
            serde_json::value::to_raw_value(&true).expect("boolean serializes"),
        );
        self.set_field("stream_options", &options)
    }

    pub fn compatibility_request(&self) -> Result<ChatCompletionRequest, BackendError> {
        if self.headers.iter().any(|(name, _)| name == "openai-beta") {
            return Err(BackendError::api_status(
                400,
                "this translated Chat route cannot forward OpenAI beta headers",
            ));
        }
        let represented: Value = serde_json::from_slice(
            &serde_json::to_vec(&self.view).map_err(|e| BackendError::Other(e.to_string()))?,
        )
        .map_err(|e| BackendError::Other(e.to_string()))?;
        for (name, raw) in &self.fields {
            let value: Value = serde_json::from_str(raw.get()).map_err(|_| {
                BackendError::api_status(
                    400,
                    format!("this translated Chat route cannot observe {name} losslessly"),
                )
            })?;
            if value.is_null() {
                continue;
            }
            // A null member is the absent member in OpenAI's JSON (its own
            // replies carry `"content": null` beside `tool_calls`, and SDK
            // clients append them verbatim), and the typed view omits absent
            // members, so compare with nulls dropped on both sides.
            if represented.get(name).map(without_nulls) != Some(without_nulls(&value)) {
                return Err(BackendError::api_status(
                    400,
                    format!("this translated Chat route cannot represent {name} losslessly"),
                ));
            }
        }
        Ok(self.view.clone())
    }

    /// Input a missing-usage estimate may trust: every field observed and
    /// every message part text.
    pub(crate) fn normalized_usage_input(
        &self,
    ) -> Option<crate::usage_estimator::NormalizedUsageInput> {
        let value = self.observed_value(false)?;
        let wire_bytes = crate::usage_estimator::serialized_json_bytes(self)?;
        crate::usage_estimator::NormalizedUsageInput::from_json(&value)
            .map(|input| input.with_minimum_wire_bytes(wire_bytes))
    }

    /// Input that bounds admission when [`Self::normalized_usage_input`] is
    /// `None` only because of opaque message parts (images, audio, files).
    /// They are left out of the count, so such a request is admitted only as
    /// a soft cap (see `RoutedRequest::soft_cap`) and never gets a
    /// missing-usage estimate.
    pub(crate) fn soft_cap_admission_input(
        &self,
    ) -> Option<crate::usage_estimator::NormalizedUsageInput> {
        crate::usage_estimator::NormalizedUsageInput::visible_json(&self.observed_value(true)?)
    }

    /// The observational request value when every field adds no input beyond
    /// its own wire bytes. With `opaque_parts`, message parts may also be
    /// image, audio, or file parts.
    fn observed_value(&self, opaque_parts: bool) -> Option<Value> {
        const OBSERVED: &[&str] = &[
            "model",
            "messages",
            "temperature",
            "top_p",
            "n",
            "stream",
            "stop",
            "max_tokens",
            "max_completion_tokens",
            "presence_penalty",
            "frequency_penalty",
            "logit_bias",
            "user",
            "tools",
            "tool_choice",
            "response_format",
            "stream_options",
            "parallel_tool_calls",
            "reasoning_effort",
            "verbosity",
            "service_tier",
            "logprobs",
            "top_logprobs",
            "seed",
            "store",
            "metadata",
            // Standard Chat controls that add no input beyond their own wire
            // bytes. Fields that make the provider read more than the body
            // (for example `web_search_options`) stay out, so they remain
            // Unknown instead of under-reserving.
            "prompt_cache_key",
            "prompt_cache_retention",
            "safety_identifier",
            "prediction",
            "modalities",
            "functions",
            "function_call",
        ];
        if self
            .fields
            .keys()
            .any(|name| !OBSERVED.contains(&name.as_str()))
        {
            return None;
        }
        let value = self.value();
        if self.fields.keys().any(|name| value.get(name).is_none()) {
            return None;
        }
        for name in ["n", "max_tokens", "max_completion_tokens"] {
            if value
                .get(name)
                .filter(|value| !value.is_null())
                .is_some_and(|value| value.as_u64().is_none())
            {
                return None;
            }
        }
        let messages = value.get("messages")?.as_array()?;
        for message in messages {
            if !message.as_object().is_some_and(|object| {
                object.keys().all(|name| {
                    matches!(
                        name.as_str(),
                        "role" | "content" | "name" | "tool_calls" | "tool_call_id"
                    )
                })
            }) {
                return None;
            }
            if let Some(content) = message
                .get("content")
                .filter(|content| !content.is_null() && !content.is_string())
            {
                if !content.as_array().is_some_and(|parts| {
                    parts.iter().all(|part| {
                        let text = part.as_object().is_some_and(|object| {
                            object
                                .keys()
                                .all(|name| matches!(name.as_str(), "type" | "text"))
                        }) && part.get("type").and_then(Value::as_str) == Some("text")
                            && part.get("text").is_some_and(Value::is_string);
                        text || (opaque_parts
                            && matches!(
                                part.get("type").and_then(Value::as_str),
                                Some("image_url" | "input_audio" | "file")
                            ))
                    })
                }) {
                    return None;
                }
            }
        }
        Some(value)
    }

    pub fn cache_fields(&self) -> Option<BTreeMap<String, Box<RawValue>>> {
        const SAFE: &[&str] = &[
            "model",
            "messages",
            "temperature",
            "top_p",
            "n",
            "stream",
            "stop",
            "max_tokens",
            "max_completion_tokens",
            "presence_penalty",
            "frequency_penalty",
            "logit_bias",
            "user",
            "tools",
            "tool_choice",
            "response_format",
            "stream_options",
            "parallel_tool_calls",
            "reasoning_effort",
            "verbosity",
            "service_tier",
            "logprobs",
            "top_logprobs",
            "seed",
        ];
        if self
            .fields
            .keys()
            .any(|name| !SAFE.contains(&name.as_str()))
        {
            return None;
        }
        let original = self.value();
        if self.fields.keys().any(|name| original.get(name).is_none()) {
            return None;
        }
        let represented = serde_json::to_value(&self.view).ok()?;
        for name in ["messages", "tools", "tool_choice", "parallel_tool_calls"] {
            if original
                .get(name)
                .filter(|value| !value.is_null())
                .is_some_and(|value| represented.get(name) != Some(value))
            {
                return None;
            }
        }
        if self.messages.iter().any(|message| {
            message
                .content
                .as_ref()
                .is_some_and(|value| !value.is_null() && !value.is_string())
        }) {
            return None;
        }
        if let Some(options) = original
            .get("stream_options")
            .filter(|value| !value.is_null())
        {
            if !options.as_object().is_some_and(|object| {
                object.keys().all(|name| name == "include_usage")
                    && object.get("include_usage").is_none_or(Value::is_boolean)
            }) {
                return None;
            }
        }
        let mut fields = self.fields.clone();
        let beta: Vec<_> = self
            .headers
            .iter()
            .filter(|(name, _)| name == "openai-beta")
            .collect();
        if !beta.is_empty() {
            fields.insert(
                "__native_headers".into(),
                serde_json::value::to_raw_value(&beta).ok()?,
            );
        }
        for name in ["stream", "stream_options", "user"] {
            fields.remove(name);
        }
        Some(fields)
    }
}

/// `value` with every null object member removed, recursively.
fn without_nulls(value: &Value) -> Value {
    match value {
        Value::Object(members) => Value::Object(
            members
                .iter()
                .filter(|(_, member)| !member.is_null())
                .map(|(key, member)| (key.clone(), without_nulls(member)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_nulls).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::NativeChatRequest;

    /// OpenAI's own tool-call replies carry `"content": null`, and clients
    /// append them verbatim on the next turn; a translated route must treat
    /// that null as the absent member the typed view omits.
    #[test]
    fn translated_chat_accepts_null_members_that_mean_absent() {
        let raw = r#"{"model":"x","messages":[
            {"role":"user","content":"weather?"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Lisbon\"}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":"{\"temp_c\":21}","name":null}
        ],"tools":[{"type":"function","function":{"name":"get_weather","parameters":{"type":"object"}}}],"temperature":null}"#;
        let request: NativeChatRequest = serde_json::from_str(raw).unwrap();
        let view = request.compatibility_request().unwrap();
        assert_eq!(view.messages.len(), 3);
        assert_eq!(view.messages[1].content, None);
        assert_eq!(
            view.messages[1].tool_calls.as_ref().unwrap()[0].id,
            "call_1"
        );

        // A member the view cannot carry is still refused.
        let raw = r#"{"model":"x","messages":[{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"f","arguments":"{}"},"index":0}]}]}"#;
        let request: NativeChatRequest = serde_json::from_str(raw).unwrap();
        assert!(request.compatibility_request().is_err());
    }
    #[test]
    fn unobservable_numbers_are_forwarded_without_panics_or_translation() {
        let raw = r#"{"model":"x","messages":[],"future_option":{"huge":1e999}}"#;
        let request: NativeChatRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(request.value()["model"], "x");
        assert!(request.normalized_usage_input().is_none());
        assert!(request.compatibility_request().is_err());
        assert!(serde_json::to_string(&request).unwrap().contains("1e999"));
        let image: crate::native_json::NativeJsonRequest<crate::types::CreateImageRequest> =
            serde_json::from_str(r#"{"model":"x","prompt":"hi","future_option":{"huge":1e999}}"#)
                .unwrap();
        assert_eq!(image.value()["prompt"], "hi");
        assert!(image.compatibility_request().is_err());
        assert!(serde_json::to_string(&image).unwrap().contains("1e999"));
    }

    #[test]
    fn native_estimation_cannot_shrink_precise_wire_numbers() {
        let raw = r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"temperature":0.123456789012345678901234567890}"#;
        let request: NativeChatRequest = serde_json::from_str(raw).unwrap();
        let wire_bytes = serde_json::to_vec(&request).unwrap().len() as u64;
        let observed_bytes = serde_json::to_vec(&request.value()).unwrap().len() as u64;
        assert_eq!(
            request
                .normalized_usage_input()
                .unwrap()
                .conservative_tokens(),
            wire_bytes.max(observed_bytes) + 32,
        );
    }

    #[test]
    fn explicit_null_completion_cap_keeps_existing_native_token_cap_normalization() {
        for stream in [false, true] {
            let mut request: NativeChatRequest = serde_json::from_str(
                r#"{"model":"x","messages":[],"max_tokens":100,"max_completion_tokens":null}"#,
            )
            .unwrap();
            request.set_stream(stream);
            request.translate_max_tokens();
            let wire: serde_json::Value =
                serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
            assert_eq!(wire["max_completion_tokens"], 100);
            assert!(wire.get("max_tokens").is_none());
            assert_eq!(wire["stream"], stream);
            assert_eq!(request.max_completion_tokens, Some(100));
            assert!(request.max_tokens.is_none());
        }
        let mut malformed: NativeChatRequest = serde_json::from_str(
            r#"{"model":"x","messages":[],"max_tokens":100,"max_completion_tokens":"bad"}"#,
        )
        .unwrap();
        malformed.translate_max_tokens();
        assert_eq!(malformed.value()["max_completion_tokens"], "bad");
        assert_eq!(malformed.value()["max_tokens"], 100);
    }

    #[test]
    fn ordinary_fractional_controls_remain_eligible_for_translation() {
        let request: NativeChatRequest = serde_json::from_str(
            r#"{"model":"x","messages":[],"temperature":0.7,"top_p":0.1,"presence_penalty":-0.2}"#,
        )
        .unwrap();
        assert!(request.compatibility_request().is_ok());
    }

    #[test]
    fn unknown_input_shapes_and_invalid_reservation_controls_are_never_estimated_as_text() {
        for raw in [
            r#"{"model":"x","messages":[{"role":"user","content":[{"type":"future_media","data":"opaque"}]}]}"#,
            r#"{"model":"x","messages":[{"role":"user","content":"hi","future_input":"opaque"}]}"#,
            r#"{"model":"x","messages":[],"future_input":"opaque"}"#,
            r#"{"model":"x","messages":[],"n":"2"}"#,
        ] {
            let request: NativeChatRequest = serde_json::from_str(raw).unwrap();
            assert!(request.normalized_usage_input().is_none(), "{raw}");
        }
        let text: NativeChatRequest = serde_json::from_str(r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"high","n":2,"max_completion_tokens":5}"#).unwrap();
        assert!(text.normalized_usage_input().is_some());
    }

    #[test]
    fn opaque_parts_admit_as_a_soft_cap_without_a_usage_estimate() {
        use crate::handlers::routing::RoutedRequest;
        for part in [
            r#"{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo=","detail":"high"}}"#,
            r#"{"type":"input_audio","input_audio":{"data":"UklGRg==","format":"wav"}}"#,
            r#"{"type":"file","file":{"file_data":"JVBERi0=","filename":"a.pdf"}}"#,
        ] {
            let raw = format!(
                r#"{{"model":"x","messages":[{{"role":"user","content":[{{"type":"text","text":"hi"}},{part}]}}],"max_tokens":5}}"#
            );
            let request: NativeChatRequest = serde_json::from_str(&raw).unwrap();
            assert!(request.normalized_usage_input().is_none(), "{raw}");
            assert!(request.soft_cap_admission_input().is_some(), "{raw}");
            assert!(request.conservative_input_tokens().is_some(), "{raw}");
            assert!(request.soft_cap(), "{raw}");
        }
        // Unknown shapes stay unknown rather than becoming soft caps.
        for raw in [
            r#"{"model":"x","messages":[{"role":"user","content":[{"type":"future_media","data":"opaque"}]}]}"#,
            r#"{"model":"x","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"u"}}]}],"future_input":"opaque"}"#,
        ] {
            let request: NativeChatRequest = serde_json::from_str(raw).unwrap();
            assert!(request.conservative_input_tokens().is_none(), "{raw}");
        }
        let text: NativeChatRequest =
            serde_json::from_str(r#"{"model":"x","messages":[{"role":"user","content":"hi"}]}"#)
                .unwrap();
        assert!(!text.soft_cap());
        // A schema property named like an opaque key is stripped from the
        // estimate, so the request must be a soft cap too.
        let schema: NativeChatRequest = serde_json::from_str(
            r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"metadata":{"audio":"x"}}"#,
        )
        .unwrap();
        assert!(schema.normalized_usage_input().is_none());
        assert!(schema.conservative_input_tokens().is_some());
        assert!(schema.soft_cap());
    }

    #[test]
    fn standard_chat_controls_without_extra_input_are_still_estimated() {
        for extra in [
            r#""prompt_cache_key":"tenant-1""#,
            r#""safety_identifier":"user-hash""#,
            r#""prompt_cache_retention":"24h""#,
            r#""prediction":{"type":"content","content":"hello"}"#,
            r#""modalities":["text"]"#,
            r#""function_call":"auto""#,
        ] {
            let raw =
                format!(r#"{{"model":"x","messages":[{{"role":"user","content":"hi"}}],{extra}}}"#);
            let request: NativeChatRequest = serde_json::from_str(&raw).unwrap();
            let input = request.normalized_usage_input();
            assert!(input.is_some(), "{raw}");
            assert!(
                input.unwrap().conservative_tokens() >= raw.len() as u64,
                "{raw}"
            );
        }
        for raw in [
            r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"web_search_options":{}}"#,
            r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"audio":{"voice":"alloy","format":"mp3"}}"#,
        ] {
            let request: NativeChatRequest = serde_json::from_str(raw).unwrap();
            assert!(request.normalized_usage_input().is_none(), "{raw}");
        }
    }
}
