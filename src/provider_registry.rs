//! Built-in provider registry.
//!
//! A single, static source of truth for the LLM providers the router knows how
//! to build a backend for. This consolidates what used to be spread across
//! `service::settings` (the provider id/display-name table), `backend::router`
//! (the dynamic-provider list, per-provider `build_provider_adapter`, and the
//! endpoint defaults), and the per-backend base-URL constants.
//!
//! Every entry here is a **built-in kind**: the router knows which adapter and
//! endpoint defaults to use, while operators may create multiple connections of
//! that kind with distinct names and prefixes. The pseudo-kind
//! [`OPENAI_COMPATIBLE_KIND`] is *not* in this registry — it denotes an
//! arbitrary user-configured OpenAI-compatible endpoint, so it is deliberately
//! excluded from [`all`], [`get`], and [`is_builtin`].

/// Which backend adapter serves a built-in kind. Determines the constructor
/// used to build a live backend (or a throwaway probe adapter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    /// Served through `backend::OpenAICompatibleBackend` (OpenAI's HTTP shape).
    OpenAiCompatible,
    /// Native Gemini Interactions with compatible transport for other operations.
    Gemini,
    /// Served through `backend::AnthropicBackend`.
    Anthropic,
    /// Served through `backend::VoyageBackend`.
    Voyage,
    /// Served through `backend::TypeSafeBackend`.
    TypeSafe,
    /// Served through `backend::VertexBackend` (GCP OAuth, project-scoped).
    Vertex,
    /// Served through Mantle or the Bedrock Runtime Converse adapter.
    Bedrock,
}

/// A statically-known provider the router can route to and build a backend for.
pub struct BuiltinProvider {
    /// Stable machine id, e.g. `"openai"`. Also the value stored in
    /// `provider_connections.provider_kind`.
    pub kind: &'static str,
    /// Human-facing label for the Providers UI.
    pub display_name: &'static str,
    /// The prefix this kind owns in the model namespace, e.g. `"openai/"`.
    pub canonical_prefix: &'static str,
    /// Default endpoint, or `None` where the adapter owns its default internally
    /// (`openai`/`anthropic`/`voyage`) or has no single code-owned default
    /// (`vllm` requires one; `bedrock`'s default is region-derived).
    pub default_base_url: Option<&'static str>,
    /// Which backend adapter serves this kind.
    pub adapter: AdapterKind,
    /// Conservative protocol/model defaults for a model on this provider.
    pub capabilities: fn(&str) -> crate::capabilities::CapabilityProfile,
}

/// The pseudo-kind for an arbitrary OpenAI-compatible endpoint. Distinct from a
/// built-in kind because every connection supplies its own `base_url`; never
/// appears in [`all`]/[`get`]/[`is_builtin`].
pub const OPENAI_COMPATIBLE_KIND: &str = "openai-compatible";

/// The prefix reserved for `chalk/*` model routes; never usable as a connection
/// prefix.
pub const ROUTE_RESERVED_PREFIX: &str = "chalk/";

/// Split a model id on its first `/` into a candidate route-prefix segment
/// (without the trailing slash) and the remaining upstream model id, e.g.
/// `strip_route_prefix("openai/gpt-4")` -> `Some(("openai", "gpt-4"))`.
/// Returns `None` when `model` has no `/` at all.
///
/// This only splits the string; it does not check whether the segment is a
/// *known* prefix (a connection's stored `prefix`, or [`ROUTE_RESERVED_PREFIX`])
/// — callers that need that do the lookup themselves.
pub fn strip_route_prefix(model: &str) -> Option<(&str, &str)> {
    model.split_once('/')
}

const BUILTINS: &[BuiltinProvider] = &[
    BuiltinProvider {
        kind: "openai",
        display_name: "OpenAI",
        canonical_prefix: "openai/",
        default_base_url: None,
        adapter: AdapterKind::OpenAiCompatible,
        capabilities: crate::capabilities::openai_capabilities,
    },
    BuiltinProvider {
        kind: "anthropic",
        display_name: "Anthropic",
        canonical_prefix: "anthropic/",
        default_base_url: None,
        adapter: AdapterKind::Anthropic,
        capabilities: crate::capabilities::anthropic_capabilities,
    },
    BuiltinProvider {
        kind: "gemini",
        display_name: "Google Gemini",
        canonical_prefix: "gemini/",
        default_base_url: Some(crate::backend::gemini::GEMINI_API_BASE_URL),
        adapter: AdapterKind::Gemini,
        capabilities: crate::capabilities::gemini_capabilities,
    },
    BuiltinProvider {
        kind: "vertex",
        display_name: "Google Vertex AI (Agent Platform)",
        canonical_prefix: "vertex/",
        default_base_url: Some(crate::backend::vertex::VERTEX_DEFAULT_BASE_URL),
        adapter: AdapterKind::Vertex,
        capabilities: crate::capabilities::vertex_capabilities,
    },
    BuiltinProvider {
        kind: "voyage",
        display_name: "Voyage AI",
        canonical_prefix: "voyage/",
        default_base_url: None,
        adapter: AdapterKind::Voyage,
        capabilities: crate::capabilities::voyage_capabilities,
    },
    BuiltinProvider {
        kind: "typesafe",
        display_name: "TypeSafe",
        canonical_prefix: "typesafe/",
        default_base_url: Some(crate::backend::typesafe::TYPESAFE_API_BASE_URL),
        adapter: AdapterKind::TypeSafe,
        capabilities: crate::capabilities::typesafe_capabilities,
    },
    BuiltinProvider {
        kind: "vllm",
        display_name: "OpenAI-compatible (vLLM)",
        canonical_prefix: "vllm/",
        default_base_url: None,
        adapter: AdapterKind::OpenAiCompatible,
        capabilities: crate::capabilities::unknown_generation_capabilities,
    },
    BuiltinProvider {
        kind: "bedrock",
        display_name: "AWS Bedrock",
        canonical_prefix: "bedrock/",
        default_base_url: None,
        adapter: AdapterKind::Bedrock,
        capabilities: crate::capabilities::bedrock_capabilities,
    },
    BuiltinProvider {
        kind: "openrouter",
        display_name: "OpenRouter",
        canonical_prefix: "openrouter/",
        default_base_url: Some(crate::backend::openrouter::OPENROUTER_API_BASE_URL),
        adapter: AdapterKind::OpenAiCompatible,
        capabilities: crate::capabilities::openrouter_capabilities,
    },
    BuiltinProvider {
        kind: "xai",
        display_name: "xAI (Grok)",
        canonical_prefix: "xai/",
        default_base_url: Some(crate::backend::xai::XAI_OPENAI_BASE_URL),
        adapter: AdapterKind::OpenAiCompatible,
        capabilities: crate::capabilities::xai_capabilities,
    },
    BuiltinProvider {
        kind: "cohere",
        display_name: "Cohere",
        canonical_prefix: "cohere/",
        default_base_url: Some(crate::backend::cohere::COHERE_OPENAI_BASE_URL),
        adapter: AdapterKind::OpenAiCompatible,
        capabilities: crate::capabilities::translated_generation_capabilities,
    },
    BuiltinProvider {
        kind: "qwen",
        display_name: "Qwen",
        canonical_prefix: "qwen/",
        default_base_url: Some(crate::backend::qwen::QWEN_OPENAI_BASE_URL),
        adapter: AdapterKind::OpenAiCompatible,
        capabilities: crate::capabilities::qwen_capabilities,
    },
    BuiltinProvider {
        kind: "fireworks",
        display_name: "Fireworks AI",
        canonical_prefix: "fireworks/",
        default_base_url: Some(crate::backend::fireworks::FIREWORKS_API_BASE_URL),
        adapter: AdapterKind::OpenAiCompatible,
        capabilities: crate::capabilities::fireworks_capabilities,
    },
    BuiltinProvider {
        kind: "together",
        display_name: "Together AI",
        canonical_prefix: "together/",
        default_base_url: Some(crate::backend::together::TOGETHER_API_BASE_URL),
        adapter: AdapterKind::OpenAiCompatible,
        capabilities: crate::capabilities::translated_generation_capabilities,
    },
    BuiltinProvider {
        kind: "groq",
        display_name: "Groq",
        canonical_prefix: "groq/",
        default_base_url: Some(crate::backend::groq::GROQ_API_BASE_URL),
        adapter: AdapterKind::OpenAiCompatible,
        capabilities: crate::capabilities::native_responses_capabilities,
    },
    BuiltinProvider {
        kind: "mistral",
        display_name: "Mistral AI",
        canonical_prefix: "mistral/",
        default_base_url: Some(crate::backend::mistral::MISTRAL_API_BASE_URL),
        adapter: AdapterKind::OpenAiCompatible,
        capabilities: crate::capabilities::translated_generation_capabilities,
    },
    BuiltinProvider {
        kind: "perplexity",
        display_name: "Perplexity",
        canonical_prefix: "perplexity/",
        default_base_url: Some(crate::backend::perplexity::PERPLEXITY_API_BASE_URL),
        adapter: AdapterKind::OpenAiCompatible,
        capabilities: crate::capabilities::perplexity_capabilities,
    },
];

/// Every built-in provider, in registry order.
pub fn all() -> &'static [BuiltinProvider] {
    BUILTINS
}

/// The built-in provider for `kind`, or `None` if `kind` is not a built-in
/// (including [`OPENAI_COMPATIBLE_KIND`], which is never built-in).
pub fn get(kind: &str) -> Option<&'static BuiltinProvider> {
    BUILTINS.iter().find(|p| p.kind == kind)
}

/// Whether `kind` names a built-in provider (false for [`OPENAI_COMPATIBLE_KIND`]).
pub fn is_builtin(kind: &str) -> bool {
    get(kind).is_some()
}

/// The canonical prefix a built-in `kind` owns, e.g. `"openai/"`.
pub fn canonical_prefix(kind: &str) -> Option<&'static str> {
    get(kind).map(|p| p.canonical_prefix)
}

/// Conservative built-in capability defaults for `model`.
pub fn capabilities(kind: &str, model: &str) -> crate::capabilities::CapabilityProfile {
    use crate::capabilities::{DeliveryMode, Interface, InterfaceOperation, OperationProfile};
    let provider = get(kind);
    let mut profile = provider
        .map(|provider| (provider.capabilities)(model))
        .unwrap_or_else(|| crate::capabilities::openai_compatible_capabilities(model));
    // Compatible transports may expose moderation; native adapters without
    // a moderation operation reject it before an upstream request.
    let mode = match provider.map(|provider| provider.adapter) {
        Some(AdapterKind::OpenAiCompatible | AdapterKind::Gemini | AdapterKind::Bedrock) | None => {
            if kind == "openai" {
                DeliveryMode::Native
            } else {
                DeliveryMode::Unknown
            }
        }
        Some(_) => DeliveryMode::Unsupported,
    };
    profile
        .interfaces
        .entry(Interface::OpenaiModerations)
        .or_default()
        .operations
        .insert(InterfaceOperation::Create, OperationProfile::new(mode));
    profile
}

/// Whether a connection of `kind` must carry an explicit `base_url` to be
/// dispatchable. Every other kind has an effective default endpoint — a built-in
/// registry default, an adapter-owned default (`openai`/`anthropic`/`voyage`),
/// or a region-derived one (`bedrock`) — so it can build a backend without one.
/// Only `vllm` and the [`OPENAI_COMPATIBLE_KIND`] pseudo-kind have no default at
/// all. Mirrors the base-URL requirement in `build_provider_adapter`, shared by
/// routing (stored connections) and `build_probe_adapter`; a readable
/// connection of such a kind with no base URL resolves but cannot dispatch
/// (surfaced as an `unconfigured` route diagnostic).
pub fn requires_explicit_base_url(kind: &str) -> bool {
    kind == "vllm" || kind == OPENAI_COMPATIBLE_KIND
}

/// Resolve a built-in provider's effective base URL, honoring an optional
/// per-connection override and falling back to the registry's
/// `default_base_url`.
///
/// Blank overrides (empty or all-whitespace) are treated as absent. Trailing
/// slashes are trimmed because backend adapters append endpoint paths with a
/// leading slash. Returns `None` when `kind` is unknown, or when it has
/// neither an override nor a registry default (e.g. `vllm`, which has no
/// universal hosted endpoint).
pub(crate) fn resolve_base_url(kind: &str, base_url_override: Option<&str>) -> Option<String> {
    let default = get(kind).and_then(|p| p.default_base_url);
    base_url_override
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .or(default)
        .map(|url| url.trim_end_matches('/').to_owned())
}

/// Whether `prefix` is the canonical prefix of some built-in kind *other than*
/// `kind`. Used to stop, e.g., an `openai-compatible` connection from claiming
/// `"anthropic/"`, while still allowing the built-in `anthropic` connection to
/// use its own canonical prefix.
pub fn prefix_reserved_by_other_kind(prefix: &str, kind: &str) -> bool {
    BUILTINS
        .iter()
        .any(|p| p.canonical_prefix == prefix && p.kind != kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn canonical_prefixes_are_unique() {
        let mut seen = HashSet::new();
        for p in all() {
            assert!(
                seen.insert(p.canonical_prefix),
                "duplicate canonical prefix: {}",
                p.canonical_prefix
            );
        }
        assert_eq!(seen.len(), all().len());
    }

    #[test]
    fn canonical_prefix_matches_kind() {
        for p in all() {
            assert_eq!(p.canonical_prefix, format!("{}/", p.kind));
            assert_eq!(canonical_prefix(p.kind), Some(p.canonical_prefix));
        }
    }

    #[test]
    fn get_and_is_builtin_agree() {
        for p in all() {
            assert!(is_builtin(p.kind));
            assert_eq!(get(p.kind).map(|b| b.kind), Some(p.kind));
        }
        // The openai-compatible pseudo-kind is never a built-in.
        assert!(!is_builtin(OPENAI_COMPATIBLE_KIND));
        assert!(get(OPENAI_COMPATIBLE_KIND).is_none());
        assert!(!is_builtin("grok"));
        assert!(get("grok").is_none());
        assert!(!is_builtin("does-not-exist"));
    }

    #[test]
    fn prefix_reservation_is_cross_kind() {
        // "openai/" is reserved against every other kind, but not against openai.
        assert!(prefix_reserved_by_other_kind("openai/", "anthropic"));
        assert!(prefix_reserved_by_other_kind(
            "openai/",
            OPENAI_COMPATIBLE_KIND
        ));
        assert!(!prefix_reserved_by_other_kind("openai/", "openai"));
        // A bespoke prefix is reserved by nobody.
        assert!(!prefix_reserved_by_other_kind(
            "my-endpoint/",
            OPENAI_COMPATIBLE_KIND
        ));
    }

    #[test]
    fn every_builtin_has_a_responses_create_default() {
        use crate::capabilities::{Protocol, ProtocolOperation};

        for provider in all() {
            let profile = (provider.capabilities)("example-model");
            assert!(
                profile
                    .operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
                    .is_some(),
                "{} has no Responses create default",
                provider.kind
            );
        }
    }

    #[test]
    fn qwen_and_fireworks_keep_provider_scoped_delivery_defaults() {
        use crate::capabilities::{DeliveryMode, Protocol, ProtocolOperation};

        let operation_mode = |provider, operation| {
            capabilities(provider, "example-model")
                .operation(Protocol::AnthropicMessages, operation)
                .unwrap()
                .mode
        };
        assert_eq!(
            operation_mode("qwen", ProtocolOperation::Create),
            DeliveryMode::Native
        );
        assert_eq!(
            operation_mode("qwen", ProtocolOperation::CountTokens),
            DeliveryMode::Unknown
        );
        assert_eq!(
            operation_mode("fireworks", ProtocolOperation::Create),
            DeliveryMode::Translated
        );
        assert_eq!(
            operation_mode("fireworks", ProtocolOperation::CountTokens),
            DeliveryMode::Unsupported
        );
    }

    #[test]
    fn resolves_base_url_for_openai_compatible_wrappers() {
        // (kind, registry default, an override to normalize)
        let cases: &[(&str, &str, &str)] = &[
            (
                "openrouter",
                crate::backend::openrouter::OPENROUTER_API_BASE_URL,
                "https://openrouter.example.test/proxy/v1",
            ),
            (
                "xai",
                crate::backend::xai::XAI_OPENAI_BASE_URL,
                "https://xai.example.test/openai/v1",
            ),
            (
                "cohere",
                crate::backend::cohere::COHERE_OPENAI_BASE_URL,
                "https://cohere.example.test/openai/v1",
            ),
            (
                "qwen",
                crate::backend::qwen::QWEN_OPENAI_BASE_URL,
                "https://workspace.example.aliyuncs.com/compatible-mode/v1",
            ),
            (
                "fireworks",
                crate::backend::fireworks::FIREWORKS_API_BASE_URL,
                "https://fireworks.example.test/v1",
            ),
            (
                "together",
                crate::backend::together::TOGETHER_API_BASE_URL,
                "https://together.example.test/v1",
            ),
            (
                "groq",
                crate::backend::groq::GROQ_API_BASE_URL,
                "https://groq.example.test/openai/v1",
            ),
            (
                "mistral",
                crate::backend::mistral::MISTRAL_API_BASE_URL,
                "https://mistral.example.test/v1",
            ),
            (
                "perplexity",
                crate::backend::perplexity::PERPLEXITY_API_BASE_URL,
                "https://perplexity.example.test",
            ),
        ];

        for (kind, default, custom) in cases {
            assert_eq!(
                resolve_base_url(kind, None).as_deref(),
                Some(*default),
                "{kind}: no override falls back to the registry default"
            );
            assert_eq!(
                resolve_base_url(kind, Some("  ")).as_deref(),
                Some(*default),
                "{kind}: a blank override falls back to the registry default"
            );
            assert_eq!(
                resolve_base_url(kind, Some(&format!(" {custom}/// "))).as_deref(),
                Some(*custom),
                "{kind}: an override is trimmed and stripped of trailing slashes"
            );
        }
    }

    #[test]
    fn resolve_base_url_has_no_default_for_kinds_without_a_registry_url() {
        // vllm has no universal hosted endpoint, so it must be configured
        // explicitly; an unknown kind naturally has no default either.
        assert_eq!(resolve_base_url("vllm", None), None);
        assert_eq!(resolve_base_url("vllm", Some("  ")), None);
        assert_eq!(resolve_base_url("does-not-exist", None), None);
    }

    #[test]
    fn strip_route_prefix_splits_on_first_slash() {
        assert_eq!(
            strip_route_prefix("openai/gpt-4"),
            Some(("openai", "gpt-4"))
        );
        assert_eq!(
            strip_route_prefix("chalk/my-route"),
            Some(("chalk", "my-route"))
        );
        // Only the first slash matters; the rest stays in the remainder.
        assert_eq!(
            strip_route_prefix("vertex/publishers/google/models/gemini"),
            Some(("vertex", "publishers/google/models/gemini"))
        );
    }

    #[test]
    fn strip_route_prefix_returns_none_without_a_slash() {
        assert_eq!(strip_route_prefix("gpt-4"), None);
        assert_eq!(strip_route_prefix(""), None);
    }
}
