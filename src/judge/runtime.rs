//! The live judge runtime: the pieces that guarantee the judge can never hurt
//! request latency for traffic that shouldn't be downgraded.
//!
//! Isolation guarantees implemented here:
//!   * **Bulkhead** — a bounded semaphore acquired with `try_acquire`; when
//!     saturated the judge is skipped (fail open), never queued, so a judge
//!     slowdown adds zero latency even to downgradable traffic.
//!   * **Deadline** — every score runs under a timeout; on timeout/error the
//!     caller uses the requested (ceiling) model.
//!   * **Score cache** — owned here, but key derivation lives with the caller
//!     (`route`), which chooses between session-affinity and full-conversation
//!     keys and also inserts heuristic verdicts. Failures and fail-open
//!     verdicts are never cached.
//!
//! Judge health is **advisory in one direction only**: an unreachable judge
//! disables judging (fail open, `configured = false`); it never influences
//! *which* rung is chosen. Downgrade decisions are about request difficulty,
//! never about transient upstream connection-test results, which remain
//! advisory and separate from request routing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use super::{DifficultyCache, Judge, JudgeRequest};
use crate::config::JudgeBackendKind;
use crate::types::ChatMessage;

/// Everything needed to run the judge at request time, shared behind an `Arc`.
pub struct JudgeRuntime {
    judge: Arc<dyn Judge>,
    kind: JudgeBackendKind,
    semaphore: Semaphore,
    timeout: Duration,
    cache: DifficultyCache,
    /// Global kill switch (env). Overrides the DB policy's `enabled`.
    kill_switch: bool,
    /// Heuristic pre-filter enabled (env).
    heuristic_prefilter: bool,
    /// Session affinity enabled (env).
    session_affinity: bool,
    /// Readiness + policy generation, packed into one atomic (low bit =
    /// configured, upper 63 bits = generation). `configured` means the last
    /// probe succeeded; `false` short-circuits all traffic (fail open) and is
    /// surfaced via logs; revalidated at boot and on policy updates.
    ///
    /// Packing matters: `invalidate` bumps the generation, and a probe only
    /// lands its result if the generation it started under is still current —
    /// as a single CAS, so a probe begun under a PREVIOUS policy can never
    /// overwrite the readiness of the current one (not even between a
    /// check and a separate store).
    state: AtomicU64,
}

/// Low bit of [`JudgeRuntime::state`]: the configured flag.
const CONFIGURED_BIT: u64 = 1;

impl JudgeRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        judge: Arc<dyn Judge>,
        kind: JudgeBackendKind,
        timeout: Duration,
        max_concurrency: usize,
        kill_switch: bool,
        cache_capacity: usize,
        cache_ttl: Duration,
        heuristic_prefilter: bool,
        session_affinity: bool,
    ) -> Self {
        Self {
            judge,
            kind,
            semaphore: Semaphore::new(max_concurrency.max(1)),
            timeout,
            cache: DifficultyCache::new(cache_capacity, cache_ttl),
            kill_switch,
            heuristic_prefilter,
            session_affinity,
            state: AtomicU64::new(0),
        }
    }

    /// Build a runtime straight from deployment config + a judge impl.
    pub fn from_config(judge: Arc<dyn Judge>, cfg: &crate::config::JudgeConfig) -> Self {
        Self::new(
            judge,
            cfg.backend,
            cfg.timeout,
            cfg.max_concurrency,
            cfg.kill_switch,
            cfg.cache_capacity,
            cfg.cache_ttl,
            cfg.heuristic_prefilter,
            cfg.session_affinity,
        )
    }

    pub fn kill_switch(&self) -> bool {
        self.kill_switch
    }

    pub fn heuristic_prefilter(&self) -> bool {
        self.heuristic_prefilter
    }

    pub fn session_affinity(&self) -> bool {
        self.session_affinity
    }

    /// Whether the judge runs in-pod (sidecar/classifier) with no content
    /// egress — in which case the per-key egress gate does not apply.
    pub fn is_local(&self) -> bool {
        !matches!(self.kind, JudgeBackendKind::Remote)
    }

    pub fn is_configured(&self) -> bool {
        self.state.load(Ordering::Relaxed) & CONFIGURED_BIT != 0
    }

    /// Test-only escape hatch: mark the runtime usable without probing.
    /// Preserves the current generation.
    pub fn force_configured(&self, configured: bool) {
        let _ = self
            .state
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                Some((cur & !CONFIGURED_BIT) | configured as u64)
            });
    }

    /// Mark the runtime not-ready until the next `revalidate` completes, and
    /// bump the policy generation so any probe already in flight for the
    /// PREVIOUS policy has its result discarded.
    ///
    /// Called when the policy changes: the new policy is visible to requests
    /// immediately, but the probe validating it runs async — without this, a
    /// stale `configured = true` earned by the previous policy (including one
    /// landing AFTER the new policy's failed probe) would let requests enter
    /// the judge path against an unvalidated (possibly broken) judge model and
    /// eat the scoring timeout each time. Fails open (judge skipped) until a
    /// probe for the current generation re-establishes readiness.
    pub fn invalidate(&self) {
        let _ = self
            .state
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                Some(((cur >> 1) + 1) << 1)
            });
    }

    /// The current policy generation. Capture this BEFORE reading the policy
    /// that a probe will validate (see [`Self::revalidate`]): generation and
    /// policy then come from the same logical snapshot, so a policy update
    /// landing between the two reads invalidates the probe instead of letting
    /// a probe of the OLD policy pass for the new one.
    pub fn generation(&self) -> u64 {
        self.state.load(Ordering::SeqCst) >> 1
    }

    /// Store a probe outcome iff the generation it started under is still
    /// current; returns `false` (discarding the result) when a newer
    /// `invalidate` superseded the probe mid-flight. Check and store are one
    /// CAS, so a stale probe can never slip in between them.
    fn try_store_configured(&self, start_gen: u64, ok: bool) -> bool {
        self.state
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                (cur >> 1 == start_gen).then_some((cur & !CONFIGURED_BIT) | ok as u64)
            })
            .is_ok()
    }

    pub fn cache_get(&self, key: u64) -> Option<f32> {
        self.cache.get(key)
    }

    pub fn cache_insert(&self, key: u64, difficulty: f32) {
        self.cache.insert(key, difficulty);
    }

    /// Score a request under the bulkhead + deadline, returning its difficulty,
    /// or `None` to fail open (saturated bulkhead, timeout, backend error, or
    /// parse failure). No cache involvement — the caller owns key derivation
    /// and only caches genuine results.
    pub async fn score(
        &self,
        messages: &[ChatMessage],
        requested_model: &str,
        judge_model: &str,
    ) -> Option<f32> {
        // Bulkhead: skip (fail open) rather than queue when saturated.
        let _permit = self.semaphore.try_acquire().ok()?;

        let req = JudgeRequest {
            messages,
            requested_model,
            judge_model,
        };
        match tokio::time::timeout(self.timeout, self.judge.score(&req)).await {
            Ok(Ok(v)) => Some(v.difficulty),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "judge scoring failed; failing open to requested model");
                None
            }
            Err(_) => {
                tracing::warn!("judge scoring timed out; failing open to requested model");
                None
            }
        }
    }

    /// Probe the judge backend for the given model and record whether it is
    /// usable. Runs at boot and whenever the DB policy changes the judge model,
    /// so an unconfigured/unreachable judge is visible instead of silently
    /// failing open on every request. A disabled policy (empty judge model)
    /// marks the judge unconfigured without probing.
    /// `start_gen` is the generation captured — via [`Self::generation`] —
    /// BEFORE the caller read the `enabled`/`judge_model` pair it passes here.
    /// Capturing it any later (e.g. in this function) would let a policy
    /// update slip between the caller's policy read and the capture, and a
    /// probe of the old policy would then pass for the new one. If an
    /// `invalidate` lands after the capture, the result below is stale and
    /// gets discarded instead of stored.
    pub async fn revalidate(&self, start_gen: u64, enabled: bool, judge_model: &str) {
        if !enabled || judge_model.is_empty() {
            self.try_store_configured(start_gen, false);
            return;
        }
        // A trivial, user-data-free scoring probe exercises the real credential
        // resolution + call path.
        let probe = [ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::Value::String("ping".to_string())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }];
        let req = JudgeRequest {
            messages: &probe,
            requested_model: judge_model,
            judge_model,
        };
        let ok = matches!(
            tokio::time::timeout(self.timeout, self.judge.score(&req)).await,
            Ok(Ok(_))
        );
        if !self.try_store_configured(start_gen, ok) {
            tracing::info!(
                judge_model,
                "Discarding stale judge probe result: the policy changed mid-probe"
            );
            return;
        }
        if ok {
            tracing::info!(judge_model, "Judge backend configured and reachable");
        } else {
            tracing::warn!(
                judge_model,
                "Judge backend not usable (unconfigured/unreachable); judging disabled until fixed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use async_trait::async_trait;

    use super::*;
    use crate::judge::{JudgeError, JudgeVerdict};

    /// A `Judge` whose outcome is fixed by the test, so `revalidate`'s CAS can
    /// be driven deterministically.
    struct StubJudge {
        ok: bool,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Judge for StubJudge {
        async fn score(&self, _req: &JudgeRequest<'_>) -> Result<JudgeVerdict, JudgeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.ok {
                Ok(JudgeVerdict { difficulty: 0.5 })
            } else {
                Err(JudgeError::Backend("stub failure".to_string()))
            }
        }
    }

    fn runtime_with(ok: bool) -> (JudgeRuntime, Arc<StubJudge>) {
        let judge = Arc::new(StubJudge {
            ok,
            calls: AtomicUsize::new(0),
        });
        let rt = JudgeRuntime::new(
            judge.clone(),
            JudgeBackendKind::Remote,
            Duration::from_secs(5),
            4,
            false,
            16,
            Duration::from_secs(60),
            false,
            false,
        );
        (rt, judge)
    }

    #[tokio::test]
    async fn revalidate_disabled_or_empty_model_short_circuits_without_probing() {
        let (rt, judge) = runtime_with(true);
        rt.force_configured(true);

        let gen = rt.generation();
        rt.revalidate(gen, false, "openai/judge").await;
        assert!(
            !rt.is_configured(),
            "disabled policy must mark unconfigured"
        );
        assert_eq!(
            judge.calls.load(Ordering::SeqCst),
            0,
            "must not call the judge when the policy is disabled"
        );

        rt.force_configured(true);
        let gen = rt.generation();
        rt.revalidate(gen, true, "").await;
        assert!(
            !rt.is_configured(),
            "empty judge_model must mark unconfigured"
        );
        assert_eq!(
            judge.calls.load(Ordering::SeqCst),
            0,
            "must not call the judge with an empty model id"
        );
    }

    #[tokio::test]
    async fn revalidate_success_marks_configured() {
        let (rt, judge) = runtime_with(true);
        let gen = rt.generation();
        rt.revalidate(gen, true, "openai/judge").await;
        assert!(rt.is_configured());
        assert_eq!(judge.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn revalidate_failure_marks_unconfigured() {
        let (rt, _judge) = runtime_with(false);
        rt.force_configured(true);
        let gen = rt.generation();
        rt.revalidate(gen, true, "openai/judge").await;
        assert!(
            !rt.is_configured(),
            "a failing probe must disable judging, never leave the stale `true` in place"
        );
    }

    #[test]
    fn try_store_configured_discards_a_result_from_a_superseded_generation() {
        // `invalidate` simulates a policy change landing while a probe for the
        // OLD generation is still in flight; that probe's eventual result must
        // never be allowed to overwrite the fresh (post-invalidate) state.
        let (rt, _judge) = runtime_with(true);
        let start_gen = rt.generation();

        rt.invalidate();
        assert!(
            rt.generation() > start_gen,
            "invalidate must bump the generation"
        );

        let stored = rt.try_store_configured(start_gen, true);
        assert!(!stored, "a stale-generation probe result must be rejected");
        assert!(
            !rt.is_configured(),
            "rejecting the stale write must leave `is_configured` at its current (unconfigured) value"
        );

        // A probe for the CURRENT generation still writes through.
        let cur_gen = rt.generation();
        assert!(rt.try_store_configured(cur_gen, true));
        assert!(rt.is_configured());
    }
}
