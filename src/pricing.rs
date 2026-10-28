//! Token prices, and the cost they imply for one completed request.
//!
//! This exists because the read side of cost was fully built and had no
//! producer. `chalk.telemetry.spans` exposes `llm_cost` as
//! `coalesce(SpanAttributes['llm.cost.total'], ['llm.cost'],
//! ['gen_ai.usage.cost'])`, the evaluation score summary casts it to a double,
//! and chalkcompute scorers sum `llm.cost` — while nothing anywhere wrote any of
//! the three. Every evaluation therefore reported a cost of zero. The router is
//! the only component that sees both the resolved model and the token counts, so
//! it is the only place that can fill that in.
//!
//! # Rate cards are a history, not a table
//!
//! `prices/rate_cards/` stores one sparse-history shard per provider, refreshed
//! by `scripts/fetch_rate_cards.py`. A model gets a new revision only when one
//! of its prices changes, and a `null` revision when it disappears.
//! [`current`] and [`card_effective_at`] select the catalog version; price lookup
//! then binary-searches only that model's revisions. This keeps historical
//! pricing without repeating every unchanged model in every version.
//!
//! # An unknown model yields no cost, never zero
//!
//! [`cost_usd`] returns `None` for a model the current card does not price, and
//! the caller omits the attribute entirely. Emitting `0.0` would be
//! indistinguishable from a genuinely free request and would reintroduce exactly
//! the bug this fixes, except harder to see: a NULL reads as "not priced", a
//! zero reads as "free".
//!
//! # These are estimated list prices
//!
//! The result is suitable for evaluation summaries and trace-level comparisons,
//! not invoice reconciliation. It uses the public, checked-in LiteLLM rate card
//! and does not account for negotiated rates, provider discounts, batch pricing,
//! long-context tiers, or any region other than Bedrock's default. A Bedrock
//! model id with no region is priced from the `us-east-1` card, the region a
//! connection uses when it has no base URL. A model sold only in some other
//! region stays unpriced. Callers stamp the estimate source and rate-card
//! version beside the resulting span attributes so consumers can keep that
//! distinction visible.
//!
//! # Token conventions
//!
//! Providers disagree about whether cached tokens are additive to the prompt
//! count. Anthropic's `input_tokens` *excludes* both cache fields; OpenAI's
//! `cached_tokens` is a subset of `input_tokens`. Both handlers normalize before
//! reaching `LlmSpanWriter::set_token_usage`, which is where cost is computed:
//! `prompt` is the *inclusive* total and the cache figures are subsets of it. So
//! the uncached portion is `prompt - cache_read - cache_write`, computed
//! saturating so a provider that ever violates that invariant under-reports
//! rather than underflowing.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

/// The checked-in history. Embedded rather than read at runtime so a deployed
/// router cannot disagree with the binary it was built from.
const RATE_CARD_MANIFEST_JSON: &str = include_str!("../prices/rate_cards/manifest.json");
const RATE_CARD_PROVIDER_SHARDS_JSON: &[&str] = &[
    include_str!("../prices/rate_cards/anthropic.json"),
    include_str!("../prices/rate_cards/bedrock.json"),
    include_str!("../prices/rate_cards/cohere_chat.json"),
    include_str!("../prices/rate_cards/fireworks_ai.json"),
    include_str!("../prices/rate_cards/gemini.json"),
    include_str!("../prices/rate_cards/groq.json"),
    include_str!("../prices/rate_cards/mistral.json"),
    include_str!("../prices/rate_cards/openai.json"),
    include_str!("../prices/rate_cards/openrouter.json"),
    include_str!("../prices/rate_cards/together_ai.json"),
    include_str!("../prices/rate_cards/typesafe.json"),
    include_str!("../prices/rate_cards/vertex_ai-language-models.json"),
    include_str!("../prices/rate_cards/xai.json"),
];

/// Dollars per million tokens, by billing category.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
pub struct ModelPrices {
    #[serde(default)]
    pub input_per_mtok: Option<f64>,
    #[serde(default)]
    pub output_per_mtok: Option<f64>,
    /// Reading a provider-side prompt cache. `None` means the catalog does not
    /// know the rate, not that the normal input rate applies.
    #[serde(default)]
    pub cache_read_per_mtok: Option<f64>,
    /// Writing a provider-side prompt cache. `None` means the catalog does not
    /// know the rate, not that the normal input rate applies.
    #[serde(default)]
    pub cache_write_per_mtok: Option<f64>,
    /// Input images billed per image, in USD.
    #[serde(default)]
    pub input_per_image_usd: Option<f64>,
    /// Generated images billed per image, in USD.
    #[serde(default)]
    pub output_per_image_usd: Option<f64>,
    /// Input pixels billed per million pixels.
    #[serde(default)]
    pub input_per_mpixel: Option<f64>,
    /// Output pixels billed per million pixels.
    #[serde(default)]
    pub output_per_mpixel: Option<f64>,
    /// Provider-defined input image tokens billed per million tokens.
    #[serde(default)]
    pub input_image_per_mtok: Option<f64>,
    /// Provider-defined output image tokens billed per million tokens.
    #[serde(default)]
    pub output_image_per_mtok: Option<f64>,
}

/// One date at which at least one model's prices changed.
#[derive(Debug, Deserialize)]
pub struct RateCard {
    /// When these prices took effect, and the key [`card_effective_at`] orders
    /// on.
    pub effective_from: String,
    /// The fetch that introduced this version. Identical later fetches are
    /// intentionally no-ops and do not rewrite history just to advance this.
    pub last_verified_at: String,
}

#[derive(Debug, Deserialize)]
struct RateCardManifest {
    schema_version: u32,
    versions: Vec<RateCard>,
    providers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ProviderRateCardHistory {
    provider: String,
    /// Keyed by the provider-local model id. Revisions are ordered by version index;
    /// `None` is a tombstone recording that the model left the catalog.
    models: HashMap<String, Vec<ModelPriceRevision>>,
}

#[derive(Debug)]
struct RateCardHistory {
    versions: Vec<RateCard>,
    providers: HashMap<String, HashMap<String, Vec<ModelPriceRevision>>>,
}

#[derive(Debug, Deserialize)]
struct ModelPriceRevision(usize, Option<ModelPrices>);

static HISTORY: LazyLock<RateCardHistory> = LazyLock::new(|| {
    let manifest: RateCardManifest = serde_json::from_str(RATE_CARD_MANIFEST_JSON)
        .expect("prices/rate_cards/manifest.json is checked in and must parse");
    assert_eq!(manifest.schema_version, 3, "unsupported rate-card schema");
    assert!(
        !manifest.versions.is_empty(),
        "prices/rate_cards/manifest.json must hold at least one version"
    );
    assert!(
        manifest
            .versions
            .windows(2)
            .all(|pair| pair[0].effective_from < pair[1].effective_from),
        "rate-card versions must be strictly ordered"
    );

    let mut providers = HashMap::new();
    for json in RATE_CARD_PROVIDER_SHARDS_JSON {
        let shard: ProviderRateCardHistory =
            serde_json::from_str(json).expect("checked-in provider rate-card shard must parse");
        for (model, revisions) in &shard.models {
            assert!(
                !revisions.is_empty(),
                "{}/{model} has no price revisions",
                shard.provider
            );
            assert!(
                revisions.windows(2).all(|pair| pair[0].0 < pair[1].0),
                "{}/{model} price revisions must be strictly ordered",
                shard.provider
            );
            assert!(
                revisions
                    .iter()
                    .all(|revision| revision.0 < manifest.versions.len()),
                "{}/{model} has a revision outside the version history",
                shard.provider
            );
        }
        let provider = shard.provider;
        assert!(
            providers.insert(provider.clone(), shard.models).is_none(),
            "duplicate rate-card shard for {provider}"
        );
    }

    let mut loaded_providers: Vec<&str> = providers.keys().map(String::as_str).collect();
    loaded_providers.sort_unstable();
    assert_eq!(
        manifest
            .providers
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        loaded_providers,
        "rate-card manifest must list every embedded provider shard in sorted order"
    );

    RateCardHistory {
        versions: manifest.versions,
        providers,
    }
});

/// The rate card in force now: the newest in the history.
pub fn current() -> &'static RateCard {
    HISTORY.versions.last().expect("history is non-empty")
}

/// The card in force at `timestamp`, for re-pricing a span after rates moved.
///
/// `timestamp` and `effective_from` are both RFC 3339 in UTC as written by the
/// fetch script, which makes them lexicographically ordered, so this compares
/// them as strings rather than taking a date dependency for an ordering the
/// format already guarantees.
///
/// Returns `None` for a timestamp preceding the first card — the honest answer,
/// since we do not know what was charged before we started recording.
pub fn card_effective_at(timestamp: &str) -> Option<&'static RateCard> {
    HISTORY
        .versions
        .iter()
        .rev()
        .find(|card| card.effective_from.as_str() <= timestamp)
}

/// The token counts of one completed request, in the normalized convention
/// described in the module docs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt: u64,
    pub completion: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub input_token_attribution: InputTokenAttribution,
}

/// Billing attribution of the inclusive prompt total. This does not change
/// token budgets: modality counts partition `prompt`, never add to it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputTokenAttribution {
    /// Preserve existing pricing for providers without modality accounting.
    #[default]
    TextRate,
    TextAndImage {
        text_tokens: u64,
        image_tokens: u64,
    },
    /// Usage is known, but its cost cannot be computed without inventing a split.
    Unknown,
}

/// The cost of one request, split so a consumer can attribute it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RequestCost {
    pub prompt_usd: f64,
    pub completion_usd: f64,
    pub total_usd: f64,
}

/// Region applied when a Bedrock model id does not already name one.
///
/// This is the same default as `backend::bedrock::DEFAULT_BEDROCK_REGION`.
/// Pricing does not import that module: the one-off backfill binary includes
/// this file directly.
const BEDROCK_DEFAULT_REGION: &str = "us-east-1";

/// Translate Chalk's provider kind to the identifier used by LiteLLM's rate
/// card. The router keeps its own stable public provider names; this translation
/// belongs at the external catalog boundary.
fn rate_card_provider(system: &str) -> &str {
    match system {
        "fireworks" => "fireworks_ai",
        "together" => "together_ai",
        "cohere" => "cohere_chat",
        "vertex" => "vertex_ai-language-models",
        _ => system,
    }
}

/// Remove only a known transport/provider prefix. Path-like upstream ids such
/// as `anthropic/claude-sonnet-4.5` on OpenRouter and
/// `accounts/fireworks/models/...` on Fireworks are otherwise preserved in
/// full; taking the final slash-delimited segment would identify a different
/// product, or no product at all.
fn upstream_model<'a>(system: &str, provider: &str, model: &'a str) -> &'a str {
    let model = [system, provider]
        .into_iter()
        .find_map(|prefix| {
            model
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix('/'))
        })
        .unwrap_or(model);

    // Gemini's native model resource names are `models/<id>`, while LiteLLM's
    // Gemini catalog keys use the bare `<id>`. Normalize that provider-specific
    // resource prefix for pricing lookup independently of the wire model ID.
    if provider == "gemini" {
        return model.strip_prefix("models/").unwrap_or(model);
    }

    // OpenRouter can report the concrete Jev build that served a request even
    // though its public rate card prices the logical release. Keep the dated
    // id in traces and usage dimensions; canonicalize only this pricing lookup.
    if provider == "openrouter" && model.starts_with("typesafe/jev-") {
        if let Some((release, build)) = model.rsplit_once('-') {
            if crate::decisions::is_openrouter_jev_version(release)
                && build.len() == 8
                && build.bytes().all(|byte| byte.is_ascii_digit())
            {
                return release;
            }
        }
    }

    model
}

impl RateCard {
    /// Prices for `model` as served by `system`.
    ///
    /// Provider and complete upstream model id form the identity: Bedrock,
    /// Anthropic, and OpenRouter can all serve Claude at different rates. No
    /// last-path-segment or cross-provider fallback is attempted because an
    /// unpriced request is recoverable and a confidently wrong price is not.
    ///
    /// The one regional exception is Bedrock. Usage stores the model id without
    /// a region, and LiteLLM often prices that id only as `us-east-1/<id>`.
    /// When the bare id has no current price, lookup uses that default region
    /// and no other.
    pub fn prices_for(&self, system: &str, model: &str) -> Option<ModelPrices> {
        let provider = rate_card_provider(system);
        let id = upstream_model(system, provider, model);
        let models = HISTORY.providers.get(provider)?;
        let version_index = HISTORY
            .versions
            .partition_point(|version| version.effective_from <= self.effective_from)
            .checked_sub(1)?;
        if let Some(prices) = models
            .get(id)
            .and_then(|revisions| prices_at(revisions, version_index))
        {
            return Some(prices);
        }
        if provider == "bedrock" && !id.contains('/') {
            let regional_id = format!("{BEDROCK_DEFAULT_REGION}/{id}");
            if let Some(prices) = models
                .get(&regional_id)
                .and_then(|revisions| prices_at(revisions, version_index))
            {
                return Some(prices);
            }
        }
        None
    }
}

fn prices_at(revisions: &[ModelPriceRevision], version_index: usize) -> Option<ModelPrices> {
    let index = revisions.partition_point(|revision| revision.0 <= version_index);
    index.checked_sub(1).and_then(|index| revisions[index].1)
}

/// The cost of `usage` under `model`, at the prices in force now.
pub fn cost_usd(system: &str, model: &str, usage: &TokenUsage) -> Option<RequestCost> {
    cost_usd_with(current(), system, model, usage)
}

/// The cost of `usage` under `model`, at a specific card's prices.
pub fn cost_usd_with(
    card: &RateCard,
    system: &str,
    model: &str,
    usage: &TokenUsage,
) -> Option<RequestCost> {
    let prices = card.prices_for(system, model)?;
    cost_with_prices(prices, usage)
}

fn cost_with_prices(prices: ModelPrices, usage: &TokenUsage) -> Option<RequestCost> {
    // A per-image/pixel catalog entry is insufficient for legacy token usage.
    // Explicit modality attribution can also use a known image-token rate.
    if prices.input_per_mtok.is_none()
        && prices.output_per_mtok.is_none()
        && !(prices.input_image_per_mtok.is_some()
            && matches!(
                usage.input_token_attribution,
                InputTokenAttribution::TextAndImage { .. }
            ))
    {
        return None;
    }

    // Saturating: the cache figures are subsets of `prompt`, and a provider that
    // ever breaks that should under-report rather than wrap to a huge number.
    let uncached_prompt = usage
        .prompt
        .saturating_sub(usage.cache_read)
        .saturating_sub(usage.cache_write);

    let per_mtok = |tokens: u64, rate: f64| (tokens as f64) * rate / 1_000_000.0;

    let cache_read_usd = if usage.cache_read == 0 {
        0.0
    } else {
        per_mtok(usage.cache_read, prices.cache_read_per_mtok?)
    };
    let cache_write_usd = if usage.cache_write == 0 {
        0.0
    } else {
        per_mtok(usage.cache_write, prices.cache_write_per_mtok?)
    };
    let prompt_usd = match usage.input_token_attribution {
        InputTokenAttribution::TextRate => {
            if uncached_prompt == 0 {
                cache_read_usd + cache_write_usd
            } else {
                per_mtok(uncached_prompt, prices.input_per_mtok?) + cache_read_usd + cache_write_usd
            }
        }
        InputTokenAttribution::TextAndImage {
            text_tokens,
            image_tokens,
        } => {
            // Gemini embeddings have no prompt cache. Without attribution of
            // cached modality tokens, subtracting them from either rate is unsafe.
            if usage.cache_read != 0
                || usage.cache_write != 0
                || text_tokens.checked_add(image_tokens)? != usage.prompt
            {
                return None;
            }
            let text_usd = if text_tokens == 0 {
                0.0
            } else {
                per_mtok(text_tokens, prices.input_per_mtok?)
            };
            let image_usd = if image_tokens == 0 {
                0.0
            } else {
                per_mtok(image_tokens, prices.input_image_per_mtok?)
            };
            text_usd + image_usd
        }
        InputTokenAttribution::Unknown => return None,
    };
    let completion_usd = if usage.completion == 0 {
        0.0
    } else {
        per_mtok(usage.completion, prices.output_per_mtok?)
    };

    Some(RequestCost {
        prompt_usd,
        completion_usd,
        total_usd: prompt_usd + completion_usd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-12, "{a} != {b}");
    }

    #[test]
    fn the_checked_in_history_parses_and_has_a_current_card() {
        let card = current();
        assert!(!HISTORY.providers.is_empty());
        assert!(card.last_verified_at >= card.effective_from);
    }

    #[test]
    fn cards_are_ordered_so_the_last_is_the_newest() {
        let effective: Vec<&str> = HISTORY
            .versions
            .iter()
            .map(|card| card.effective_from.as_str())
            .collect();
        let mut sorted = effective.clone();
        sorted.sort_unstable();
        assert_eq!(effective, sorted, "cards must be appended in time order");
    }

    #[test]
    fn unknown_model_is_unpriced_rather_than_free() {
        assert!(cost_usd("openai", "no-such-model", &TokenUsage::default()).is_none());
    }

    #[test]
    fn openai_moderation_is_free_rather_than_unpriced() {
        let cost = cost_usd(
            "openai",
            "omni-moderation-latest",
            &TokenUsage {
                prompt: 1_000,
                completion: 1,
                ..TokenUsage::default()
            },
        )
        .expect("moderation has a published zero price");
        approx(cost.total_usd, 0.0);
    }

    #[test]
    fn codex_auto_review_stays_unpriced() {
        assert!(cost_usd("openai", "codex-auto-review", &TokenUsage::default()).is_none());
    }

    #[test]
    fn provider_disambiguates_a_model_two_providers_both_sell() {
        // Bedrock resells Anthropic's models at its own rates. Asking without a
        // provider must not silently pick one of them.
        let card = current();
        let anthropic = card.prices_for("anthropic", "claude-sonnet-4-5");
        let bedrock_keys: Vec<&String> = HISTORY
            .providers
            .get("bedrock")
            .expect("bedrock shard")
            .iter()
            .filter(|(model, revisions)| {
                model.contains("claude-sonnet-4")
                    && prices_at(revisions, HISTORY.versions.len() - 1).is_some()
            })
            .map(|(key, _)| key)
            .collect();
        assert!(anthropic.is_some(), "anthropic sonnet must be priced");
        assert!(!bedrock_keys.is_empty(), "bedrock must also sell sonnet");
    }

    #[test]
    fn a_routing_prefix_is_stripped_before_lookup() {
        let card = current();
        assert_eq!(
            card.prices_for("anthropic", "anthropic/claude-sonnet-4-5"),
            card.prices_for("anthropic", "claude-sonnet-4-5"),
        );
    }

    #[test]
    fn gemini_resource_prefix_is_stripped_before_lookup() {
        let card = current();
        assert_eq!(
            card.prices_for("gemini", "models/gemini-3.8-flash"),
            card.prices_for("gemini", "gemini-3.8-flash"),
        );
    }

    #[test]
    fn bedrock_without_a_region_uses_only_the_default_region() {
        let card = current();
        let version = HISTORY.versions.len() - 1;
        let models = HISTORY.providers.get("bedrock").expect("bedrock shard");
        let bare = "deepseek.v3.2";
        assert!(
            models
                .get(bare)
                .and_then(|revisions| prices_at(revisions, version))
                .is_none(),
            "the bare id must be unpriced for this test to exercise the fallback"
        );
        let priced = card
            .prices_for("bedrock", bare)
            .expect("us-east-1 deepseek.v3.2");
        assert_eq!(
            priced,
            card.prices_for("bedrock", "us-east-1/deepseek.v3.2")
                .expect("regional card")
        );
        assert_ne!(
            priced,
            card.prices_for("bedrock", "ap-northeast-1/deepseek.v3.2")
                .expect("another region's card")
        );

        let gov_only = models.iter().find(|(id, revisions)| {
            let Some(rest) = id.strip_prefix("us-gov-east-1/") else {
                return false;
            };
            prices_at(revisions, version).is_some()
                && models
                    .get(rest)
                    .and_then(|revisions| prices_at(revisions, version))
                    .is_none()
                && models
                    .get(&format!("{BEDROCK_DEFAULT_REGION}/{rest}"))
                    .and_then(|revisions| prices_at(revisions, version))
                    .is_none()
        });
        let (gov_key, _) = gov_only.expect("a model priced only outside us-east-1");
        let bare_gov = gov_key.strip_prefix("us-gov-east-1/").expect("prefix");
        assert!(
            card.prices_for("bedrock", bare_gov).is_none(),
            "{bare_gov} must stay unpriced when us-east-1 has no card"
        );
        assert!(card.prices_for("bedrock", gov_key).is_some());
    }

    #[test]
    fn routed_provider_names_and_path_like_model_ids_match_the_catalog() {
        let card = current();
        for (provider, model) in [
            ("fireworks", "accounts/fireworks/models/deepseek-v3"),
            ("bedrock", "anthropic.claude-3-5-sonnet-20241022-v2:0"),
            ("together", "meta-llama/Llama-3.3-70B-Instruct-Turbo"),
            ("vertex", "gemini-3.1-pro-preview"),
        ] {
            assert!(
                card.prices_for(provider, model).is_some(),
                "missing routed price for {provider}/{model}"
            );
        }
    }

    #[test]
    fn path_like_models_keep_their_provider_namespace() {
        assert_eq!(
            upstream_model("openrouter", "openrouter", "anthropic/claude-sonnet-4.5"),
            "anthropic/claude-sonnet-4.5"
        );
        assert_eq!(
            upstream_model(
                "openrouter",
                "openrouter",
                "openrouter/anthropic/claude-sonnet-4.5"
            ),
            "anthropic/claude-sonnet-4.5"
        );
    }

    #[test]
    fn cache_tokens_are_billed_out_of_the_prompt_not_on_top_of_it() {
        // 1M prompt of which 400k was a cache read and 100k a cache write, so
        // 500k is fresh input. Anything treating the cache figures as additive
        // would bill 1.5M tokens here.
        let usage = TokenUsage {
            prompt: 1_000_000,
            completion: 0,
            cache_read: 400_000,
            cache_write: 100_000,
            ..Default::default()
        };
        let cost = cost_usd("anthropic", "claude-sonnet-4-5", &usage).expect("priced");
        // 0.5 * 3.00 + 0.4 * 0.30 + 0.1 * 3.75
        approx(cost.prompt_usd, 1.5 + 0.12 + 0.375);
        approx(cost.total_usd, cost.prompt_usd);
    }

    #[test]
    fn completion_is_priced_at_the_output_rate() {
        let usage = TokenUsage {
            completion: 1_000_000,
            ..TokenUsage::default()
        };
        let cost = cost_usd("anthropic", "claude-sonnet-4-5", &usage).expect("priced");
        approx(cost.completion_usd, 15.0);
        approx(cost.prompt_usd, 0.0);
    }

    #[test]
    fn cache_figures_exceeding_the_prompt_saturate_rather_than_wrapping() {
        let usage = TokenUsage {
            prompt: 10,
            completion: 0,
            cache_read: 100,
            cache_write: 100,
            ..Default::default()
        };
        let cost = cost_usd("anthropic", "claude-sonnet-4-5", &usage).expect("priced");
        assert!(cost.total_usd.is_finite() && cost.total_usd >= 0.0);
    }

    #[test]
    fn missing_cache_rate_makes_cached_usage_unpriced() {
        let prices = ModelPrices {
            input_per_mtok: Some(1.0),
            output_per_mtok: Some(2.0),
            ..ModelPrices::default()
        };

        assert!(cost_with_prices(
            prices,
            &TokenUsage {
                prompt: 10,
                cache_read: 5,
                ..TokenUsage::default()
            }
        )
        .is_none());
        assert!(cost_with_prices(
            prices,
            &TokenUsage {
                prompt: 10,
                ..TokenUsage::default()
            }
        )
        .is_some());
    }

    #[test]
    fn input_only_embedding_rate_prices_zero_completion_usage() {
        let prices = ModelPrices {
            input_per_mtok: Some(0.02),
            ..ModelPrices::default()
        };

        let cost = cost_with_prices(
            prices,
            &TokenUsage {
                prompt: 1_000_000,
                ..TokenUsage::default()
            },
        )
        .expect("input-only embedding usage is priceable");
        approx(cost.total_usd, 0.02);
    }

    #[test]
    fn gemini_embedding_modality_tokens_use_their_own_rates() {
        for model in ["gemini-embedding-2", "gemini-embedding-2-preview"] {
            for (text_tokens, image_tokens, expected) in [
                (7, 0, 0.0000014),
                (0, 258, 0.0001161),
                (7, 258, 0.0001175),
                (0, 0, 0.0),
            ] {
                let usage = TokenUsage {
                    prompt: text_tokens + image_tokens,
                    input_token_attribution: InputTokenAttribution::TextAndImage {
                        text_tokens,
                        image_tokens,
                    },
                    ..Default::default()
                };
                let cost = cost_usd("gemini", model, &usage).expect("modality rates known");
                approx(cost.prompt_usd, expected);
                approx(cost.completion_usd, 0.0);
                approx(cost.total_usd, expected);
            }
        }
    }

    #[test]
    fn unknown_or_inconsistent_modality_attribution_is_unpriced() {
        for input_token_attribution in [
            InputTokenAttribution::Unknown,
            InputTokenAttribution::TextAndImage {
                text_tokens: 7,
                image_tokens: 257,
            },
            InputTokenAttribution::TextAndImage {
                text_tokens: u64::MAX,
                image_tokens: 1,
            },
        ] {
            assert!(cost_usd(
                "gemini",
                "gemini-embedding-2",
                &TokenUsage {
                    prompt: 265,
                    input_token_attribution,
                    ..Default::default()
                }
            )
            .is_none());
        }
        let usage = TokenUsage {
            prompt: 258,
            input_token_attribution: InputTokenAttribution::TextAndImage {
                text_tokens: 0,
                image_tokens: 258,
            },
            ..Default::default()
        };
        assert!(cost_with_prices(
            ModelPrices {
                input_per_mtok: Some(0.20),
                ..Default::default()
            },
            &usage
        )
        .is_none());
        assert!(cost_usd(
            "gemini",
            "gemini-embedding-2",
            &TokenUsage {
                cache_read: 1,
                ..usage
            }
        )
        .is_none());
    }

    #[test]
    fn typesafe_jev_versions_and_aliases_are_priced_as_input_only() {
        for model in ["jev-1.13.0", "jev-latest", "jev-preview"] {
            let cost = cost_usd(
                "typesafe",
                model,
                &TokenUsage {
                    prompt: 1_000_000,
                    completion: 70,
                    ..Default::default()
                },
            )
            .expect("TypeSafe Jev must be priced");
            approx(cost.prompt_usd, 0.042);
            approx(cost.completion_usd, 0.0);
            approx(cost.total_usd, 0.042);
        }
    }

    #[test]
    fn openrouter_jev_logical_and_resolved_build_ids_share_a_price() {
        // The dated build canonicalizes to the logical release. The current
        // OpenRouter card has no price for that release, so both lookups are
        // unpriced rather than diverging.
        assert_eq!(
            upstream_model("openrouter", "openrouter", "typesafe/jev-1.13-20260917"),
            "typesafe/jev-1.13"
        );
        let usage = TokenUsage {
            prompt: 1_000_000,
            completion: 70,
            ..Default::default()
        };
        assert!(cost_usd("openrouter", "typesafe/jev-1.13", &usage).is_none());
        assert!(cost_usd("openrouter", "typesafe/jev-1.13-20260917", &usage).is_none());
        assert!(cost_usd("openrouter", "typesafe/jev-router", &usage).is_none());
        assert!(cost_usd("openrouter", "typesafe/jev-router-20260917", &usage).is_none());
    }

    #[test]
    fn sparse_model_history_selects_changes_and_tombstones() {
        let revisions = [
            ModelPriceRevision(
                1,
                Some(ModelPrices {
                    input_per_mtok: Some(1.0),
                    ..ModelPrices::default()
                }),
            ),
            ModelPriceRevision(
                2,
                Some(ModelPrices {
                    input_per_mtok: Some(2.0),
                    ..ModelPrices::default()
                }),
            ),
            ModelPriceRevision(3, None),
        ];

        assert!(prices_at(&revisions, 0).is_none());
        assert_eq!(
            prices_at(&revisions, 1).and_then(|prices| prices.input_per_mtok),
            Some(1.0)
        );
        assert_eq!(
            prices_at(&revisions, 2).and_then(|prices| prices.input_per_mtok),
            Some(2.0)
        );
        assert!(prices_at(&revisions, 3).is_none());
    }

    #[test]
    fn a_timestamp_before_the_first_card_is_unpriced() {
        assert!(card_effective_at("1970-01-01T00:00:00+00:00").is_none());
        assert!(card_effective_at("9999-01-01T00:00:00+00:00").is_some());
    }

    #[test]
    fn translated_provider_kinds_are_known_registry_kinds() {
        // rate_card_provider special-cases a handful of router provider kinds
        // to LiteLLM's external identifier. If one of those kinds were ever
        // renamed in the registry, its translation rule here would go
        // silently unused rather than fail loudly — pin the mapping's inputs
        // to kinds the registry still recognizes.
        for kind in ["fireworks", "together", "cohere", "vertex"] {
            assert!(
                crate::provider_registry::is_builtin(kind),
                "{kind} is translated by rate_card_provider but is not a known registry kind"
            );
        }
    }
}
