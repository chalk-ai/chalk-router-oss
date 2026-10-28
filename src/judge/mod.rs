//! Route-owned classification runtime and reusable judge utilities.
//! Legacy scoring types remain for archived policy compatibility tests.

mod cache;
mod classifier;
mod heuristic;
pub(crate) mod input;
mod llm;
pub(crate) mod routing;
pub use routing::RouteJudgeRuntime;
mod runtime;

pub use cache::{difficulty_cache_key, session_cache_key, DifficultyCache};
pub use classifier::ClassifierJudge;
pub use heuristic::looks_hard;
pub(crate) use heuristic::looks_hard_with_config;
pub use llm::{LlmJudge, JUDGE_PROMPT_VERSION};
pub use runtime::JudgeRuntime;

use async_trait::async_trait;

use crate::types::{ChatCompletionRequest, ChatMessage};

/// Default per-message character clip applied before a conversation is shown to
/// the judge. Keeps a 100k-token history from making the judge slow/expensive.
pub const DEFAULT_JUDGE_MAX_CHARS_PER_MSG: usize = 2_000;
/// Default number of trailing turns kept for the judge.
pub const DEFAULT_JUDGE_MAX_TURNS: usize = 8;

/// A judge's assessment of how hard a request is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JudgeVerdict {
    /// `0.0` = trivial (cheapest model suffices) .. `1.0` = hard (ceiling).
    pub difficulty: f32,
}

/// Why a judge call did not produce a usable verdict. The caller treats every
/// variant identically — **fail open** to the requested (ceiling) model — but
/// the variant is kept for logging/metrics.
#[derive(Debug)]
pub enum JudgeError {
    /// The judge backend call itself failed (network, provider error, timeout).
    Backend(String),
    /// The judge responded but its output could not be parsed into a score.
    Parse(String),
}

impl std::fmt::Display for JudgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JudgeError::Backend(m) => write!(f, "judge backend error: {m}"),
            JudgeError::Parse(m) => write!(f, "judge parse error: {m}"),
        }
    }
}

impl std::error::Error for JudgeError {}

/// Everything a judge needs to score one request. Borrows already-truncated
/// messages; a judge never owns the full conversation.
pub struct JudgeRequest<'a> {
    pub messages: &'a [ChatMessage],
    pub requested_model: &'a str,
    /// The judge model to use, read from the *current* DB policy at call time
    /// (never captured at construction) so a UI model change takes effect
    /// without a restart.
    pub judge_model: &'a str,
}

#[async_trait]
pub trait Judge: Send + Sync {
    /// Score a request. An `Err` is the caller's cue to fail open (serve the
    /// requested model); scoring must never make routing fatal.
    async fn score(&self, req: &JudgeRequest<'_>) -> Result<JudgeVerdict, JudgeError>;
}

/// Canonicalize a model name for ladder/floor comparisons (dedupe, floor
/// position, ladder lookup). Resolution and permission checks use the raw
/// string — those are exact-match systems everywhere else in the router — so
/// canonicalization is only for comparing two configured names to each other.
pub fn canonicalize_model(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

/// Whether a request is eligible for downgrade at all, on capability grounds.
///
/// Phase 1 never downgrades requests that carry tools/function-calling,
/// `response_format`/structured output, or image content parts: these constrain
/// which model can serve the request, and the ladder is not capability-aware
/// yet. Returning `false` is an additional short-circuit before the judge runs.
pub fn request_is_downgradable(req: &ChatCompletionRequest) -> bool {
    if req.tools.as_ref().is_some_and(|t| !t.is_empty()) {
        return false;
    }
    if req.response_format.is_some() {
        return false;
    }
    !req.messages.iter().any(message_has_image)
}

fn message_has_image(msg: &ChatMessage) -> bool {
    match &msg.content {
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("image_url")),
        _ => false,
    }
}

/// Render a message's text for inclusion in the judge prompt (or for reading
/// the judge's own reply). Used by the LLM judge backend and the heuristic.
pub(crate) fn message_text_for_prompt(msg: &ChatMessage) -> String {
    message_text(msg)
}

/// Extract plain text from a message's `content`, which OpenAI allows to be
/// either a string or an array of typed parts.
fn message_text(msg: &ChatMessage) -> String {
    match &msg.content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Truncate a conversation for judging: keep the system message plus the last
/// `max_turns` messages, each clipped to `max_chars` characters. Runs before
/// any clone into a spawned shadow task so sampled traffic pays a small clone.
pub fn truncate_for_judge(
    messages: &[ChatMessage],
    max_turns: usize,
    max_chars: usize,
) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::new();
    // Preserve a leading system message regardless of the turn window.
    let mut start = 0usize;
    if messages.first().is_some_and(|m| m.role == "system") {
        out.push(clip_message(&messages[0], max_chars));
        start = 1;
    }
    let tail_start = messages.len().saturating_sub(max_turns).max(start);
    for msg in &messages[tail_start..] {
        out.push(clip_message(msg, max_chars));
    }
    out
}

fn clip_message(msg: &ChatMessage, max_chars: usize) -> ChatMessage {
    let text = message_text(msg);
    let clipped: String = if text.chars().count() > max_chars {
        text.chars().take(max_chars).collect()
    } else {
        text
    };
    ChatMessage {
        role: msg.role.clone(),
        content: Some(serde_json::Value::String(clipped)),
        name: None,
        tool_calls: None,
        tool_call_id: None,
    }
}

/// Pick the ladder index for a difficulty score.
///
/// The ladder is ordered ceiling→floor (index 0 = requested/most-capable, last
/// index = floor/cheapest). `boundaries` are the sorted cut points in `(0, 1)`
/// separating rungs; there must be `ladder_len - 1` of them. When empty,
/// equal-width bands over `[0, 1]` are used. Higher difficulty ⇒ lower index
/// (more capable). For a length-2 ladder with `boundaries = [t]` this reduces
/// to RouteLLM's rule: `difficulty < t → floor`, else the requested model.
pub fn select_rung(difficulty: f32, ladder_len: usize, boundaries: &[f32]) -> usize {
    debug_assert!(ladder_len >= 1);
    if ladder_len <= 1 {
        return 0;
    }
    let d = difficulty.clamp(0.0, 1.0);
    let n = ladder_len;

    // Segment index in [0, n-1]: how many band boundaries the score meets or
    // exceeds. Default bands are equal width when no explicit boundaries given.
    let seg = if boundaries.is_empty() {
        // floor(d * n), clamped so d == 1.0 lands in the top segment.
        ((d * n as f32).floor() as usize).min(n - 1)
    } else {
        boundaries.iter().filter(|&&b| d >= b).count().min(n - 1)
    };

    // Higher difficulty → more capable (lower index).
    (n - 1) - seg
}

#[cfg(test)]
pub(crate) fn test_msg(role: &str, text: &str) -> ChatMessage {
    ChatMessage {
        role: role.to_string(),
        content: Some(serde_json::Value::String(text.to_string())),
        name: None,
        tool_calls: None,
        tool_call_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, text: &str) -> ChatMessage {
        test_msg(role, text)
    }

    #[test]
    fn binary_ladder_threshold() {
        // length-2 ladder, single boundary at 0.5 → RouteLLM binary rule.
        assert_eq!(select_rung(0.2, 2, &[0.5]), 1); // easy → floor
        assert_eq!(select_rung(0.8, 2, &[0.5]), 0); // hard → ceiling
        assert_eq!(select_rung(0.5, 2, &[0.5]), 0); // boundary is inclusive → ceiling
    }

    #[test]
    fn equal_width_bands_three_rungs() {
        // bands: [0,1/3) floor, [1/3,2/3) middle, [2/3,1] ceiling
        assert_eq!(select_rung(0.1, 3, &[]), 2); // floor
        assert_eq!(select_rung(0.5, 3, &[]), 1); // middle
        assert_eq!(select_rung(0.9, 3, &[]), 0); // ceiling
        assert_eq!(select_rung(1.0, 3, &[]), 0); // clamps into top band
        assert_eq!(select_rung(0.0, 3, &[]), 2);
    }

    #[test]
    fn single_rung_never_downgrades() {
        assert_eq!(select_rung(0.0, 1, &[]), 0);
    }

    #[test]
    fn capability_short_circuit() {
        let mut req = ChatCompletionRequest {
            fallbacks: Default::default(),
            model: "chalk/gpt-4o".to_string(),
            messages: vec![msg("user", "hi")],
            temperature: None,
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            max_tokens: None,
            max_completion_tokens: None,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
            user: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            stream_options: None,
            parallel_tool_calls: None,
            reasoning_effort: None,
        };
        assert!(request_is_downgradable(&req));
        req.response_format = Some(serde_json::json!({"type": "json_object"}).into());
        assert!(!request_is_downgradable(&req));
        req.response_format = None;
        req.messages = vec![ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!([{"type": "image_url", "image_url": {"url": "x"}}])),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }];
        assert!(!request_is_downgradable(&req));
    }

    #[test]
    fn truncate_keeps_system_and_tail() {
        let msgs = vec![
            msg("system", "sys"),
            msg("user", "1"),
            msg("assistant", "2"),
            msg("user", "3"),
        ];
        let out = truncate_for_judge(&msgs, 2, 100);
        // system + last 2
        let roles: Vec<&str> = out.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["system", "assistant", "user"]);
    }

    #[test]
    fn truncate_clips_long_messages() {
        let long = "x".repeat(50);
        let out = truncate_for_judge(&[msg("user", &long)], 8, 10);
        assert_eq!(message_text(&out[0]).len(), 10);
    }
}
