//! Configurable binary difficulty classification: any enabled hard signal
//! selects outcome 1; otherwise the request selects outcome 0.

use crate::route_grid::HeuristicJudgeBackend;
use crate::types::ChatMessage;

use super::message_text_for_prompt;

/// Default case-insensitive substring markers for technical or multi-step work.
const HARD_MARKERS: &[&str] = &[
    "step by step",
    "step-by-step",
    "prove ",
    "proof ",
    "theorem",
    "algorithm",
    "refactor",
    "debug",
    "stack trace",
    "regression",
];

/// Classify using the default heuristic settings. Retained for legacy callers.
pub fn looks_hard(messages: &[ChatMessage]) -> bool {
    looks_hard_with_config(messages, &HeuristicJudgeBackend::default())
}

/// Any enabled signal marks a request hard; otherwise it is easy. Keyword
/// matching uses user text, while code fences and length include every role.
pub(crate) fn looks_hard_with_config(
    messages: &[ChatMessage],
    config: &HeuristicJudgeBackend,
) -> bool {
    let removed: Vec<_> = config
        .removed_hard_keywords
        .iter()
        .map(|keyword| keyword.to_lowercase())
        .collect();
    let markers: Vec<_> = HARD_MARKERS
        .iter()
        .filter(|marker| {
            config.use_default_keywords.unwrap_or(true)
                && !removed.iter().any(|removed| removed == **marker)
        })
        .map(|marker| (*marker).to_owned())
        .chain(
            config
                .added_hard_keywords
                .iter()
                .map(|keyword| keyword.to_lowercase()),
        )
        .collect();
    let max_bytes = config.max_easy_input_bytes.unwrap_or(6_000) as usize;
    let mut total_bytes = 0usize;
    for msg in messages {
        let text = message_text_for_prompt(msg);
        total_bytes = total_bytes.saturating_add(text.len());
        if max_bytes != 0 && total_bytes > max_bytes {
            return true;
        }
        if config.code_blocks_are_hard.unwrap_or(true) && text.contains("```") {
            return true;
        }
        if msg.role == "user" {
            let lower = text.to_lowercase();
            if markers.iter().any(|marker| lower.contains(marker)) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::test_msg as msg;

    #[test]
    fn trivial_text_is_not_hard() {
        assert!(!looks_hard(&[msg("user", "hi, how are you?")]));
        assert!(!looks_hard(&[
            msg("system", "you are helpful"),
            msg("user", "what is the capital of France?"),
        ]));
    }

    #[test]
    fn code_fence_is_hard() {
        assert!(looks_hard(&[msg(
            "user",
            "why does this fail?\n```rust\nfn main() {}\n```"
        )]));
        // Fences count in any role: pasted assistant code in history still
        // signals a technical session.
        assert!(looks_hard(&[
            msg("assistant", "```py\nprint(1)\n```"),
            msg("user", "explain"),
        ]));
    }

    #[test]
    fn long_context_is_hard() {
        let long = "word ".repeat(2_000); // 10k chars
        assert!(looks_hard(&[msg("user", &long)]));
    }

    #[test]
    fn user_markers_are_hard_but_assistant_markers_are_not() {
        assert!(looks_hard(&[msg(
            "user",
            "prove the sum formula step by step"
        )]));
        assert!(!looks_hard(&[
            msg("assistant", "let's go step by step"),
            msg("user", "thanks!"),
        ]));
    }

    #[test]
    fn default_keywords_can_be_removed_case_insensitively() {
        let config = HeuristicJudgeBackend {
            removed_hard_keywords: vec!["DeBuG".into()],
            ..Default::default()
        };
        assert!(!looks_hard_with_config(
            &[msg("user", "DEBUG this")],
            &config
        ));
        assert!(looks_hard_with_config(
            &[msg("user", "refactor this")],
            &config
        ));
    }

    #[test]
    fn added_keywords_match_substrings_and_phrases_case_insensitively() {
        let config = HeuristicJudgeBackend {
            added_hard_keywords: vec!["ÉTAPE".into(), "complex analysis".into()],
            ..Default::default()
        };
        assert!(looks_hard_with_config(&[msg("user", "étapes")], &config));
        assert!(looks_hard_with_config(
            &[msg("user", "COMPLEX ANALYSIS please")],
            &config
        ));
        assert!(!looks_hard_with_config(
            &[
                msg("system", "complex analysis"),
                msg("assistant", "étapes"),
                msg("tool", "étapes")
            ],
            &config,
        ));
    }

    #[test]
    fn default_keywords_can_be_disabled_without_disabling_additions() {
        let config = HeuristicJudgeBackend {
            use_default_keywords: Some(false),
            added_hard_keywords: vec!["custom signal".into()],
            ..Default::default()
        };
        assert!(!looks_hard_with_config(
            &[msg("user", "debug algorithm theorem")],
            &config
        ));
        assert!(looks_hard_with_config(
            &[msg("user", "custom signal")],
            &config
        ));
    }

    #[test]
    fn code_block_signal_can_be_disabled_for_every_role() {
        let config = HeuristicJudgeBackend {
            code_blocks_are_hard: Some(false),
            ..Default::default()
        };
        for role in ["system", "user", "assistant", "tool"] {
            let messages = [msg(role, "```\nhello\n```")];
            assert!(looks_hard(&messages));
            assert!(!looks_hard_with_config(&messages, &config));
        }
        assert!(looks_hard_with_config(
            &[msg("user", "debug this")],
            &config
        ));
    }

    #[test]
    fn length_cutoff_counts_utf8_bytes_across_roles_with_strict_boundary() {
        let config = HeuristicJudgeBackend {
            max_easy_input_bytes: Some(8),
            ..Default::default()
        };
        let mut messages = vec![msg("system", "🦀"), msg("assistant", "🦀")];
        assert!(!looks_hard_with_config(&messages, &config));
        messages.push(msg("tool", "x"));
        assert!(looks_hard_with_config(&messages, &config));
        assert!(!looks_hard(&[msg("user", &"x".repeat(6_000))]));
        assert!(looks_hard(&[msg("user", &"x".repeat(6_001))]));
    }

    #[test]
    fn zero_disables_only_the_length_signal() {
        let config = HeuristicJudgeBackend {
            max_easy_input_bytes: Some(0),
            ..Default::default()
        };
        assert!(!looks_hard_with_config(
            &[msg("user", &"x".repeat(10_000))],
            &config
        ));
        assert!(looks_hard_with_config(
            &[msg("user", "debug this")],
            &config
        ));
        assert!(looks_hard_with_config(&[msg("assistant", "```")], &config));
    }
}
