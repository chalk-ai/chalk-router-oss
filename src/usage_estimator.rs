//! Conservative usage estimation for successful text-generation calls whose
//! provider response omits authoritative usage.
//!
//! This intentionally does not try to imitate a model tokenizer. The bound is
//! one token per serialized UTF-8 byte plus fixed request/response framing,
//! which is deliberately conservative for byte-level tokenizers. Multimodal
//! and opaque/binary inputs, including references to provider-stored history,
//! are never assigned a false text-token estimate; admission bounds only their
//! visible bytes and holds them to a soft cap.

use std::io::{self, Write};

use crate::backend::RouteResolution;

const INPUT_FRAMING_TOKENS: u64 = 32;
const OUTPUT_FRAMING_TOKENS: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EstimateMethod {
    ConservativeHeuristic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TokenEstimate {
    pub tokens: u64,
    pub method: EstimateMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NormalizedUsageInput {
    utf8_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct NormalizedUsageOutput {
    utf8_bytes: u64,
}

pub(crate) trait UsageEstimator: Send + Sync {
    fn estimate_input(&self, input: &NormalizedUsageInput) -> TokenEstimate;
    fn estimate_output(&self, output: &NormalizedUsageOutput) -> TokenEstimate;
}

struct ConservativeTextEstimator;

impl UsageEstimator for ConservativeTextEstimator {
    fn estimate_input(&self, input: &NormalizedUsageInput) -> TokenEstimate {
        TokenEstimate {
            tokens: input.utf8_bytes.saturating_add(INPUT_FRAMING_TOKENS),
            method: EstimateMethod::ConservativeHeuristic,
        }
    }

    fn estimate_output(&self, output: &NormalizedUsageOutput) -> TokenEstimate {
        TokenEstimate {
            tokens: output.utf8_bytes.saturating_add(OUTPUT_FRAMING_TOKENS),
            method: EstimateMethod::ConservativeHeuristic,
        }
    }
}

static CONSERVATIVE_TEXT_ESTIMATOR: ConservativeTextEstimator = ConservativeTextEstimator;

/// Estimators are registered for Chalk's built-in generation adapters. An
/// arbitrary OpenAI-compatible connection must report usage until it gains an
/// explicit estimator configuration contract.
fn estimator_for(resolution: &RouteResolution) -> Option<&'static dyn UsageEstimator> {
    (crate::provider_registry::is_builtin(&resolution.provider_kind)
        && resolution.provider_kind != "voyage")
        .then_some(&CONSERVATIVE_TEXT_ESTIMATOR as &dyn UsageEstimator)
}

/// Count exactly the bytes serde_json would write, without retaining a payload.
/// Saturation preserves the conservative estimate even on counter overflow.
#[derive(Default)]
struct JsonByteCounter(u64);

impl Write for JsonByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn serialized_json_bytes(value: &impl serde::Serialize) -> Option<u64> {
    let mut counter = JsonByteCounter::default();
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.0)
}

/// Where a request's tool context comes from. Caller-run tools return their
/// results inline on a later request, so the wire bounds them. Hosted tools run
/// at the provider, which adds their output to the model context unseen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolContext {
    Visible,
    Hosted,
    /// A Responses tool type no one has classified yet.
    Unclassified,
}

impl ToolContext {
    fn of_tools<'a>(
        tools: impl IntoIterator<Item = &'a serde_json::Value>,
        classify: impl Fn(&serde_json::Value) -> Self,
    ) -> Self {
        tools
            .into_iter()
            .map(classify)
            .max_by_key(|context| match context {
                Self::Visible => 0,
                Self::Hosted => 1,
                Self::Unclassified => 2,
            })
            .unwrap_or(Self::Visible)
    }

    /// Classify an OpenAI Responses request's `tools`.
    pub(crate) fn of_responses(value: &serde_json::Value) -> Self {
        let tools = value.get("tools").and_then(serde_json::Value::as_array);
        Self::of_tools(tools.into_iter().flatten(), |tool| {
            match tool.get("type").and_then(serde_json::Value::as_str) {
                Some(
                    "function"
                    | "custom"
                    | "computer_use_preview"
                    | "computer_use"
                    | "local_shell"
                    | "apply_patch",
                ) => Self::Visible,
                Some(kind) if kind.starts_with("web_search") => Self::Hosted,
                Some("file_search" | "code_interpreter" | "mcp" | "image_generation") => {
                    Self::Hosted
                }
                _ => Self::Unclassified,
            }
        })
    }

    /// Classify an Anthropic Messages request's server tools and MCP servers.
    /// Every other tool is caller-run, as Messages has always admitted it.
    pub(crate) fn of_messages(value: &serde_json::Value) -> Self {
        let has_mcp_servers = value
            .get("mcp_servers")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|servers| !servers.is_empty());
        if has_mcp_servers {
            return Self::Hosted;
        }
        let tools = value.get("tools").and_then(serde_json::Value::as_array);
        Self::of_tools(tools.into_iter().flatten(), |tool| {
            match tool.get("type").and_then(serde_json::Value::as_str) {
                Some(kind)
                    if ["web_search_", "web_fetch_", "code_execution_"]
                        .iter()
                        .any(|prefix| kind.starts_with(prefix)) =>
                {
                    Self::Hosted
                }
                _ => Self::Visible,
            }
        })
    }
}

impl NormalizedUsageInput {
    /// Visible input of a Responses request whose tool context the wire bounds.
    /// Hosted tools add provider-side input, so neither a hard reservation nor
    /// a missing-usage estimate may rely on this view; admission uses
    /// [`Self::visible_responses_json`] with a soft cap instead.
    pub(crate) fn from_responses_json(value: &serde_json::Value) -> Option<Self> {
        match ToolContext::of_responses(value) {
            ToolContext::Visible => Self::from_json(value),
            ToolContext::Hosted | ToolContext::Unclassified => None,
        }
    }

    /// Visible input of a Responses request with classified tools, including
    /// hosted ones and input the provider adds (see [`Self::visible_json`]).
    /// Only a soft-cap admission may use it.
    pub(crate) fn visible_responses_json(value: &serde_json::Value) -> Option<Self> {
        match ToolContext::of_responses(value) {
            ToolContext::Visible | ToolContext::Hosted => Self::visible_json(value),
            ToolContext::Unclassified => None,
        }
    }

    /// The input bytes the wire carries, leaving out opaque content (images,
    /// audio, files, encrypted reasoning) whose token cost only the provider
    /// knows. Equal to [`Self::from_json`] when [`provider_adds_input`] is
    /// false; otherwise only a soft-cap admission may use it, and a
    /// missing-usage estimate never may.
    pub(crate) fn visible_json(value: &serde_json::Value) -> Option<Self> {
        let total = serialized_json_bytes(value)?;
        Some(Self {
            utf8_bytes: total.saturating_sub(opaque_bytes(value)),
        })
    }

    pub(crate) fn from_json(value: &serde_json::Value) -> Option<Self> {
        if has_referenced_history(value) || contains_unsupported_modality(value) {
            return None;
        }
        Some(Self {
            utf8_bytes: serialized_json_bytes(value)?,
        })
    }

    /// Native envelopes retain number lexemes and whitespace that a JSON
    /// observation can shorten. Keep the reservation at least as large as the
    /// authoritative wire input without changing modality validation.
    pub(crate) fn with_minimum_wire_bytes(mut self, bytes: u64) -> Self {
        self.utf8_bytes = self.utf8_bytes.max(bytes);
        self
    }

    /// Provider-independent conservative bound used for pre-dispatch token
    /// reservations. Unlike response-side usage estimation, this does not need
    /// a registered adapter: one token per input byte plus framing is safe for
    /// text on arbitrary OpenAI-compatible connections too.
    pub(crate) fn conservative_tokens(&self) -> u64 {
        self.utf8_bytes.saturating_add(INPUT_FRAMING_TOKENS)
    }
}

impl NormalizedUsageOutput {
    pub(crate) fn observe_text(&mut self, text: &str) {
        self.utf8_bytes = self
            .utf8_bytes
            .saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
    }

    pub(crate) fn observe_json(&mut self, value: &serde_json::Value) {
        if let Some(bytes) = serialized_json_bytes(value) {
            self.utf8_bytes = self.utf8_bytes.saturating_add(bytes);
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct UsageEstimateState {
    input: TokenEstimate,
    output: NormalizedUsageOutput,
    estimator: &'static dyn UsageEstimator,
}

impl UsageEstimateState {
    pub(crate) fn new(
        resolution: &RouteResolution,
        input: Option<NormalizedUsageInput>,
    ) -> Option<Self> {
        let estimator = estimator_for(resolution)?;
        let input = estimator.estimate_input(&input?);
        Some(Self {
            input,
            output: NormalizedUsageOutput::default(),
            estimator,
        })
    }

    pub(crate) fn observe_text(&mut self, text: &str) {
        self.output.observe_text(text);
    }

    pub(crate) fn observe_json(&mut self, value: &serde_json::Value) {
        self.output.observe_json(value);
    }

    pub(crate) fn finish(self) -> (TokenEstimate, TokenEstimate) {
        (self.input, self.estimator.estimate_output(&self.output))
    }
}

/// Inspect protocol fields, not arbitrary keys inside tool arguments, schemas,
/// or user content. Inline tool history and signatures do not by themselves
/// reference additional input stored at the provider.
fn has_referenced_history(value: &serde_json::Value) -> bool {
    [
        "previous_interaction_id",
        "previous_response_id",
        "conversation",
        "cached_content",
        "cachedContent",
    ]
    .iter()
    .any(|field| value.get(field).is_some_and(|value| !value.is_null()))
        || value
            .get("input")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("type").and_then(serde_json::Value::as_str) == Some("item_reference")
                        || (item.get("type").is_none()
                            && item.get("id").is_some_and(serde_json::Value::is_string)
                            && item.get("content").is_none())
                })
            })
}

/// Whether the provider reads input the wire does not bound: history stored at
/// the provider, or opaque content. Such a request can only be admitted as a
/// soft cap over [`NormalizedUsageInput::visible_json`].
pub(crate) fn provider_adds_input(value: &serde_json::Value) -> bool {
    has_referenced_history(value) || contains_unsupported_modality(value)
}

/// Keys whose value is opaque content, and part types that are opaque as a whole.
const OPAQUE_KEYS: &[&str] = &[
    "image_url",
    "input_audio",
    "audio",
    "video",
    "encrypted_content",
];
const OPAQUE_PART_TYPES: &[&str] = &[
    "image",
    "image_url",
    "input_image",
    "input_audio",
    "audio",
    "video",
    "file",
    "document",
];

fn is_opaque_part(object: &serde_json::Map<String, serde_json::Value>) -> bool {
    object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| OPAQUE_PART_TYPES.contains(&kind))
}

fn contains_unsupported_modality(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(values) => values.iter().any(contains_unsupported_modality),
        serde_json::Value::Object(object) => {
            object.keys().any(|key| OPAQUE_KEYS.contains(&key.as_str()))
                || is_opaque_part(object)
                || object.values().any(contains_unsupported_modality)
        }
        _ => false,
    }
}

/// Serialized bytes of the opaque subtrees [`contains_unsupported_modality`]
/// finds: whole opaque parts, and the values of opaque keys.
fn opaque_bytes(value: &serde_json::Value) -> u64 {
    match value {
        serde_json::Value::Array(values) => values.iter().map(opaque_bytes).sum(),
        serde_json::Value::Object(object) if is_opaque_part(object) => {
            serialized_json_bytes(value).unwrap_or(0)
        }
        serde_json::Value::Object(object) => object
            .iter()
            .map(|(key, value)| {
                if OPAQUE_KEYS.contains(&key.as_str()) {
                    serialized_json_bytes(value).unwrap_or(0)
                } else {
                    opaque_bytes(value)
                }
            })
            .sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolution(provider: &str) -> RouteResolution {
        RouteResolution {
            provider_kind: provider.to_string(),
            connection_id: Some("connection-1".to_string()),
            connection_name: "primary".to_string(),
            upstream_model_id: "model-1".to_string(),
            connection_prefix: None,
            model_route_id: None,
        }
    }

    #[test]
    fn counted_json_matches_allocated_serialization_for_text_and_tools() {
        for value in [
            serde_json::json!({"input": "雪🦀\n\t\"\\\u{0000}"}),
            serde_json::json!({
                "model": "model", "temperature": 0.123456789,
                "messages": [
                    {"role": "assistant", "tool_calls": [{"id": "call-1", "type": "function",
                        "function": {"name": "lookup", "arguments": "{\"city\":\"東京\"}"}}]},
                    {"role": "tool", "tool_call_id": "call-1", "content": "晴れ"}
                ],
                "tools": [{"type": "function", "function": {"name": "lookup",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}]
            }),
            serde_json::json!({"input": "x".repeat(32_768), "metadata": {"empty": null}}),
        ] {
            let previous_bytes = serde_json::to_vec(&value).unwrap().len() as u64;
            assert_eq!(serialized_json_bytes(&value), Some(previous_bytes));
            let input = NormalizedUsageInput::from_json(&value).unwrap();
            assert_eq!(
                input.conservative_tokens(),
                previous_bytes + INPUT_FRAMING_TOKENS
            );
            let mut estimate = UsageEstimateState::new(&resolution("openai"), Some(input)).unwrap();
            estimate.observe_json(&value);
            let (estimated_input, estimated_output) = estimate.finish();
            assert_eq!(estimated_input.tokens, input.conservative_tokens());
            assert_eq!(
                estimated_output.tokens,
                previous_bytes + OUTPUT_FRAMING_TOKENS
            );
        }
    }

    #[test]
    fn counting_handles_raw_json_errors_and_counter_overflow() {
        let raw = serde_json::value::RawValue::from_string(
            r#"{ "temperature" : 0.123456789012345678901234567890, "text": "雪" }"#.into(),
        )
        .unwrap();
        assert_eq!(serialized_json_bytes(&raw), Some(raw.get().len() as u64));

        struct Invalid;
        impl serde::Serialize for Invalid {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("test serialization error"))
            }
        }
        assert_eq!(serialized_json_bytes(&Invalid), None);
        let mut counter = JsonByteCounter(u64::MAX - 1);
        assert_eq!(counter.write(b"abc").unwrap(), 3);
        assert_eq!(counter.0, u64::MAX);
        assert_eq!(
            NormalizedUsageInput {
                utf8_bytes: counter.0
            }
            .conservative_tokens(),
            u64::MAX
        );
    }

    #[test]
    fn conservative_estimate_is_byte_bounded_and_has_framing() {
        let input = NormalizedUsageInput::from_json(&serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}]
        }));
        let mut state = UsageEstimateState::new(&resolution("openai"), input).unwrap();
        state.observe_text("world");
        let (input, output) = state.finish();
        assert_eq!(input.method, EstimateMethod::ConservativeHeuristic);
        assert_eq!(output.method, EstimateMethod::ConservativeHeuristic);
        assert!(input.tokens >= 32);
        assert!(output.tokens >= "world".len() as u64 + 8);
    }

    #[test]
    fn multimodal_and_arbitrary_connections_do_not_claim_text_estimates() {
        let image = NormalizedUsageInput::from_json(&serde_json::json!({
            "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}}]
        }));
        assert!(image.is_none());

        let text = NormalizedUsageInput::from_json(&serde_json::json!({"input": "hello"}));
        assert!(text.is_some_and(|input| input.conservative_tokens() > 0));
        assert!(UsageEstimateState::new(&resolution("openai-compatible"), text).is_none());
    }

    #[test]
    fn responses_tools_are_classified_by_who_runs_them() {
        let request = |kind: &str| serde_json::json!({"input":"hello","max_output_tokens":32,"tools":[{"type":kind}]});
        for (kind, context) in [
            ("function", ToolContext::Visible),
            ("custom", ToolContext::Visible),
            ("computer_use_preview", ToolContext::Visible),
            ("local_shell", ToolContext::Visible),
            ("apply_patch", ToolContext::Visible),
            ("web_search", ToolContext::Hosted),
            ("web_search_preview", ToolContext::Hosted),
            ("file_search", ToolContext::Hosted),
            ("code_interpreter", ToolContext::Hosted),
            ("mcp", ToolContext::Hosted),
            ("image_generation", ToolContext::Hosted),
            ("future_hosted_tool", ToolContext::Unclassified),
        ] {
            let request = request(kind);
            assert_eq!(ToolContext::of_responses(&request), context, "{kind}");
            // Only a hosted tool admits on its visible input without the
            // estimate that could stand in for missing usage.
            let visible = NormalizedUsageInput::visible_responses_json(&request);
            let estimable = NormalizedUsageInput::from_responses_json(&request);
            assert_eq!(
                visible.is_some(),
                context != ToolContext::Unclassified,
                "{kind}"
            );
            assert_eq!(
                estimable.is_some(),
                context == ToolContext::Visible,
                "{kind}"
            );
        }
        let mixed = serde_json::json!({"input":"hello","tools":[
            {"type":"function","name":"lookup"},{"type":"web_search"},{"type":"future_hosted_tool"}]});
        assert_eq!(ToolContext::of_responses(&mixed), ToolContext::Unclassified);
        let function = serde_json::json!({"input":"hello","max_output_tokens":32,
            "tools":[{"type":"function","name":"lookup","parameters":{"properties":{"type":{"const":"web_search"}}}}]});
        assert!(NormalizedUsageInput::from_responses_json(&function).is_some());
        let custom = serde_json::json!({"max_output_tokens":32,
            "tools":[{"type":"custom","name":"lookup","format":{"type":"text"}}],
            "input":[{"type":"custom_tool_call","call_id":"call-1","name":"lookup","input":"hello"},
                {"type":"custom_tool_call_output","call_id":"call-1","output":"world"}]});
        assert!(NormalizedUsageInput::from_responses_json(&custom).is_some());
    }

    #[test]
    fn messages_server_tools_and_mcp_servers_are_hosted() {
        let messages = |extra: serde_json::Value| {
            let mut request = serde_json::json!({"messages":[{"role":"user","content":"hello"}]});
            request
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            request
        };
        for (extra, context) in [
            (serde_json::json!({}), ToolContext::Visible),
            // Anthropic's caller-run tools omit `type` or name a client tool.
            (
                serde_json::json!({"tools":[{"name":"lookup","input_schema":{"type":"object"}}]}),
                ToolContext::Visible,
            ),
            (
                serde_json::json!({"tools":[{"type":"bash_20250124","name":"bash"}]}),
                ToolContext::Visible,
            ),
            (
                serde_json::json!({"tools":[{"type":"web_search_20250305","name":"web_search"}]}),
                ToolContext::Hosted,
            ),
            (
                serde_json::json!({"tools":[{"type":"web_fetch_20250910","name":"web_fetch"}]}),
                ToolContext::Hosted,
            ),
            (
                serde_json::json!({"tools":[{"type":"code_execution_20250825","name":"code_execution"}]}),
                ToolContext::Hosted,
            ),
            (
                serde_json::json!({"mcp_servers":[{"type":"url","url":"https://example.test/mcp","name":"x"}]}),
                ToolContext::Hosted,
            ),
            (serde_json::json!({"mcp_servers":[]}), ToolContext::Visible),
        ] {
            let request = messages(extra);
            assert_eq!(ToolContext::of_messages(&request), context, "{request}");
            assert!(NormalizedUsageInput::from_json(&request).is_some());
        }
    }

    #[test]
    fn stored_history_is_a_soft_cap_with_no_fallback_usage_estimate() {
        for request in [
            serde_json::json!({"input":"continue","previous_interaction_id":"interaction-1"}),
            serde_json::json!({"input":"continue","previous_response_id":"resp-1"}),
            serde_json::json!({"input":"continue","conversation":"conv-1"}),
            serde_json::json!({"input":"continue","conversation":{"id":"conv-1"}}),
            serde_json::json!({"input":"continue","cached_content":"cached-1"}),
            serde_json::json!({"input":"continue","cachedContent":"cached-1"}),
            serde_json::json!({"input":[{"type":"item_reference","id":"item-1"}]}),
            serde_json::json!({"input":[{"id":"item-1"}]}),
        ] {
            let input = NormalizedUsageInput::from_json(&request);
            assert!(input.is_none(), "{request}");
            assert!(UsageEstimateState::new(&resolution("openai"), input).is_none());
            assert!(provider_adds_input(&request), "{request}");
            // The reference itself is visible and counted.
            assert_eq!(
                NormalizedUsageInput::visible_json(&request),
                Some(NormalizedUsageInput {
                    utf8_bytes: serialized_json_bytes(&request).unwrap()
                }),
                "{request}"
            );
        }
    }

    #[test]
    fn opaque_content_is_left_out_of_the_visible_soft_cap_input() {
        let blob = "A".repeat(10_000);
        for (request, opaque) in [
            (
                serde_json::json!({"input":[{"type":"reasoning","id":"rs-1","summary":[],
                    "encrypted_content":blob}]}),
                serde_json::json!(blob),
            ),
            (
                serde_json::json!({"input":[{"role":"user","content":[{"type":"input_text","text":"hi"},
                    {"type":"input_image","image_url":format!("data:image/png;base64,{blob}")}]}]}),
                serde_json::json!({"type":"input_image",
                    "image_url":format!("data:image/png;base64,{blob}")}),
            ),
            (
                serde_json::json!({"messages":[{"role":"user","content":[{"type":"text","text":"hi"},
                    {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{blob}")}}]}]}),
                serde_json::json!({"type":"image_url",
                    "image_url":{"url":format!("data:image/png;base64,{blob}")}}),
            ),
            (
                serde_json::json!({"messages":[{"role":"user","content":[{"type":"document",
                    "source":{"type":"base64","media_type":"application/pdf","data":blob}}]}]}),
                serde_json::json!({"type":"document",
                    "source":{"type":"base64","media_type":"application/pdf","data":blob}}),
            ),
        ] {
            assert!(provider_adds_input(&request), "{request}");
            // Opaque content never gets a text-token estimate.
            assert!(NormalizedUsageInput::from_json(&request).is_none());
            let visible = NormalizedUsageInput::visible_json(&request).unwrap();
            let total = serialized_json_bytes(&request).unwrap();
            assert_eq!(
                visible.utf8_bytes,
                total - serialized_json_bytes(&opaque).unwrap(),
                "{request}"
            );
            assert!(visible.utf8_bytes < 1_000, "{request}");
        }
        let text = serde_json::json!({"messages":[{"role":"user","content":"hi"}]});
        assert!(!provider_adds_input(&text));
        assert_eq!(
            NormalizedUsageInput::visible_json(&text),
            NormalizedUsageInput::from_json(&text)
        );
    }

    #[test]
    fn inline_history_and_reference_names_in_user_data_remain_estimable() {
        for request in [
            serde_json::json!({"input":"continue","previous_interaction_id":null,
                "previous_response_id":null,"conversation":null,"cached_content":null,"cachedContent":null}),
            serde_json::json!({"input":[
                {"type":"thought","signature":"opaque-signature"},
                {"type":"function_result","call_id":"call-1","result":{"previous_interaction_id":"user-data","type":"item_reference"}},
                {"type":"message","id":"msg-1","role":"assistant","content":"hello"}
            ]}),
            serde_json::json!({"input":"hello","metadata":{"previous_response_id":"user-data"},
                "tools":[{"type":"function","parameters":{"properties":{"conversation":{"type":"string"}}}}]}),
        ] {
            assert!(
                NormalizedUsageInput::from_json(&request).is_some(),
                "{request}"
            );
        }
    }
}
