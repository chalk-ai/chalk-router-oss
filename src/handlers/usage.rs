//! Shared token accounting for generation frontends.

use std::future::Future;
use std::pin::Pin;

use crate::backend::RouteResolution;
use crate::handlers::trace_attributes::LlmSpanWriter;
use crate::usage_estimator::UsageEstimateState;
/// Protocol-neutral token counts. Provider `output_tokens` already includes
/// reasoning tokens; `reasoning_tokens` is a detail field and must never be
/// added to the completion or total count a second time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Whether the upstream usage object actually contained an output-token
    /// count. The ledger still records zero when absent, while telemetry omits
    /// the attribute instead of fabricating a measurement.
    pub output_tokens_observed: bool,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub usage_source: crate::usage_tracker::UsageSource,
    #[serde(default)]
    pub input_token_attribution: crate::pricing::InputTokenAttribution,
}

#[derive(Debug, Clone)]
pub(crate) struct UsageAttribution {
    event_id: uuid::Uuid,
    key_hash: String,
    usage_pool_id: Option<String>,
    resolution: RouteResolution,
}

type BoxUsageWriteFuture = Pin<
    Box<dyn Future<Output = Result<(), crate::usage_tracker::UsageWriteError>> + Send + 'static>,
>;

/// Retain the accounting intent until settlement is attached, instead of
/// hiding the usage insert inside an opaque future before it can be combined.
pub(crate) struct UsageWriteFuture {
    intent: Option<(crate::AppState, UsageAttribution, TokenUsage)>,
    future: Option<BoxUsageWriteFuture>,
}

impl UsageWriteFuture {
    fn running(future: BoxUsageWriteFuture) -> Self {
        Self {
            intent: None,
            future: Some(future),
        }
    }

    pub(crate) fn as_mut(
        &mut self,
    ) -> Pin<&mut (dyn Future<Output = Result<(), crate::usage_tracker::UsageWriteError>> + Send)>
    {
        if let Some((state, attribution, usage)) = self.intent.take() {
            self.future = Some(Box::pin(async move {
                record_attribution(&state, attribution, usage).await
            }));
        }
        self.future
            .as_mut()
            .expect("usage accounting future")
            .as_mut()
    }
}

impl Future for UsageWriteFuture {
    type Output = Result<(), crate::usage_tracker::UsageWriteError>;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.get_mut().as_mut().poll(cx)
    }
}

impl UsageAttribution {
    pub(crate) fn new(
        api_key: &crate::middleware::ResolvedApiKey,
        resolution: &RouteResolution,
    ) -> Self {
        Self {
            event_id: uuid::Uuid::new_v4(),
            key_hash: api_key.0.key_hash.clone(),
            usage_pool_id: api_key.0.usage_pool_id.clone(),
            resolution: resolution.clone(),
        }
    }

    pub(crate) fn resolution(&self) -> &RouteResolution {
        &self.resolution
    }
}

impl TokenUsage {
    pub(crate) fn derived_total_tokens(self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    pub(crate) fn from_chat(usage: &crate::types::Usage) -> Option<Self> {
        let completion_tokens = usage.completion_tokens?;
        let input_tokens = u64::try_from(usage.prompt_tokens).ok()?;
        let output_tokens = u64::try_from(completion_tokens).ok()?;
        let cached_input_tokens = u64::try_from(
            usage
                .prompt_tokens_details
                .as_ref()
                .map_or(0, |details| details.cached_tokens),
        )
        .ok()?;
        let cache_write_input_tokens = u64::try_from(
            usage
                .prompt_tokens_details
                .as_ref()
                .map_or(0, |details| details.cache_write_tokens),
        )
        .ok()?;
        let reasoning_tokens = u64::try_from(
            usage
                .completion_tokens_details
                .as_ref()
                .map_or(0, |details| details.reasoning_tokens),
        )
        .ok()?;
        if cached_input_tokens > input_tokens
            || cache_write_input_tokens > input_tokens
            || reasoning_tokens > output_tokens
        {
            return None;
        }
        warn_on_reported_total_mismatch(
            "chat",
            i64::from(usage.total_tokens),
            input_tokens,
            output_tokens,
        );
        Some(Self {
            input_tokens,
            output_tokens,
            output_tokens_observed: true,
            cached_input_tokens,
            cache_write_input_tokens,
            reasoning_tokens,
            total_tokens: input_tokens.saturating_add(output_tokens),
            usage_source: crate::usage_tracker::UsageSource::Reported,
            ..Self::default()
        })
    }

    pub(crate) fn from_responses_value(value: &serde_json::Value) -> Option<Self> {
        let input_tokens = value.get("input_tokens")?.as_i64()?;
        let output_tokens = value.get("output_tokens")?.as_i64()?;
        let input_tokens = u64::try_from(input_tokens).ok()?;
        let output_tokens = u64::try_from(output_tokens).ok()?;
        let usage =
            serde_json::from_value::<crate::responses::ResponseUsage>(value.clone()).ok()?;
        let cached_input_tokens = u64::try_from(usage.input_tokens_details.cached_tokens).ok()?;
        let cache_write_input_tokens =
            u64::try_from(usage.input_tokens_details.cache_write_tokens).ok()?;
        let reasoning_tokens = u64::try_from(usage.output_tokens_details.reasoning_tokens).ok()?;
        if cached_input_tokens > input_tokens
            || cache_write_input_tokens > input_tokens
            || reasoning_tokens > output_tokens
        {
            return None;
        }
        warn_on_reported_total_mismatch(
            "responses",
            usage.total_tokens,
            input_tokens,
            output_tokens,
        );
        Some(Self {
            input_tokens,
            output_tokens,
            output_tokens_observed: true,
            cached_input_tokens,
            cache_write_input_tokens,
            reasoning_tokens,
            total_tokens: input_tokens.saturating_add(output_tokens),
            usage_source: crate::usage_tracker::UsageSource::Reported,
            ..Self::default()
        })
    }

    pub(crate) fn from_decisions(usage: &crate::decisions::DecisionsUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            output_tokens_observed: true,
            total_tokens: usage.input_tokens.saturating_add(usage.output_tokens),
            usage_source: crate::usage_tracker::UsageSource::Reported,
            ..Self::default()
        }
    }

    pub(crate) fn from_estimate(estimate: UsageEstimateState) -> Self {
        let (input, output) = estimate.finish();
        tracing::warn!(
            method = ?input.method,
            input_tokens = input.tokens,
            output_tokens = output.tokens,
            "Provider usage was missing or invalid; using conservative estimate"
        );
        Self {
            input_tokens: input.tokens,
            output_tokens: output.tokens,
            output_tokens_observed: true,
            total_tokens: input.tokens.saturating_add(output.tokens),
            usage_source: crate::usage_tracker::UsageSource::Estimated,
            ..Self::default()
        }
    }

    pub(crate) fn stamp(self, span: &LlmSpanWriter) {
        span.set_token_usage_with_attribution(
            self.input_tokens,
            self.output_tokens_observed.then_some(self.output_tokens),
            self.total_tokens,
            self.cached_input_tokens,
            self.cache_write_input_tokens,
            self.reasoning_tokens,
            self.input_token_attribution,
        );
    }
}

fn warn_on_reported_total_mismatch(
    protocol: &'static str,
    reported_total: i64,
    input_tokens: u64,
    output_tokens: u64,
) {
    let derived = input_tokens.saturating_add(output_tokens);
    if u64::try_from(reported_total).ok() != Some(derived) {
        tracing::warn!(
            protocol,
            reported_total,
            derived_total = derived,
            "Provider-reported token total disagrees with normalized components"
        );
    }
}

/// Record one completed response against the presented key and the exact
/// connection that won routing. The protocol never enters the ledger key, so
/// chat and Responses reconcile into one billing contract.
pub(crate) async fn record_attribution(
    state: &crate::AppState,
    attribution: UsageAttribution,
    usage: TokenUsage,
) -> Result<(), crate::usage_tracker::UsageWriteError> {
    record_attribution_with_quota(state, attribution, usage, None, None).await
}

async fn record_attribution_with_quota(
    state: &crate::AppState,
    attribution: UsageAttribution,
    usage: TokenUsage,
    permit: Option<crate::quota::QuotaPermit>,
    actual_amount: Option<u64>,
) -> Result<(), crate::usage_tracker::UsageWriteError> {
    if let Some(ref tracker) = state.usage_tracker {
        let Some(connection_id) = attribution.resolution.connection_id.as_deref() else {
            if let Some(permit) = permit {
                permit.finish(actual_amount).await;
            }
            return Err(crate::usage_tracker::UsageWriteError::Invalid(
                "resolved usage has no provider connection",
            ));
        };
        tracker
            .record_with_quota(
                crate::usage_tracker::UsageRecord {
                    event_id: attribution.event_id,
                    time: chrono::Utc::now(),
                    key_hash: &attribution.key_hash,
                    usage_pool_id: attribution.usage_pool_id.as_deref(),
                    connection_id,
                    provider_kind: &attribution.resolution.provider_kind,
                    model: &attribution.resolution.upstream_model_id,
                    prompt_tokens: usage.input_tokens,
                    completion_tokens: usage.output_tokens,
                    cached_tokens: usage.cached_input_tokens,
                    cache_write_tokens: usage.cache_write_input_tokens,
                    reasoning_tokens: usage.reasoning_tokens,
                    usage_source: usage.usage_source,
                    input_token_attribution: usage.input_token_attribution,
                },
                permit,
                actual_amount,
            )
            .await?;
    } else if let Some(permit) = permit {
        permit.finish(actual_amount).await;
    }

    state
        .api_key_store
        .record_usage(
            &attribution.key_hash,
            usage.input_tokens,
            usage.output_tokens,
            usage.derived_total_tokens(),
        )
        .await;
    Ok(())
}

pub(crate) fn record_future(
    state: crate::AppState,
    attribution: UsageAttribution,
    usage: TokenUsage,
) -> UsageWriteFuture {
    UsageWriteFuture {
        intent: Some((state, attribution, usage)),
        future: None,
    }
}

pub(crate) fn missing_usage_future() -> UsageWriteFuture {
    UsageWriteFuture::running(Box::pin(async {
        Err(crate::usage_tracker::UsageWriteError::Invalid(
            "successful response had neither valid reported usage nor a supported estimator",
        ))
    }))
}

pub(crate) fn empty_usage_future() -> UsageWriteFuture {
    UsageWriteFuture::running(Box::pin(async { Ok(()) }))
}

/// Reconcile quota before completing the associated usage write. Passing no
/// actual amount deliberately retains the conservative token charge while
/// still releasing concurrency. Keeping the permit inside this future lets
/// non-clean stream paths schedule explicit settlement instead of relying on
/// `QuotaPermit::drop` as their normal control flow.
pub(crate) fn with_quota_settlement(
    mut future: UsageWriteFuture,
    permit: Option<crate::quota::QuotaPermit>,
    actual_amount: Option<u64>,
) -> UsageWriteFuture {
    if let Some((state, attribution, usage)) = future.intent.take() {
        return UsageWriteFuture::running(Box::pin(async move {
            record_attribution_with_quota(&state, attribution, usage, permit, actual_amount).await
        }));
    }
    UsageWriteFuture::running(Box::pin(async move {
        if let Some(permit) = permit {
            permit.finish(actual_amount).await;
        }
        future.await
    }))
}

pub(crate) fn detach_future(future: UsageWriteFuture) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            if let Err(error) = future.await {
                tracing::error!(error = %error, "Detached usage write failed");
            }
        });
    } else {
        tracing::error!("Durable usage write could not run outside a Tokio runtime");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_reasoning_is_not_added_to_output_twice() {
        let usage = crate::responses::ResponseUsage {
            input_tokens: 100,
            input_tokens_details: crate::responses::ResponseInputTokenDetails {
                cached_tokens: 40,
                cache_write_tokens: 10,
                ..Default::default()
            },
            output_tokens: 30,
            output_tokens_details: crate::responses::ResponseOutputTokenDetails {
                reasoning_tokens: 20,
                ..Default::default()
            },
            total_tokens: 130,
            ..Default::default()
        };

        assert_eq!(
            TokenUsage::from_responses_value(&serde_json::to_value(&usage).unwrap()).unwrap(),
            TokenUsage {
                input_tokens: 100,
                output_tokens: 30,
                output_tokens_observed: true,
                cached_input_tokens: 40,
                cache_write_input_tokens: 10,
                reasoning_tokens: 20,
                total_tokens: 130,
                ..Default::default()
            }
        );
    }

    #[test]
    fn chat_omitted_completion_tokens_remains_unobserved() {
        let usage = crate::types::Usage {
            prompt_tokens: 12,
            completion_tokens: None,
            total_tokens: 12,
            ..Default::default()
        };

        assert_eq!(TokenUsage::from_chat(&usage), None);
    }

    #[test]
    fn chat_normalizes_cache_and_reasoning_details() {
        let usage = crate::types::Usage {
            prompt_tokens: 20,
            completion_tokens: Some(10),
            total_tokens: 999,
            prompt_tokens_details: Some(crate::types::PromptTokenDetails {
                cached_tokens: 8,
                cache_write_tokens: 3,
            }),
            completion_tokens_details: Some(crate::types::CompletionTokenDetails {
                reasoning_tokens: 7,
            }),
            ..Default::default()
        };
        let normalized = TokenUsage::from_chat(&usage).unwrap();
        assert_eq!(normalized.input_tokens, 20);
        assert_eq!(normalized.output_tokens, 10);
        assert_eq!(normalized.cached_input_tokens, 8);
        assert_eq!(normalized.cache_write_input_tokens, 3);
        assert_eq!(normalized.reasoning_tokens, 7);
        assert_eq!(normalized.total_tokens, 30);
    }

    #[test]
    fn explicitly_reported_zero_usage_is_authoritative() {
        let usage = crate::types::Usage {
            prompt_tokens: 0,
            completion_tokens: Some(0),
            total_tokens: 0,
            ..Default::default()
        };
        let normalized = TokenUsage::from_chat(&usage).unwrap();
        assert_eq!(
            normalized.usage_source,
            crate::usage_tracker::UsageSource::Reported
        );
        assert_eq!(normalized.derived_total_tokens(), 0);
    }

    #[test]
    fn responses_requires_both_top_level_components() {
        assert!(TokenUsage::from_responses_value(&serde_json::json!({
            "input_tokens": 10,
            "total_tokens": 10
        }))
        .is_none());
        assert!(TokenUsage::from_responses_value(&serde_json::json!({
            "input_tokens": 10,
            "output_tokens": 2,
            "total_tokens": 12,
            "input_tokens_details": {"cached_tokens": 11}
        }))
        .is_none());
    }
}
