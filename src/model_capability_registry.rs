//! Curated, model-scoped capability facts.
//!
//! Rules are intentionally provider-specific and auditable. Generic name
//! heuristics belong in clients as unverified suggestions, never in server
//! capability truth.

use crate::capabilities::{
    CapabilityFactPatch, InputModality, Interface, InterfaceFactPatch, InterfaceOperation,
    ModelCapabilityPatch, ModelFeature, ModelSurface, OperationPatch, OutputModality,
    ParameterCapability, ParameterConstraint, Support,
};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelIdentity {
    pub provider_kind: String,
    pub upstream_model_id: String,
}

impl ModelIdentity {
    pub fn from_resolved(
        provider_kind: impl Into<String>,
        public_or_upstream_model_id: &str,
        connection_prefix: Option<&str>,
    ) -> Self {
        let upstream_model_id = connection_prefix
            .and_then(|prefix| public_or_upstream_model_id.strip_prefix(prefix))
            .unwrap_or(public_or_upstream_model_id)
            .to_string();
        Self {
            provider_kind: provider_kind.into(),
            upstream_model_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Matcher {
    Exact(&'static str),
    Prefix(&'static str),
    Suffix(&'static str),
}

impl Matcher {
    fn matches(self, model: &str) -> bool {
        match self {
            Self::Exact(value) => model == value,
            Self::Prefix(value) => model.starts_with(value),
            Self::Suffix(value) => model.ends_with(value),
        }
    }

    fn priority(self) -> (u8, usize) {
        match self {
            Self::Exact(value) => (3, value.len()),
            Self::Prefix(value) => (2, value.len()),
            Self::Suffix(value) => (1, value.len()),
        }
    }
}

#[derive(Clone, Copy)]
struct RegistryRule {
    provider_kind: &'static str,
    matcher: Matcher,
    name: &'static str,
    facts: fn() -> ModelCapabilityPatch,
    endpoint_facts: Option<fn() -> CapabilityFactPatch>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegistryMatch {
    pub rule_name: &'static str,
    pub facts: ModelCapabilityPatch,
    pub endpoint_facts: Option<CapabilityFactPatch>,
}

const RULES: &[RegistryRule] = &[
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Suffix("-chat-latest"),
        name: "openai.chat-latest-family",
        facts: chat,
        endpoint_facts: Some(chat_completions_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Exact("chatgpt-4o-latest"),
        name: "openai.chatgpt-4o-latest",
        facts: chat,
        endpoint_facts: Some(chat_completions_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-4o-search-preview"),
        name: "openai.search.gpt-4o-search-preview",
        facts: chat,
        endpoint_facts: Some(chat_completions_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-4o-mini-search-preview"),
        name: "openai.search.gpt-4o-mini-search-preview",
        facts: chat,
        endpoint_facts: Some(chat_completions_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-5-search-api"),
        name: "openai.search.gpt-5-search-api",
        facts: chat,
        endpoint_facts: Some(search_chat_completions_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-5-codex"),
        name: "openai.responses-only.gpt-5-codex",
        facts: chat,
        endpoint_facts: Some(responses_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-5.1-codex"),
        name: "openai.responses-only.gpt-5.1-codex",
        facts: chat,
        endpoint_facts: Some(responses_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-5.2-codex"),
        name: "openai.responses-only.gpt-5.2-codex",
        facts: chat,
        endpoint_facts: Some(responses_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-5.3-codex"),
        name: "openai.responses-only.gpt-5.3-codex",
        facts: chat,
        endpoint_facts: Some(responses_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-5-pro"),
        name: "openai.responses-only.gpt-5-pro",
        facts: chat,
        endpoint_facts: Some(responses_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-5.2-pro"),
        name: "openai.responses-only.gpt-5.2-pro",
        facts: chat,
        endpoint_facts: Some(responses_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-5.4-pro"),
        name: "openai.responses-only.gpt-5.4-pro",
        facts: chat,
        endpoint_facts: Some(responses_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-3.5-turbo-instruct"),
        name: "openai.instruct-family",
        facts: non_chat,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("davinci-"),
        name: "openai.davinci-family",
        facts: non_chat,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("babbage-"),
        name: "openai.babbage-family",
        facts: non_chat,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("text-moderation-"),
        name: "openai.text-moderation-family",
        facts: moderation,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("omni-moderation-"),
        name: "openai.moderation-family",
        facts: moderation,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-audio"),
        name: "openai.audio-chat-family",
        facts: audio_required_chat,
        endpoint_facts: Some(chat_completions_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-4o-audio-preview"),
        name: "openai.4o-audio-chat-family",
        facts: chat,
        endpoint_facts: Some(chat_completions_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-4o-mini-audio-preview"),
        name: "openai.4o-mini-audio-chat-family",
        facts: chat,
        endpoint_facts: Some(chat_completions_only),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Exact("gpt-image-2"),
        name: "openai.gpt-image-2",
        facts: image_generation,
        endpoint_facts: Some(gpt_image_2_endpoint),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-image-"),
        name: "openai.gpt-image-family",
        facts: image_generation,
        endpoint_facts: Some(gpt_image_family_endpoint),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Exact("dall-e-3"),
        name: "openai.dall-e-3",
        facts: image_generation,
        endpoint_facts: Some(dall_e_3_endpoint),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Exact("dall-e-2"),
        name: "openai.dall-e-2",
        facts: image_generation,
        endpoint_facts: Some(dall_e_2_endpoint),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("dall-e-"),
        name: "openai.dall-e-family",
        facts: image_generation,
        endpoint_facts: Some(image_generation_endpoint),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("text-embedding-"),
        name: "openai.text-embedding-family",
        facts: text_embeddings,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("tts-"),
        name: "openai.tts-family",
        facts: audio_speech,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Suffix("-tts"),
        name: "openai.tts-suffix",
        facts: audio_speech,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-4o-mini-tts-"),
        name: "openai.tts-snapshots",
        facts: audio_speech,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("whisper-"),
        name: "openai.whisper-family",
        facts: audio_transcription,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Suffix("-transcribe"),
        name: "openai.transcribe-suffix",
        facts: audio_transcription,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-4o-transcribe-"),
        name: "openai.transcribe-variants",
        facts: audio_transcription,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-4o-mini-transcribe-"),
        name: "openai.mini-transcribe-snapshots",
        facts: audio_transcription,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-realtime"),
        name: "openai.realtime-family",
        facts: realtime,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-4o-realtime-"),
        name: "openai.4o-realtime-family",
        facts: realtime,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-4o-mini-realtime-"),
        name: "openai.4o-mini-realtime-family",
        facts: realtime,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-live-"),
        name: "openai.live-family",
        facts: realtime,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("chatgpt-image-"),
        name: "openai.chatgpt-image-family",
        facts: image_generation,
        endpoint_facts: Some(image_generation_endpoint),
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("sora-"),
        name: "openai.video-family",
        facts: non_chat,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("o1"),
        name: "openai.o1-family",
        facts: reasoning_chat,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("o3"),
        name: "openai.o3-family",
        facts: reasoning_chat,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("o4"),
        name: "openai.o4-family",
        facts: reasoning_chat,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Exact("models/gemini-3-pro-image"),
        name: "gemini.gemini-3-pro-image",
        facts: image_generation,
        endpoint_facts: Some(gemini_3_pro_image_endpoint),
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Prefix("models/imagen-"),
        name: "gemini.imagen-family",
        facts: image_generation,
        endpoint_facts: Some(imagen_endpoint),
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Prefix("imagen-"),
        name: "gemini.imagen-family",
        facts: image_generation,
        endpoint_facts: Some(imagen_endpoint),
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Suffix("-image"),
        name: "gemini.image-output-family",
        facts: image_generation,
        endpoint_facts: Some(image_generation_endpoint),
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Suffix("-image-preview"),
        name: "gemini.image-preview-family",
        facts: image_generation,
        endpoint_facts: Some(image_generation_endpoint),
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Prefix("models/nano-banana-"),
        name: "gemini.nano-banana-family",
        facts: image_generation,
        endpoint_facts: Some(image_generation_endpoint),
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Suffix("-tts"),
        name: "gemini.tts-family",
        facts: audio_speech,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Suffix("-tts-preview"),
        name: "gemini.tts-preview-family",
        facts: audio_speech,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Prefix("models/gemini-2.5-flash-native-audio-"),
        name: "gemini.native-audio-family",
        facts: realtime,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Suffix("-live-preview"),
        name: "gemini.live-preview-family",
        facts: realtime,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Prefix("models/gemini-3.5-live-translate-"),
        name: "gemini.live-translation-family",
        facts: realtime,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Prefix("models/gemini-3.8-live"),
        name: "gemini.gemini-3.8-live-family",
        facts: realtime,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Suffix("-transcribe"),
        name: "gemini.transcribe-family",
        facts: audio_transcription,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Suffix("-transcribe-live"),
        name: "gemini.live-transcribe-family",
        facts: realtime,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Prefix("models/lyria-"),
        name: "gemini.music-family",
        facts: non_chat,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Prefix("models/veo-"),
        name: "gemini.veo-family",
        facts: non_chat,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Prefix("models/gemini-omni-"),
        name: "gemini.omni-video-family",
        facts: non_chat,
        endpoint_facts: None,
    },
    // Deep Research uses the Interactions agent workflow, which the router
    // does not currently support; it must not inherit the generic chat default.
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Prefix("models/deep-research-"),
        name: "gemini.deep-research-agent-family",
        facts: non_chat,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("computer-use-preview"),
        name: "openai.computer-use-preview-family",
        facts: non_chat,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Prefix("models/gemini-2.5-computer-use-preview"),
        name: "gemini.computer-use-preview-family",
        facts: non_chat,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Exact("models/gemini-robotics-er-2-streaming-preview"),
        name: "gemini.robotics-live",
        facts: realtime,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Exact("models/gemini-embedding-2"),
        name: "gemini.multimodal-embedding-2",
        facts: gemini_2_embeddings,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Exact("models/gemini-embedding-2-preview"),
        name: "gemini.multimodal-embedding-2-preview",
        facts: gemini_2_embeddings,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Prefix("models/gemini-embedding-"),
        name: "gemini.embedding-family",
        facts: gemini_text_embeddings,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "gemini",
        matcher: Matcher::Prefix("models/text-embedding-"),
        name: "gemini.text-embedding-family",
        facts: gemini_text_embeddings,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "anthropic",
        matcher: Matcher::Prefix("claude-"),
        name: "anthropic.claude-family",
        facts: chat,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "bedrock",
        matcher: Matcher::Prefix("anthropic.claude"),
        name: "bedrock.anthropic-claude-family",
        facts: chat,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "voyage",
        matcher: Matcher::Prefix("voyage-"),
        name: "voyage.embedding-family",
        facts: text_embeddings,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "openai",
        matcher: Matcher::Prefix("gpt-6-luna"),
        name: "openai.decisions.gpt-6-luna",
        facts: openai_luna,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "perplexity",
        matcher: Matcher::Prefix("pplx-decider"),
        name: "perplexity.decider-family",
        facts: perplexity_decider,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "typesafe",
        matcher: Matcher::Prefix("jev-1.13"),
        name: "typesafe.jev-1.13",
        facts: jev_without_boxes,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "typesafe",
        matcher: Matcher::Exact("jev-latest"),
        name: "typesafe.jev-latest",
        facts: jev_without_boxes,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "typesafe",
        matcher: Matcher::Prefix("jev-1.14"),
        name: "typesafe.jev-1.14",
        facts: jev_with_boxes,
        endpoint_facts: None,
    },
    RegistryRule {
        provider_kind: "typesafe",
        matcher: Matcher::Exact("jev-preview"),
        name: "typesafe.jev-preview",
        facts: jev_with_boxes,
        endpoint_facts: None,
    },
];

/// Maps an OpenRouter model id's leading `provider/` segment onto the
/// registry's canonical provider kind and the remaining nested model id (e.g.
/// `"google/gemini-2.5-pro"` -> `("gemini", "gemini-2.5-pro")`). `None` when
/// `model` has no `provider/model` shape.
///
/// This is the single source of truth for "what nested provider does this
/// OpenRouter model belong to" — shared by [`lookup`]'s nested-provider
/// fallback below and by `capabilities::openrouter_capabilities`'s
/// Anthropic-passthrough heuristic, so the two never disagree about which
/// OpenRouter models are "really" Anthropic (or Gemini, xAI, Mistral, ...)
/// models.
pub(crate) fn openrouter_nested_provider(model: &str) -> Option<(&str, &str)> {
    let (nested_provider, nested_model) = model.split_once('/')?;
    let registry_provider = match nested_provider {
        "google" => "gemini",
        "x-ai" => "xai",
        "mistralai" => "mistral",
        other => other,
    };
    Some((registry_provider, nested_model))
}

/// [`RULES`] indexed by `provider_kind`, computed once on first use. [`lookup`]
/// is called on every capability resolution, so this avoids a full linear
/// scan of every rule (across every provider) per call.
fn rules_by_provider() -> &'static HashMap<&'static str, Vec<&'static RegistryRule>> {
    static INDEX: OnceLock<HashMap<&'static str, Vec<&'static RegistryRule>>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut index: HashMap<&'static str, Vec<&'static RegistryRule>> = HashMap::new();
        for rule in RULES {
            index.entry(rule.provider_kind).or_default().push(rule);
        }
        index
    })
}

pub fn lookup(provider_kind: &str, model: &str) -> Option<RegistryMatch> {
    // Gemini accepts both catalog resource names and bare model names.
    if provider_kind == "gemini" && !model.starts_with("models/") {
        return lookup(provider_kind, &format!("models/{model}"));
    }
    if provider_kind == "openrouter" {
        if crate::decisions::is_openrouter_decision_model(model) {
            return Some(RegistryMatch {
                rule_name: "openrouter.decision-model",
                facts: openrouter_decisions(model),
                endpoint_facts: None,
            });
        }
        if let Some((registry_provider, nested_model)) = openrouter_nested_provider(model) {
            if registry_provider == "typesafe" {
                return None;
            }
            return lookup(registry_provider, nested_model)
                .or_else(|| {
                    (crate::provider_registry::is_builtin(registry_provider)
                        || matches!(registry_provider, "meta-llama" | "deepseek"))
                    .then(|| RegistryMatch {
                        rule_name: "openrouter.nested-provider-default",
                        facts: if matches!(registry_provider, "meta-llama" | "deepseek") {
                            chat()
                        } else {
                            provider_default(registry_provider)
                        },
                        endpoint_facts: None,
                    })
                })
                .map(|matched| RegistryMatch {
                    rule_name: matched.rule_name,
                    facts: matched.facts,
                    // Nested provider rules describe the model sold through
                    // OpenRouter, not OpenRouter's endpoint contract.
                    endpoint_facts: None,
                });
        }
    }

    let mut matches: Vec<_> = rules_by_provider()
        .get(provider_kind)
        .into_iter()
        .flatten()
        .filter(|rule| rule.matcher.matches(model))
        .collect();
    matches.sort_by_key(|rule| std::cmp::Reverse(rule.matcher.priority()));
    let best = **matches.first()?;
    Some(RegistryMatch {
        rule_name: best.name,
        facts: (best.facts)(),
        endpoint_facts: best.endpoint_facts.map(|facts| facts()),
    })
}

/// Provider kinds with no curated [`RULES`] entry that nonetheless default to
/// "this looks like a chat model" (see [`provider_default`]). Kept as an
/// explicit, named set (rather than inline in `provider_default`'s match) so
/// a coverage test can assert every builtin provider kind is accounted for
/// either by a rule or by an explicit default decision — see
/// `every_builtin_provider_kind_has_a_rule_or_an_explicit_default` below.
const GENERIC_CHAT_DEFAULT_KINDS: &[&str] = &[
    "openai",
    "anthropic",
    "gemini",
    "vertex",
    "bedrock",
    "xai",
    "cohere",
    "qwen",
    "fireworks",
    "together",
    "groq",
    "mistral",
    "perplexity",
];

/// Provider kinds with no curated [`RULES`] entry and no chat-default
/// assumption: connections declare their own capabilities (`vllm`) or the
/// upstream model is itself arbitrary (`openrouter`, whose own facts come
/// from its nested-provider lookup above, not from this generic default), so
/// [`provider_default`] conservatively falls through to `unknown_chat()` for
/// them. Not read by `provider_default` itself (its `else` branch already
/// covers this case) -- kept as an explicit, named, test-only set so
/// `every_builtin_provider_kind_has_a_rule_or_an_explicit_default` can
/// distinguish "intentionally unknown" from "forgotten".
#[cfg(test)]
const GENERIC_UNKNOWN_DEFAULT_KINDS: &[&str] = &["vllm", "openrouter"];

/// Providers whose catalog is authoritative for model names and whose models
/// all share the same decision surface.
#[cfg(test)]
const GENERIC_DECISIONS_DEFAULT_KINDS: &[&str] = &["typesafe"];

pub fn provider_default(provider_kind: &str) -> ModelCapabilityPatch {
    if GENERIC_CHAT_DEFAULT_KINDS.contains(&provider_kind) {
        chat()
    } else if provider_kind == "voyage" {
        text_embeddings()
    } else if provider_kind == "typesafe" {
        jev_prompting()
    } else {
        unknown_chat()
    }
}

fn chat_completions_only() -> CapabilityFactPatch {
    chat_endpoints(Support::Supported, Support::Unsupported)
}

fn search_chat_completions_only() -> CapabilityFactPatch {
    let mut facts = chat_completions_only();
    let operation = facts
        .interfaces
        .get_mut(&Interface::OpenaiChatCompletions)
        .unwrap()
        .operations
        .get_mut(&InterfaceOperation::Create)
        .unwrap();
    for parameter in [
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
    ] {
        operation.parameters.insert(
            parameter.to_string(),
            ParameterCapability {
                support: Support::Unsupported,
                constraint: None,
            },
        );
    }
    facts
}

fn responses_only() -> CapabilityFactPatch {
    chat_endpoints(Support::Unsupported, Support::Supported)
}

fn chat_endpoints(chat: Support, responses: Support) -> CapabilityFactPatch {
    CapabilityFactPatch {
        interfaces: [
            (Interface::OpenaiChatCompletions, chat),
            (Interface::OpenaiResponses, responses),
        ]
        .into_iter()
        .map(|(interface, support)| {
            (
                interface,
                InterfaceFactPatch {
                    operations: BTreeMap::from([(
                        InterfaceOperation::Create,
                        OperationPatch {
                            support: Some(support),
                            ..Default::default()
                        },
                    )]),
                },
            )
        })
        .collect(),
        ..Default::default()
    }
}

fn model(
    surface: ModelSurface,
    input: InputModality,
    output: OutputModality,
) -> ModelCapabilityPatch {
    ModelCapabilityPatch {
        surfaces: BTreeMap::from([(surface, Support::Supported)]),
        modalities: crate::capabilities::ModalitiesPatch {
            input: BTreeMap::from([(input, Support::Supported)]),
            output: BTreeMap::from([(output, Support::Supported)]),
        },
        ..Default::default()
    }
}

fn chat() -> ModelCapabilityPatch {
    model(
        ModelSurface::Chat,
        InputModality::Text,
        OutputModality::Text,
    )
}

/// A decision model on its native decisions surface: no chat. Answers are a
/// few tokens per question, so there is no output ceiling to reserve against.
fn decisions() -> ModelCapabilityPatch {
    let mut facts = model(
        ModelSurface::Decisions,
        InputModality::Text,
        OutputModality::Decisions,
    );
    facts
        .surfaces
        .insert(ModelSurface::Chat, Support::Unsupported);
    facts.limits.max_output_tokens = Some(None);
    facts
}

/// A decision model sold through OpenRouter. Window and image input come from
/// its catalog once discovered; only Jev's are known statically. OpenRouter
/// documents box questions as Jev 1.14+ only.
fn openrouter_decisions(model: &str) -> ModelCapabilityPatch {
    let mut facts = decisions();
    let boxes = if crate::decisions::is_openrouter_jev_version(model) {
        facts.limits.context_window = Some(Some(262_144));
        facts.limits.max_input_tokens = Some(Some(262_144));
        facts
            .modalities
            .input
            .insert(InputModality::Image, Support::Supported);
        crate::decisions::jev_answers_boxes(model).unwrap_or(false)
    } else {
        false
    };
    facts
        .features
        .insert(ModelFeature::DecisionBoxes, support(boxes));
    facts
}

fn support(supported: bool) -> Support {
    if supported {
        Support::Supported
    } else {
        Support::Unsupported
    }
}

/// OpenAI's `gpt-6-luna`: a chat model that also answers OpenAI Decisions,
/// with text and image evidence and no box questions.
fn openai_luna() -> ModelCapabilityPatch {
    let mut facts = chat();
    facts
        .surfaces
        .insert(ModelSurface::Decisions, Support::Supported);
    facts
        .modalities
        .output
        .insert(OutputModality::Decisions, Support::Supported);
    facts
        .modalities
        .input
        .insert(InputModality::Image, Support::Supported);
    facts
        .features
        .insert(ModelFeature::DecisionBoxes, Support::Unsupported);
    facts
}

/// Perplexity Decider: 256K-token input, images inline in `state`, no box.
fn perplexity_decider() -> ModelCapabilityPatch {
    let mut facts = decisions();
    facts.limits.context_window = Some(Some(262_144));
    facts.limits.max_input_tokens = Some(Some(262_144));
    facts
        .modalities
        .input
        .insert(InputModality::Image, Support::Supported);
    facts
        .features
        .insert(ModelFeature::DecisionBoxes, Support::Unsupported);
    facts
}

/// TypeSafe Jev takes images on every release; box questions need 1.14.
fn jev_with_boxes() -> ModelCapabilityPatch {
    jev_release(true)
}

fn jev_without_boxes() -> ModelCapabilityPatch {
    jev_release(false)
}

fn jev_release(boxes: bool) -> ModelCapabilityPatch {
    let mut facts = jev_prompting();
    // Images arrive as System One assets; Jev prompting still refuses chat
    // image parts at the endpoint layer.
    facts
        .modalities
        .input
        .insert(InputModality::Image, Support::Supported);
    facts
        .features
        .insert(ModelFeature::DecisionBoxes, support(boxes));
    facts
}

/// A System One model served directly by TypeSafe, where the router also
/// prompts it through chat completions (`jev_prompting`): the chat surface
/// exists, but only as structured output. Its answers are the schema's JSON
/// object, and it never calls tools.
fn jev_prompting() -> ModelCapabilityPatch {
    let mut facts = decisions();
    facts.limits = crate::capabilities::ModelLimitsPatch {
        context_window: Some(Some(64_000)),
        max_input_tokens: Some(Some(64_000)),
        max_output_tokens: Some(None),
        ..Default::default()
    };
    facts
        .surfaces
        .insert(ModelSurface::Chat, Support::Supported);
    facts
        .modalities
        .output
        .insert(OutputModality::Text, Support::Supported);
    facts
        .features
        .insert(ModelFeature::StructuredOutput, Support::Supported);
    facts
        .features
        .insert(ModelFeature::StructuredOutputOnly, Support::Supported);
    facts
        .features
        .insert(ModelFeature::TextOnlyChat, Support::Supported);
    for feature in [
        ModelFeature::ToolCalling,
        ModelFeature::ParallelToolCalling,
        ModelFeature::StrictToolSchema,
    ] {
        facts.features.insert(feature, Support::Unsupported);
    }
    facts
}

fn audio_required_chat() -> ModelCapabilityPatch {
    let mut facts = chat();
    facts
        .features
        .insert(ModelFeature::TextOnlyChat, Support::Unsupported);
    facts
        .modalities
        .input
        .insert(InputModality::Audio, Support::Supported);
    facts
        .modalities
        .output
        .insert(OutputModality::Audio, Support::Supported);
    facts
}

fn unknown_chat() -> ModelCapabilityPatch {
    ModelCapabilityPatch {
        surfaces: BTreeMap::from([(ModelSurface::Chat, Support::Unknown)]),
        modalities: crate::capabilities::ModalitiesPatch {
            input: BTreeMap::from([(InputModality::Text, Support::Unknown)]),
            output: BTreeMap::from([(OutputModality::Text, Support::Unknown)]),
        },
        ..Default::default()
    }
}

fn moderation() -> ModelCapabilityPatch {
    let mut facts = non_chat();
    facts
        .surfaces
        .insert(ModelSurface::Moderation, Support::Supported);
    facts
        .modalities
        .input
        .insert(InputModality::Text, Support::Supported);
    facts
}

// Known families without text-chat support must not inherit the provider's
// generic chat default. Other capabilities remain unspecified.
fn non_chat() -> ModelCapabilityPatch {
    ModelCapabilityPatch {
        surfaces: BTreeMap::from([(ModelSurface::Chat, Support::Unsupported)]),
        ..Default::default()
    }
}

fn reasoning_chat() -> ModelCapabilityPatch {
    let mut facts = chat();
    facts
        .features
        .insert(ModelFeature::Reasoning, Support::Supported);
    facts
}

fn image_generation() -> ModelCapabilityPatch {
    let mut facts = model(
        ModelSurface::ImageGeneration,
        InputModality::Text,
        OutputModality::Image,
    );
    facts
        .surfaces
        .insert(ModelSurface::Chat, Support::Unsupported);
    facts
}

fn image_generation_endpoint() -> CapabilityFactPatch {
    image_endpoint_parameters([])
}

fn imagen_endpoint() -> CapabilityFactPatch {
    // Imagen uses the images endpoint, not Gemini Interactions. Keep this
    // model-scoped so clients can choose the endpoint even through aliases.
    image_generation_endpoint().with_operation_support(
        Interface::GeminiInteractions,
        InterfaceOperation::Create,
        Support::Unsupported,
    )
}

fn gpt_image_2_endpoint() -> CapabilityFactPatch {
    image_endpoint_parameters([
        (
            "size",
            ParameterConstraint::StringEnum {
                values: strings(["1024x1024", "1536x1024", "1024x1536"]),
            },
        ),
        (
            "quality",
            ParameterConstraint::StringEnum {
                values: strings(["auto", "low", "medium", "high"]),
            },
        ),
        ("n", ParameterConstraint::IntegerRange { min: 1, max: 10 }),
    ])
}

fn gpt_image_family_endpoint() -> CapabilityFactPatch {
    image_endpoint_parameters([
        (
            "size",
            ParameterConstraint::StringEnum {
                values: strings(["auto", "1024x1024", "1536x1024", "1024x1536"]),
            },
        ),
        (
            "quality",
            ParameterConstraint::StringEnum {
                values: strings(["auto", "low", "medium", "high"]),
            },
        ),
        ("n", ParameterConstraint::IntegerRange { min: 1, max: 10 }),
    ])
}

fn dall_e_3_endpoint() -> CapabilityFactPatch {
    image_endpoint_parameters([
        (
            "size",
            ParameterConstraint::StringEnum {
                values: strings(["1024x1024", "1792x1024", "1024x1792"]),
            },
        ),
        (
            "quality",
            ParameterConstraint::StringEnum {
                values: strings(["standard", "hd"]),
            },
        ),
        ("n", ParameterConstraint::IntegerRange { min: 1, max: 1 }),
    ])
}

fn dall_e_2_endpoint() -> CapabilityFactPatch {
    image_endpoint_parameters([
        (
            "size",
            ParameterConstraint::StringEnum {
                values: strings(["256x256", "512x512", "1024x1024"]),
            },
        ),
        (
            "quality",
            ParameterConstraint::StringEnum {
                values: strings(["standard"]),
            },
        ),
        ("n", ParameterConstraint::IntegerRange { min: 1, max: 10 }),
    ])
}

fn gemini_3_pro_image_endpoint() -> CapabilityFactPatch {
    // Google's OpenAI-compatible images endpoint accepts `size`, not the
    // native API's resolution tier. Google normalizes the output dimensions.
    image_endpoint_parameters([(
        "size",
        ParameterConstraint::StringEnum {
            values: strings(["1024x1024", "1792x1024", "1024x1792"]),
        },
    )])
}

fn image_endpoint_parameters<const N: usize>(
    parameters: [(&'static str, ParameterConstraint); N],
) -> CapabilityFactPatch {
    CapabilityFactPatch {
        interfaces: BTreeMap::from([(
            Interface::OpenaiImages,
            InterfaceFactPatch {
                operations: BTreeMap::from([(
                    InterfaceOperation::Generate,
                    OperationPatch {
                        support: Some(Support::Supported),
                        parameters: parameters
                            .into_iter()
                            .map(|(name, constraint)| {
                                (
                                    name.to_string(),
                                    ParameterCapability {
                                        support: Support::Supported,
                                        constraint: Some(constraint),
                                    },
                                )
                            })
                            .collect(),
                        ..OperationPatch::default()
                    },
                )]),
            },
        )]),
        ..CapabilityFactPatch::default()
    }
}

fn strings<const N: usize>(values: [&'static str; N]) -> Vec<String> {
    values.into_iter().map(str::to_string).collect()
}

fn text_embeddings() -> ModelCapabilityPatch {
    let mut facts = model(
        ModelSurface::Embeddings,
        InputModality::Text,
        OutputModality::Embeddings,
    );
    facts
        .surfaces
        .insert(ModelSurface::Chat, Support::Unsupported);
    facts
}

fn gemini_2_embeddings() -> ModelCapabilityPatch {
    let mut facts = text_embeddings();
    facts
        .modalities
        .input
        .insert(InputModality::Image, Support::Supported);
    let limits = &crate::embedding_limits::GEMINI_EMBEDDING_2_LIMITS;
    facts.limits.max_input_tokens = Some(Some(limits.max_input_tokens));
    facts.limits.embedding_input = Some(Some(limits.capability_limits()));
    facts
}

fn gemini_text_embeddings() -> ModelCapabilityPatch {
    let mut facts = text_embeddings();
    facts
        .modalities
        .input
        .insert(InputModality::Image, Support::Unsupported);
    facts
}

fn audio_speech() -> ModelCapabilityPatch {
    let mut facts = model(
        ModelSurface::AudioSpeech,
        InputModality::Text,
        OutputModality::Audio,
    );
    facts
        .surfaces
        .insert(ModelSurface::Chat, Support::Unsupported);
    facts
}

fn audio_transcription() -> ModelCapabilityPatch {
    let mut facts = model(
        ModelSurface::AudioTranscription,
        InputModality::Audio,
        OutputModality::Text,
    );
    facts
        .surfaces
        .insert(ModelSurface::Chat, Support::Unsupported);
    facts
}

fn realtime() -> ModelCapabilityPatch {
    let mut facts = model(
        ModelSurface::Realtime,
        InputModality::Audio,
        OutputModality::Audio,
    );
    facts
        .surfaces
        .insert(ModelSurface::Chat, Support::Unsupported);
    facts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_gemini_2_models_publish_embedding_input_limits() {
        for model in [
            "gemini-embedding-2",
            "models/gemini-embedding-2",
            "gemini-embedding-2-preview",
            "models/gemini-embedding-2-preview",
        ] {
            let resolved = crate::capabilities::resolve("gemini", model, None, None).unwrap();
            assert_eq!(resolved.profile.limits.max_input_tokens, Some(8192));
            assert_eq!(
                serde_json::to_value(resolved.profile.limits.embedding_input).unwrap(),
                serde_json::json!({"max_images_per_document": 6, "image_mime_types": ["image/png", "image/jpeg"]})
            );
        }
        // OpenRouter inherits model facts from its nested provider as before;
        // this does not make it use Gemini's execution or quota admission path.
        let relay =
            crate::capabilities::resolve("openrouter", "google/gemini-embedding-2", None, None)
                .unwrap();
        assert_eq!(relay.profile.limits.max_input_tokens, Some(8192));
        assert_eq!(
            relay.profile.limits.embedding_input,
            crate::capabilities::resolve("gemini", "gemini-embedding-2", None, None)
                .unwrap()
                .profile
                .limits
                .embedding_input
        );
        for (provider, model) in [
            ("gemini", "gemini-embedding-001"),
            ("gemini", "gemini-embedding-2-custom"),
            ("gemini", "gemini-embedding-20"),
            ("gemini", "text-embedding-004"),
            ("openai-compatible", "gemini-embedding-2"),
            ("voyage", "voyage-multimodal-3"),
        ] {
            let resolved = crate::capabilities::resolve(provider, model, None, None).unwrap();
            assert!(
                resolved.profile.limits.embedding_input.is_none(),
                "{provider}/{model}"
            );
        }
    }

    #[test]
    fn canonical_identity_removes_only_the_resolved_connection_prefix() {
        assert_eq!(
            ModelIdentity::from_resolved(
                "openrouter",
                "openrouter/anthropic/claude-sonnet-4",
                Some("openrouter/")
            ),
            ModelIdentity {
                provider_kind: "openrouter".to_string(),
                upstream_model_id: "anthropic/claude-sonnet-4".to_string(),
            }
        );
        assert_eq!(
            ModelIdentity::from_resolved(
                "gemini",
                "gemini/models/gemini-3-pro-image",
                Some("gemini/")
            )
            .upstream_model_id,
            "models/gemini-3-pro-image"
        );
    }

    #[test]
    fn required_fixtures_resolve_to_model_scoped_facts() {
        for (provider, model, surface) in [
            ("openai", "gpt-image-2", ModelSurface::ImageGeneration),
            ("openai", "text-embedding-3-large", ModelSurface::Embeddings),
            (
                "gemini",
                "models/gemini-3-pro-image",
                ModelSurface::ImageGeneration,
            ),
            (
                "gemini",
                "models/gemini-embedding-001",
                ModelSurface::Embeddings,
            ),
            ("bedrock", "anthropic.claude-haiku-4-5", ModelSurface::Chat),
            (
                "openrouter",
                "anthropic/claude-sonnet-4",
                ModelSurface::Chat,
            ),
        ] {
            let matched = lookup(provider, model).expect("registry match");
            assert_eq!(matched.facts.surfaces[&surface], Support::Supported);
        }
    }

    #[test]
    fn dedicated_models_are_not_suggested_for_text_chat() {
        for (provider, models) in [
            (
                "openai",
                &[
                    "whisper-1",
                    "tts-1",
                    "gpt-4o-mini-tts",
                    "gpt-4o-mini-tts-2025-12-15",
                    "gpt-4o-transcribe",
                    "gpt-4o-transcribe-diarize",
                    "gpt-4o-mini-transcribe-2025-12-15",
                    "gpt-realtime",
                    "gpt-realtime-mini-2025-12-15",
                    "gpt-4o-realtime-preview",
                    "gpt-4o-mini-realtime-preview-2024-12-17",
                    "gpt-live-1",
                    "gpt-image-1",
                    "chatgpt-image-latest",
                    "dall-e-3",
                    "sora-2",
                ][..],
            ),
            (
                "gemini",
                &[
                    "models/gemini-2.5-flash-preview-tts",
                    "models/gemini-2.5-pro-preview-tts",
                    "models/gemini-3.1-flash-tts-preview",
                    "models/gemini-2.5-flash-native-audio-latest",
                    "models/gemini-2.5-flash-native-audio-preview-12-2025",
                    "models/gemini-3.1-flash-live-preview",
                    "models/gemini-3.5-live-translate-preview",
                    "models/gemini-3.8-live",
                    "models/gemini-3.8-live-extended-thinking",
                    "models/gemini-3.5-transcribe",
                    "models/gemini-3.5-transcribe-live",
                    "models/gemini-2.5-flash-image",
                    "models/gemini-3-pro-image",
                    "models/gemini-3-pro-image-preview",
                    "models/gemini-3.1-flash-image-preview",
                    "models/nano-banana-pro-preview",
                    "models/lyria-3-pro-preview",
                    "models/lyria-realtime-exp",
                    "models/veo-3.1-generate-preview",
                    "models/gemini-omni-1.1-flash",
                    "models/deep-research-pro-preview-12-2025",
                    "models/deep-research-preview-04-2026",
                    "models/deep-research-max-preview-04-2026",
                ][..],
            ),
        ] {
            for model in models {
                let resolved = crate::capabilities::resolve(provider, model, None, None).unwrap();
                assert_eq!(
                    crate::capabilities::ClientCapabilityProjection::playground(&resolved.profile)
                        .chat_support,
                    Support::Unsupported,
                    "{provider}/{model}",
                );
            }
        }
    }

    #[test]
    fn text_chat_and_dedicated_surfaces_remain_available() {
        for (provider, model, surface, input, output) in [
            (
                "openai",
                "gpt-4o-mini",
                ModelSurface::Chat,
                InputModality::Text,
                OutputModality::Text,
            ),
            (
                "openai",
                "o3-mini",
                ModelSurface::Chat,
                InputModality::Text,
                OutputModality::Text,
            ),
            (
                "gemini",
                "models/gemini-3.6-flash",
                ModelSurface::Chat,
                InputModality::Text,
                OutputModality::Text,
            ),
            (
                "gemini",
                "models/gemini-3.8-live",
                ModelSurface::Realtime,
                InputModality::Audio,
                OutputModality::Audio,
            ),
            (
                "gemini",
                "models/gemini-3.8-live-extended-thinking",
                ModelSurface::Realtime,
                InputModality::Audio,
                OutputModality::Audio,
            ),
            (
                "anthropic",
                "claude-sonnet-4",
                ModelSurface::Chat,
                InputModality::Text,
                OutputModality::Text,
            ),
            (
                "openai",
                "gpt-image-1",
                ModelSurface::ImageGeneration,
                InputModality::Text,
                OutputModality::Image,
            ),
            (
                "gemini",
                "models/gemini-3-pro-image-preview",
                ModelSurface::ImageGeneration,
                InputModality::Text,
                OutputModality::Image,
            ),
            (
                "openai",
                "tts-1",
                ModelSurface::AudioSpeech,
                InputModality::Text,
                OutputModality::Audio,
            ),
            (
                "openai",
                "whisper-1",
                ModelSurface::AudioTranscription,
                InputModality::Audio,
                OutputModality::Text,
            ),
            (
                "gemini",
                "models/gemini-embedding-001",
                ModelSurface::Embeddings,
                InputModality::Text,
                OutputModality::Embeddings,
            ),
        ] {
            let profile = crate::capabilities::resolve(provider, model, None, None)
                .unwrap()
                .profile;
            assert_eq!(
                profile.surfaces[&surface],
                Support::Supported,
                "{provider}/{model}"
            );
            assert_eq!(
                profile.modalities.input[&input],
                Support::Supported,
                "{provider}/{model}"
            );
            assert_eq!(
                profile.modalities.output[&output],
                Support::Supported,
                "{provider}/{model}"
            );
        }
        // Built-in provider rules must not classify arbitrary custom model IDs.
        let custom =
            crate::capabilities::resolve("openai-compatible", "my-tts", None, None).unwrap();
        assert_eq!(
            custom.profile.surfaces[&ModelSurface::Chat],
            Support::Unknown
        );
    }

    #[test]
    fn required_audio_and_tools_are_excluded_from_text_only_playground() {
        use crate::capabilities::{ClientCapabilityProjection, Protocol};
        for (provider, model) in [
            ("openai", "gpt-audio"),
            ("openai", "gpt-audio-1.5"),
            ("openai", "gpt-audio-mini"),
            ("openai", "gpt-audio-mini-2025-12-15"),
            ("openai", "computer-use-preview"),
            ("openai", "computer-use-preview-2025-03-11"),
            ("gemini", "models/gemini-2.5-computer-use-preview-10-2025"),
            ("gemini", "models/gemini-robotics-er-2-streaming-preview"),
        ] {
            let profile = crate::capabilities::resolve(provider, model, None, None)
                .unwrap()
                .profile;
            assert_eq!(
                ClientCapabilityProjection::playground(&profile).chat_support,
                Support::Unsupported,
                "{provider}/{model}"
            );
        }
        let audio = crate::capabilities::resolve("openai", "gpt-audio", None, None)
            .unwrap()
            .profile;
        // Audio remains a Chat Completions model, not a speech-generation model.
        assert_eq!(audio.surfaces[&ModelSurface::Chat], Support::Supported);
        assert_ne!(
            audio.surfaces.get(&ModelSurface::AudioSpeech),
            Some(&Support::Supported)
        );
        assert_eq!(
            audio
                .operation(
                    Protocol::OpenaiChatCompletions,
                    crate::capabilities::ProtocolOperation::Create
                )
                .unwrap()
                .support,
            Support::Supported
        );
        let robotics = crate::capabilities::resolve(
            "gemini",
            "models/gemini-robotics-er-2-preview",
            None,
            None,
        )
        .unwrap()
        .profile;
        assert_eq!(
            ClientCapabilityProjection::playground(&robotics).chat_support,
            Support::Supported
        );
    }

    #[test]
    fn search_models_publish_unsupported_sampling_parameters() {
        for model in ["gpt-5-search-api", "gpt-5-search-api-2025-10-14"] {
            let profile = crate::capabilities::resolve("openai", model, None, None)
                .unwrap()
                .profile;
            let operation = &profile.interfaces[&Interface::OpenaiChatCompletions].operations
                [&InterfaceOperation::Create];
            for parameter in [
                "temperature",
                "top_p",
                "frequency_penalty",
                "presence_penalty",
            ] {
                assert_eq!(
                    operation.parameters[parameter].support,
                    Support::Unsupported,
                    "{model}: {parameter}"
                );
            }
        }
    }

    #[test]
    fn endpoint_overrides_and_provider_defaults_control_chat_eligibility() {
        use crate::capabilities::{ClientCapabilityProjection, Protocol};
        for model in [
            "gpt-4o-audio-preview",
            "gpt-4o-mini-audio-preview",
            "gpt-4o-search-preview",
            "gpt-4o-search-preview-2025-03-11",
            "gpt-4o-mini-search-preview",
            "gpt-4o-mini-search-preview-2025-03-11",
            "gpt-5-search-api",
            "gpt-5-search-api-2025-10-14",
        ] {
            let profile = crate::capabilities::resolve("openai", model, None, None)
                .unwrap()
                .profile;
            assert_eq!(
                ClientCapabilityProjection::playground(&profile).chat_protocol,
                Some(Protocol::OpenaiChatCompletions),
                "{model}"
            );
            assert_eq!(
                profile.interfaces[&Interface::OpenaiResponses].operations
                    [&InterfaceOperation::Create]
                    .support,
                Support::Unsupported,
                "{model}"
            );
        }
        // Newly listed models inherit the provider default without an allowlist.
        for (provider, model) in [
            ("openai", "new-chat-model"),
            ("gemini", "models/new-chat-model"),
        ] {
            assert!(lookup(provider, model).is_none());
            let profile = crate::capabilities::resolve(provider, model, None, None)
                .unwrap()
                .profile;
            assert_eq!(profile.surfaces[&ModelSurface::Chat], Support::Supported);
            assert_eq!(
                ClientCapabilityProjection::playground(&profile).chat_protocol,
                Some(Protocol::OpenaiResponses)
            );
        }
        for model in [
            "gpt-3.5-turbo-instruct",
            "gpt-3.5-turbo-instruct-0914",
            "davinci-002",
            "babbage-002",
            "omni-moderation-latest",
        ] {
            let profile = crate::capabilities::resolve("openai", model, None, None)
                .unwrap()
                .profile;
            assert_eq!(
                ClientCapabilityProjection::playground(&profile).chat_protocol,
                None,
                "{model}"
            );
        }
        for model in ["gpt-5-codex", "gpt-5.1-codex-max", "gpt-5.2-pro-2025-12-11"] {
            let profile = crate::capabilities::resolve("openai", model, None, None)
                .unwrap()
                .profile;
            assert_eq!(
                ClientCapabilityProjection::playground(&profile).chat_protocol,
                Some(Protocol::OpenaiResponses)
            );
            assert_eq!(
                profile.interfaces[&Interface::OpenaiChatCompletions].operations
                    [&InterfaceOperation::Create]
                    .support,
                Support::Unsupported
            );
        }
        assert!(!model(
            ModelSurface::ImageGeneration,
            InputModality::Text,
            OutputModality::Image,
        )
        .surfaces
        .contains_key(&ModelSurface::Chat));
    }

    #[test]
    fn cross_matcher_overlap_has_deterministic_precedence() {
        let matched = lookup("openai", "tts-1-tts").unwrap();
        assert_eq!(matched.rule_name, "openai.tts-family");
    }

    #[test]
    fn same_priority_rules_are_unambiguous() {
        for (index, left) in RULES.iter().enumerate() {
            for right in &RULES[index + 1..] {
                if left.provider_kind == right.provider_kind
                    && left.matcher.priority() == right.matcher.priority()
                {
                    assert!(
                        !matchers_overlap(left.matcher, right.matcher),
                        "ambiguous capability registry rules: {} and {}",
                        left.name,
                        right.name
                    );
                }
            }
        }
    }

    #[test]
    fn openrouter_maps_known_vendor_namespaces_without_broadening_unknown_defaults() {
        for model in [
            "google/gemini-2.5-pro",
            "x-ai/grok-4",
            "mistralai/mistral-large",
            "meta-llama/llama-4-maverick",
            "deepseek/deepseek-v3",
        ] {
            let matched = lookup("openrouter", model).unwrap();
            assert_eq!(
                matched.facts.surfaces[&ModelSurface::Chat],
                Support::Supported
            );
            assert!(matched.endpoint_facts.is_none());
        }
        assert!(lookup("openrouter", "unknown-vendor/private-model").is_none());
        assert_eq!(
            provider_default("vllm").surfaces[&ModelSurface::Chat],
            Support::Unknown
        );
    }

    #[test]
    fn all_typesafe_models_decide_and_are_prompted_as_structured_output() {
        for model in ["jev-1.13.0", "jev-latest", "future-system-one"] {
            let profile = crate::capabilities::resolve("typesafe", model, None, None)
                .unwrap()
                .profile;
            assert_eq!(
                profile.surfaces[&ModelSurface::Decisions],
                Support::Supported
            );
            assert_eq!(profile.surfaces[&ModelSurface::Chat], Support::Supported);
            assert_eq!(
                profile.features[&crate::capabilities::ModelFeature::StructuredOutput],
                Support::Supported
            );
            assert_eq!(
                profile.features[&crate::capabilities::ModelFeature::StructuredOutputOnly],
                Support::Supported
            );
            assert_eq!(
                profile.features[&crate::capabilities::ModelFeature::ToolCalling],
                Support::Unsupported
            );
            assert_eq!(
                profile
                    .operation(
                        crate::capabilities::Protocol::OpenaiChatCompletions,
                        crate::capabilities::ProtocolOperation::Create,
                    )
                    .map(|operation| operation.mode),
                Some(crate::capabilities::DeliveryMode::Adapted)
            );
            assert_eq!(
                profile
                    .operation(
                        crate::capabilities::Protocol::OpenaiResponses,
                        crate::capabilities::ProtocolOperation::Create,
                    )
                    .map(|operation| operation.mode),
                Some(crate::capabilities::DeliveryMode::Unsupported)
            );
            assert_eq!(profile.limits.context_window, Some(64_000));
            assert_eq!(profile.limits.max_input_tokens, Some(64_000));
            assert_eq!(profile.limits.max_output_tokens, None);
            assert_eq!(
                crate::capabilities::ClientCapabilityProjection::playground(&profile).chat_support,
                Support::Unsupported,
                "structured-output-only models must not appear in the chat playground"
            );
            assert_eq!(
                profile
                    .operation(
                        crate::capabilities::Protocol::RouterDecisions,
                        crate::capabilities::ProtocolOperation::Create,
                    )
                    .map(|operation| operation.mode),
                Some(crate::capabilities::DeliveryMode::Adapted)
            );
        }

        let openrouter =
            crate::capabilities::resolve("openrouter", "typesafe/jev-1.13", None, None)
                .unwrap()
                .profile;
        assert_eq!(
            openrouter.surfaces[&ModelSurface::Chat],
            Support::Unsupported,
            "OpenRouter only relays Jev's native decisions surface"
        );
        assert_eq!(
            openrouter
                .operation(
                    crate::capabilities::Protocol::TypesafeSystemOne,
                    crate::capabilities::ProtocolOperation::Create,
                )
                .map(|operation| operation.mode),
            Some(crate::capabilities::DeliveryMode::Native)
        );
        assert_eq!(openrouter.limits.context_window, Some(262_144));
        assert_eq!(openrouter.limits.max_input_tokens, Some(262_144));
        assert_eq!(openrouter.limits.max_output_tokens, None);
        assert_eq!(
            openrouter.features[&ModelFeature::DecisionBoxes],
            Support::Unsupported,
            "Jev 1.13 predates box questions"
        );
        let openrouter_alias =
            crate::capabilities::resolve("openrouter", "typesafe/jev-latest", None, None)
                .unwrap()
                .profile;
        assert_eq!(
            openrouter_alias
                .operation(
                    crate::capabilities::Protocol::TypesafeSystemOne,
                    crate::capabilities::ProtocolOperation::Create,
                )
                .map(|operation| operation.mode),
            Some(crate::capabilities::DeliveryMode::Unsupported)
        );
    }

    #[test]
    fn decision_models_resolve_per_provider_with_images_boxes_and_windows() {
        use crate::capabilities::{DeliveryMode, Protocol, ProtocolOperation};
        let resolve = |provider: &str, model: &str| {
            crate::capabilities::resolve(provider, model, None, None)
                .unwrap()
                .profile
        };
        let decisions_mode = |profile: &crate::capabilities::CapabilityProfile| {
            profile
                .operation(Protocol::RouterDecisions, ProtocolOperation::Create)
                .map(|operation| operation.mode)
        };

        let luna = resolve("openai", "gpt-6-luna");
        assert_eq!(luna.surfaces[&ModelSurface::Chat], Support::Supported);
        assert_eq!(luna.surfaces[&ModelSurface::Decisions], Support::Supported);
        assert_eq!(
            luna.modalities.input[&InputModality::Image],
            Support::Supported
        );
        assert_eq!(decisions_mode(&luna), Some(DeliveryMode::Translated));
        assert_eq!(
            decisions_mode(&resolve("openai", "gpt-4o")),
            Some(DeliveryMode::Unsupported)
        );

        let decider = resolve("perplexity", "pplx-decider-v1.2");
        assert_eq!(decisions_mode(&decider), Some(DeliveryMode::Native));
        assert_eq!(decider.limits.context_window, Some(262_144));
        assert_eq!(
            decider.features[&ModelFeature::DecisionBoxes],
            Support::Unsupported
        );
        assert_eq!(
            decisions_mode(&resolve("perplexity", "sonar-pro")),
            Some(DeliveryMode::Unsupported)
        );

        for (model, boxes) in [
            ("jev-1.14.0", Support::Supported),
            ("jev-preview", Support::Supported),
            ("jev-1.13.0", Support::Unsupported),
            ("jev-latest", Support::Unsupported),
        ] {
            let profile = resolve("typesafe", model);
            assert_eq!(
                profile.features[&ModelFeature::DecisionBoxes],
                boxes,
                "{model}"
            );
            assert_eq!(
                profile.modalities.input[&InputModality::Image],
                Support::Supported
            );
        }

        let jev = resolve("openrouter", "typesafe/jev-1.14");
        assert_eq!(
            jev.features[&ModelFeature::DecisionBoxes],
            Support::Supported
        );
        for model in [
            "perplexity/pplx-decider-v1.2",
            "openai/gpt-6-luna-decisions",
        ] {
            let profile = resolve("openrouter", model);
            assert_eq!(
                decisions_mode(&profile),
                Some(DeliveryMode::Native),
                "{model}"
            );
            assert_eq!(profile.surfaces[&ModelSurface::Chat], Support::Unsupported);
            assert_eq!(
                profile.features[&ModelFeature::DecisionBoxes],
                Support::Unsupported
            );
            assert_eq!(
                profile
                    .operation(Protocol::TypesafeSystemOne, ProtocolOperation::Create)
                    .map(|operation| operation.mode),
                Some(DeliveryMode::Unsupported),
                "System One ingress names TypeSafe models only"
            );
        }
        // Moving aliases are deliberately not routed as decision models.
        assert_ne!(
            decisions_mode(&resolve("openrouter", "~typesafe/jev-latest")),
            Some(DeliveryMode::Native)
        );
    }

    #[test]
    fn every_builtin_provider_kind_has_a_rule_or_an_explicit_default() {
        for provider in crate::provider_registry::all() {
            let kind = provider.kind;
            let has_rule = RULES.iter().any(|rule| rule.provider_kind == kind);
            assert!(
                has_rule
                    || GENERIC_CHAT_DEFAULT_KINDS.contains(&kind)
                    || GENERIC_DECISIONS_DEFAULT_KINDS.contains(&kind)
                    || GENERIC_UNKNOWN_DEFAULT_KINDS.contains(&kind),
                "builtin provider kind '{kind}' has no RULES entry and is not in \
                 a named generic-default set -- decide \
                 its model_capability_registry::provider_default fallback"
            );
        }
    }

    fn matchers_overlap(left: Matcher, right: Matcher) -> bool {
        match (left, right) {
            (Matcher::Exact(left), Matcher::Exact(right)) => left == right,
            (Matcher::Exact(exact), Matcher::Prefix(prefix))
            | (Matcher::Prefix(prefix), Matcher::Exact(exact)) => exact.starts_with(prefix),
            (Matcher::Exact(exact), Matcher::Suffix(suffix))
            | (Matcher::Suffix(suffix), Matcher::Exact(exact)) => exact.ends_with(suffix),
            (Matcher::Prefix(left), Matcher::Prefix(right)) => {
                left.starts_with(right) || right.starts_with(left)
            }
            (Matcher::Suffix(left), Matcher::Suffix(right)) => {
                left.ends_with(right) || right.ends_with(left)
            }
            (Matcher::Prefix(_), Matcher::Suffix(_)) | (Matcher::Suffix(_), Matcher::Prefix(_)) => {
                true
            }
        }
    }
}
