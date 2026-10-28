use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Utc};
use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};
use tokio::sync::Mutex;
use uuid::Uuid;

mod operations;
pub use operations::{BackgroundResponseEntry, BackgroundResponsePage};

use crate::{
    backend::{provider::ResponsesRequestHeaders, BackendError, RouteResolution},
    handlers::{
        trace_attributes::{LlmSpanKind, LlmSpanWriter},
        usage::TokenUsage,
    },
    middleware::ResolvedApiKey,
    sampling_policy::SamplingPolicy,
    usage_tracker::UsageRecord,
};

const CLEANUP_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug)]
pub struct PollConfig {
    pub interval: Duration,
    pub batch_size: usize,
    pub concurrency: usize,
    pub retention_days: u32,
}

impl PollConfig {
    pub fn new(interval_secs: u64, batch_size: usize, concurrency: usize) -> Self {
        assert!(
            interval_secs > 0 && interval_secs <= 86400,
            "background polling interval must be in 1..=86400 seconds"
        );
        assert!(
            batch_size > 0 && batch_size <= 10000,
            "background poll batch size must be in 1..=10000"
        );
        assert!(
            concurrency > 0 && concurrency <= 1000,
            "background poll concurrency must be in 1..=1000"
        );
        Self {
            interval: Duration::from_secs(interval_secs),
            batch_size,
            concurrency,
            retention_days: 30,
        }
    }

    pub fn with_retention_days(mut self, days: u32) -> Self {
        self.retention_days = days;
        self
    }
}

impl Default for PollConfig {
    fn default() -> Self {
        Self::new(30, 100, 4)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct TraceContext {
    pub trace_id: String,
    pub span_id: String,
    pub trace_flags: u8,
    pub baggage: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Job {
    #[serde(default)]
    pub submission_rejected: bool,
    pub event_id: Uuid,
    pub key_hash: String,
    pub usage_pool_id: Option<String>,
    pub requested_model: String,
    pub served_model: String,
    pub connection_id: Option<String>,
    pub provider_kind: String,
    pub upstream_model: String,
    pub upstream_response_id: Option<String>,
    pub phase: String,
    pub upstream_status: Option<String>,
    pub usage: Option<TokenUsage>,
    pub rate_card_version: String,
    pub sampling_policy: SamplingPolicy,
    pub trace_context: TraceContext,
    pub outcome_rate: Option<f64>,
    pub submitted_at: DateTime<Utc>,
    pub terminal_at: Option<DateTime<Utc>>,
    pub poll_attempts: u64,
}

struct MemoryJob {
    job: Job,
    due: DateTime<Utc>,
    claim_until: Option<DateTime<Utc>>,
    settled_at: Option<DateTime<Utc>>,
    last_error_code: Option<String>,
}

#[derive(Default)]
struct MemoryState {
    jobs: HashMap<Uuid, MemoryJob>,
    settled: BTreeSet<(DateTime<Utc>, Uuid)>,
}

pub struct BackgroundResponses {
    pool: Option<PgPool>,
    environment_id: String,
    memory: Mutex<MemoryState>,
    config: PollConfig,
    reconcile_guard: tokio::sync::Semaphore,
}

fn unavailable() -> BackendError {
    BackendError::api_status(
        503,
        "background response accounting is temporarily unavailable",
    )
}

fn database_error(error: impl std::fmt::Display) -> BackendError {
    tracing::error!(%error, "background response accounting failed");
    unavailable()
}

impl BackgroundResponses {
    pub fn new(pool: Option<PgPool>, environment_id: String, config: PollConfig) -> Self {
        Self {
            pool,
            environment_id,
            memory: Mutex::new(MemoryState::default()),
            config,
            reconcile_guard: tokio::sync::Semaphore::new(1),
        }
    }

    pub(crate) async fn begin(
        &self,
        state: &crate::AppState,
        key: &ResolvedApiKey,
        model: &str,
        span: &LlmSpanWriter,
    ) -> Result<Uuid, BackendError> {
        let event_id = Uuid::new_v4();
        let job = Job {
            submission_rejected: false,
            event_id,
            key_hash: key.0.key_hash.clone(),
            usage_pool_id: key.0.usage_pool_id.clone(),
            requested_model: model.to_string(),
            served_model: model.to_string(),
            connection_id: None,
            provider_kind: String::new(),
            upstream_model: String::new(),
            upstream_response_id: None,
            phase: "submitting".to_string(),
            upstream_status: None,
            usage: None,
            rate_card_version: crate::pricing::current().effective_from.clone(),
            sampling_policy: state.sampling_policy.get().await.map_err(database_error)?,
            trace_context: span.background_context(),
            outcome_rate: None,
            submitted_at: Utc::now(),
            terminal_at: None,
            poll_attempts: 0,
        };
        let due = Utc::now() + chrono::Duration::minutes(5);
        if let Some(pool) = &self.pool {
            sqlx::query("INSERT INTO chalk_router.background_response_jobs (environment_id,event_id,phase,payload,next_poll_at) VALUES ($1,$2,$3,$4,$5)")
                .bind(&self.environment_id).bind(event_id).bind(&job.phase)
                .bind(serde_json::to_value(&job).map_err(database_error)?).bind(due)
                .execute(pool).await.map_err(database_error)?;
        } else {
            self.memory.lock().await.jobs.insert(
                event_id,
                MemoryJob {
                    job,
                    due,
                    claim_until: None,
                    settled_at: None,
                    last_error_code: None,
                },
            );
        }
        Ok(event_id)
    }

    async fn change(
        &self,
        event_id: Uuid,
        change: impl FnOnce(&mut Job),
    ) -> Result<(), BackendError> {
        if let Some(pool) = &self.pool {
            let mut transaction = pool.begin().await.map_err(database_error)?;
            if let Some(mut job) = self.lock_job(&mut transaction, event_id).await? {
                change(&mut job);
                self.save(&mut transaction, &job).await?;
            }
            transaction.commit().await.map_err(database_error)?;
        } else if let Some(entry) = self.memory.lock().await.jobs.get_mut(&event_id) {
            change(&mut entry.job);
            entry.due = if entry.job.phase == "submitting" {
                Utc::now() + chrono::Duration::minutes(5)
            } else {
                Utc::now()
            };
        }
        Ok(())
    }

    async fn lock_job(
        &self,
        connection: &mut PgConnection,
        event_id: Uuid,
    ) -> Result<Option<Job>, BackendError> {
        let row = sqlx::query("SELECT payload,poll_attempts FROM chalk_router.background_response_jobs WHERE environment_id=$1 AND event_id=$2 FOR UPDATE")
            .bind(&self.environment_id).bind(event_id).fetch_optional(connection).await.map_err(database_error)?;
        row.map(|row| {
            let mut job: Job =
                serde_json::from_value(row.get("payload")).map_err(database_error)?;
            job.poll_attempts = row.get::<i64, _>("poll_attempts") as u64;
            Ok(job)
        })
        .transpose()
    }

    async fn save(&self, connection: &mut PgConnection, job: &Job) -> Result<(), BackendError> {
        sqlx::query("UPDATE chalk_router.background_response_jobs SET connection_id=$3,upstream_response_id=$4,phase=$5,payload=$6,next_poll_at=CASE WHEN $5='submitting' THEN now()+interval '5 minutes' ELSE now() END,updated_at=now(),settled_at=CASE WHEN $5='settled' THEN COALESCE(settled_at,now()) ELSE settled_at END WHERE environment_id=$1 AND event_id=$2")
            .bind(&self.environment_id).bind(job.event_id).bind(&job.connection_id).bind(&job.upstream_response_id)
            .bind(&job.phase).bind(serde_json::to_value(job).map_err(database_error)?)
            .execute(connection).await.map_err(database_error)?;
        Ok(())
    }

    pub(crate) async fn prepare(
        &self,
        event_id: Uuid,
        resolution: &RouteResolution,
        served_model: &str,
    ) -> Result<(), BackendError> {
        if resolution.connection_id.is_none() {
            return Err(BackendError::api_status(
                400,
                "background Responses require a stored provider connection",
            ));
        }
        self.change(event_id, |job| {
            job.connection_id = resolution.connection_id.clone();
            job.provider_kind = resolution.provider_kind.clone();
            job.upstream_model = resolution.upstream_model_id.clone();
            job.served_model = served_model.to_string();
            job.rate_card_version = crate::pricing::current().effective_from.clone();
        })
        .await
    }

    pub(crate) async fn accept(&self, event_id: Uuid, body: &Value) -> Result<(), BackendError> {
        let upstream_id = body
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(unavailable)?;
        self.change(event_id, |job| {
            if job.phase == "settled" {
                return;
            }
            if job
                .upstream_response_id
                .as_deref()
                .is_some_and(|known| known != upstream_id)
            {
                return;
            }
            job.upstream_response_id = Some(upstream_id.to_string());
            observe_job(job, body);
        })
        .await
    }

    pub(crate) async fn observe(
        &self,
        state: &crate::AppState,
        connection_id: &str,
        body: &Value,
    ) -> Result<(), BackendError> {
        let Some(upstream_id) = body.get("id").and_then(Value::as_str) else {
            return Ok(());
        };
        let event_id = if let Some(pool) = &self.pool {
            sqlx::query_scalar::<_,Uuid>("SELECT event_id FROM chalk_router.background_response_jobs WHERE environment_id=$1 AND connection_id=$2 AND upstream_response_id=$3")
                .bind(&self.environment_id).bind(connection_id).bind(upstream_id).fetch_optional(pool).await.map_err(database_error)?
        } else {
            self.memory
                .lock()
                .await
                .jobs
                .values()
                .find(|entry| {
                    entry.job.connection_id.as_deref() == Some(connection_id)
                        && entry.job.upstream_response_id.as_deref() == Some(upstream_id)
                })
                .map(|entry| entry.job.event_id)
        };
        if let Some(event_id) = event_id {
            self.change(event_id, |job| observe_job(job, body)).await?;
            self.settle(state, event_id).await?;
        }
        Ok(())
    }

    pub(crate) async fn submission_failed(&self, event_id: Uuid) {
        if let Err(error) = self
            .change(event_id, |job| {
                if job.upstream_response_id.is_none() {
                    job.phase = "unresolved".to_string();
                }
            })
            .await
        {
            tracing::error!(%error, %event_id, "could not record unresolved background submission");
        }
    }

    pub(crate) async fn reject_submission(&self, state: &crate::AppState, event_id: Uuid) {
        let result = async {
            self.change(event_id, |job| {
                job.phase = "unresolved".to_string();
                job.submission_rejected = true;
            })
            .await?;
            self.settle(state, event_id).await
        }
        .await;
        if let Err(error) = result {
            tracing::error!(%event_id,%error,"background rejection settlement will retry");
        }
    }

    pub(crate) async fn settle(
        &self,
        state: &crate::AppState,
        event_id: Uuid,
    ) -> Result<(), BackendError> {
        let settled = if let Some(pool) = &self.pool {
            let mut transaction = pool.begin().await.map_err(database_error)?;
            let Some(mut job) = self.lock_job(&mut transaction, event_id).await? else {
                return Ok(());
            };
            if !settleable(&job) {
                return Ok(());
            }
            let owner = crate::quota::BackgroundOwner {
                environment_id: self.environment_id.clone(),
                event_id,
            };
            let outcome = quota_outcome(&job);
            if !state
                .quota_backend
                .finish_background_in_transaction(&mut transaction, &owner, outcome)
                .await
                .map_err(database_error)?
            {
                state
                    .quota_backend
                    .finish_background(&owner, outcome)
                    .await
                    .map_err(database_error)?;
            }
            if let (Some(tracker), Some(usage)) = (&state.usage_tracker, job.usage) {
                tracker
                    .record_in_transaction(
                        &mut transaction,
                        usage_record(&job, usage)?,
                        &job.rate_card_version,
                    )
                    .await
                    .map_err(database_error)?;
            }
            job.phase = "settled".to_string();
            self.save(&mut transaction, &job).await?;
            transaction.commit().await.map_err(database_error)?;
            Some(job)
        } else {
            let mut jobs = self.memory.lock().await;
            let Some(entry) = jobs.jobs.get_mut(&event_id) else {
                return Ok(());
            };
            if !settleable(&entry.job) {
                return Ok(());
            }
            let owner = crate::quota::BackgroundOwner {
                environment_id: self.environment_id.clone(),
                event_id,
            };
            state
                .quota_backend
                .finish_background(&owner, quota_outcome(&entry.job))
                .await
                .map_err(database_error)?;
            if let (Some(tracker), Some(usage)) = (&state.usage_tracker, entry.job.usage) {
                tracker
                    .record_at_version(
                        usage_record(&entry.job, usage)?,
                        &entry.job.rate_card_version,
                    )
                    .await
                    .map_err(database_error)?;
            }
            entry.job.phase = "settled".to_string();
            let settled_at = Utc::now();
            entry.settled_at = Some(settled_at);
            let job = entry.job.clone();
            jobs.settled.insert((settled_at, event_id));
            Some(job)
        };
        if let Some(job) = settled {
            if let Some(usage) = job.usage {
                state
                    .api_key_store
                    .record_usage(
                        &job.key_hash,
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.derived_total_tokens(),
                    )
                    .await;
            }
            emit_settlement(state, &job).await;
        }
        Ok(())
    }

    async fn claim(&self) -> Result<Vec<Job>, BackendError> {
        let lease_seconds =
            60 * (self.config.batch_size.div_ceil(self.config.concurrency) as i64) + 30;
        if let Some(pool) = &self.pool {
            let rows = sqlx::query("WITH due AS (SELECT environment_id,event_id FROM chalk_router.background_response_jobs WHERE environment_id=$1 AND phase IN ('submitting','active','terminal','unresolved') AND next_poll_at<=now() AND (claim_until IS NULL OR claim_until<=now()) ORDER BY next_poll_at LIMIT $2 FOR UPDATE SKIP LOCKED) UPDATE chalk_router.background_response_jobs AS job SET claim_owner=$3,claim_until=now()+make_interval(secs=>$4),poll_attempts=job.poll_attempts+1 FROM due WHERE job.environment_id=due.environment_id AND job.event_id=due.event_id RETURNING job.payload,job.poll_attempts")
                .bind(&self.environment_id).bind(self.config.batch_size as i64).bind(Uuid::new_v4()).bind(lease_seconds as f64)
                .fetch_all(pool).await.map_err(database_error)?;
            rows.into_iter()
                .map(|row| {
                    let mut job: Job =
                        serde_json::from_value(row.get("payload")).map_err(database_error)?;
                    job.poll_attempts = row.get::<i64, _>("poll_attempts") as u64;
                    Ok(job)
                })
                .collect()
        } else {
            let now = Utc::now();
            let mut jobs = self.memory.lock().await;
            Ok(jobs
                .jobs
                .values_mut()
                .filter(|entry| {
                    entry.job.phase != "settled"
                        && entry.due <= now
                        && entry.claim_until.is_none_or(|expiry| expiry <= now)
                })
                .take(self.config.batch_size)
                .map(|entry| {
                    entry.claim_until = Some(now + chrono::Duration::seconds(lease_seconds));
                    entry.job.poll_attempts += 1;
                    entry.job.clone()
                })
                .collect())
        }
    }

    async fn reschedule(&self, job: &Job, failed: bool) -> Result<(), BackendError> {
        let multiplier = if failed {
            1u32 << job.poll_attempts.min(6)
        } else {
            1
        };
        let delay = self
            .config
            .interval
            .saturating_mul(multiplier)
            .min(Duration::from_secs(3600));
        let jitter = job.event_id.as_u128() as u64 % (delay.as_secs() / 5 + 1);
        let due = Utc::now() + chrono::Duration::seconds((delay.as_secs() + jitter) as i64);
        if let Some(pool) = &self.pool {
            sqlx::query("UPDATE chalk_router.background_response_jobs SET next_poll_at=$3,claim_owner=NULL,claim_until=NULL,last_error_code=$4 WHERE environment_id=$1 AND event_id=$2")
                .bind(&self.environment_id).bind(job.event_id).bind(due).bind(failed.then_some("upstream_or_settlement_unavailable"))
                .execute(pool).await.map_err(database_error)?;
        } else if let Some(entry) = self.memory.lock().await.jobs.get_mut(&job.event_id) {
            entry.due = due;
            entry.claim_until = None;
            entry.last_error_code =
                failed.then(|| "upstream_or_settlement_unavailable".to_string());
        }
        Ok(())
    }

    pub async fn reconcile(&self, state: &crate::AppState) -> Result<(), BackendError> {
        // Only this environment serializes reconciles. A slow provider cannot
        // block another tenant's workers. Each environment has bounded poll
        // concurrency, and only confirmed environments get a worker.
        let _permit = self
            .reconcile_guard
            .acquire()
            .await
            .map_err(|_| unavailable())?;
        let jobs = self.claim().await?;
        stream::iter(jobs).for_each_concurrent(self.config.concurrency, |job| async move {
            let result = async {
                if settleable(&job) { return self.settle(state, job.event_id).await; }
                let (Some(connection_id),Some(upstream_id)) = (&job.connection_id,&job.upstream_response_id) else {
                    self.submission_failed(job.event_id).await;
                    return Err(unavailable());
                };
                let route = state.backend.resolve_connection(connection_id).await?;
                let upstream = tokio::time::timeout(Duration::from_secs(30), route.backend.retrieve_response(upstream_id, &ResponsesRequestHeaders::default()))
                    .await.map_err(|_| unavailable())??;
                self.observe(state, connection_id, &upstream.body).await
            }.await;
            if let Err(error) = &result { tracing::warn!(event_id=%job.event_id, %error, "background reconciliation will retry"); }
            if let Err(error) = self.reschedule(&job, result.is_err()).await { tracing::error!(%error, "background rescheduling failed"); }
        }).await;
        Ok(())
    }

    pub fn start(self: &Arc<Self>, state: crate::AppState) -> tokio::task::JoinHandle<()> {
        let runtime = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(runtime.config.interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut cleanup = tokio::time::interval(CLEANUP_INTERVAL);
            cleanup.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(error) = runtime.reconcile(&state).await {
                            tracing::error!(%error, "background worker failed to claim jobs");
                        }
                    },
                    _ = cleanup.tick() => {
                        match runtime.cleanup().await {
                            Ok(deleted) if deleted > 0 => tracing::info!(deleted, "settled background response metadata pruned"),
                            Ok(_) => {},
                            Err(error) => tracing::warn!(%error, "background response cleanup will retry"),
                        }
                    }
                }
            }
        })
    }
}

fn observe_job(job: &mut Job, body: &Value) {
    if job.phase == "settled" || job.phase == "terminal" {
        return;
    }
    let status = body.get("status").and_then(Value::as_str);
    job.upstream_status = status.map(str::to_string);
    if matches!(
        status,
        Some("completed" | "failed" | "cancelled" | "incomplete")
    ) {
        job.phase = "terminal".to_string();
        job.terminal_at = Some(Utc::now());
        job.usage = body.get("usage").and_then(TokenUsage::from_responses_value);
        job.outcome_rate = job.sampling_policy.matched_outcome_rate(body);
    } else {
        job.phase = "active".to_string();
    }
}

fn settleable(job: &Job) -> bool {
    job.phase == "terminal" || (job.phase == "unresolved" && job.submission_rejected)
}

fn quota_outcome(job: &Job) -> crate::quota::QuotaOutcome {
    if job.submission_rejected {
        crate::quota::QuotaOutcome::Failed
    } else {
        job.usage
            .map_or(crate::quota::QuotaOutcome::Unknown, |usage| {
                crate::quota::QuotaOutcome::Actual(usage.total_tokens)
            })
    }
}

fn usage_record(job: &Job, usage: TokenUsage) -> Result<UsageRecord<'_>, BackendError> {
    Ok(UsageRecord {
        event_id: job.event_id,
        time: job.submitted_at,
        key_hash: &job.key_hash,
        usage_pool_id: job.usage_pool_id.as_deref(),
        connection_id: job.connection_id.as_deref().ok_or_else(unavailable)?,
        provider_kind: &job.provider_kind,
        model: &job.upstream_model,
        prompt_tokens: usage.input_tokens,
        completion_tokens: usage.output_tokens,
        cached_tokens: usage.cached_input_tokens,
        cache_write_tokens: usage.cache_write_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        usage_source: usage.usage_source,
        input_token_attribution: usage.input_token_attribution,
    })
}

/// Environments with background jobs still to poll or settle, in any runtime.
/// Cluster routers load these so no environment's jobs wait for its next request.
pub async fn environments_with_pending_jobs(
    pool: &sqlx::PgPool,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT DISTINCT environment_id FROM chalk_router.background_response_jobs WHERE phase IN ('submitting','active','terminal','unresolved')")
        .fetch_all(pool)
        .await
}

async fn emit_settlement(state: &crate::AppState, job: &Job) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    let tracing_span = tracing::info_span!(parent: None, "background_response", otel.name="responses.background.settle");
    tracing_span.set_parent(opentelemetry::Context::new().with_value(
        crate::telemetry::sampling::TenantSampling {
            environment_id: state.environment_id.clone(),
            gate: state.sampling_gate.clone(),
        },
    ));
    let span = LlmSpanWriter::new(tracing_span, state.object_store.clone(), LlmSpanKind::Llm);
    span.restore_background_context(&job.trace_context);
    span.set_model_system(&job.upstream_model, &job.provider_kind);
    span.set_requested_served_models(&job.requested_model, &job.served_model);
    span.set_background_metrics(
        job.poll_attempts,
        (Utc::now() - job.submitted_at).num_milliseconds(),
    );
    span.set_rate_card_version(&job.rate_card_version);
    if let Some(rate) = job.outcome_rate {
        span.set_sampling_outcome_rate(rate);
    }
    if let Some(usage) = job.usage {
        usage.stamp(&span);
    }
    if job.upstream_status.as_deref() == Some("failed") {
        span.set_error("background_generation_failed", None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn job() -> Job {
        Job {
            submission_rejected: false,
            event_id: Uuid::new_v4(),
            key_hash: "creator".to_string(),
            usage_pool_id: Some("pool".to_string()),
            requested_model: "alias".to_string(),
            served_model: "alias".to_string(),
            connection_id: Some("connection".to_string()),
            provider_kind: "openai".to_string(),
            upstream_model: "gpt-4o".to_string(),
            upstream_response_id: Some("resp_test".to_string()),
            phase: "active".to_string(),
            upstream_status: Some("queued".to_string()),
            usage: None,
            rate_card_version: crate::pricing::current().effective_from.clone(),
            sampling_policy: SamplingPolicy::default(),
            trace_context: TraceContext::default(),
            outcome_rate: None,
            submitted_at: Utc::now(),
            terminal_at: None,
            poll_attempts: 0,
        }
    }

    #[test]
    fn terminal_usage_is_normalized_and_replay_does_not_replace_it() {
        let mut job = job();
        observe_job(
            &mut job,
            &serde_json::json!({"status":"completed","usage":{"input_tokens":11,"input_tokens_details":{"cached_tokens":4},"output_tokens":7,"output_tokens_details":{"reasoning_tokens":3},"total_tokens":18}}),
        );
        let usage = job.usage.unwrap();
        assert_eq!(usage.total_tokens, 18);
        assert_eq!(usage.cached_input_tokens, 4);
        assert_eq!(usage.reasoning_tokens, 3);
        observe_job(&mut job, &serde_json::json!({"status":"queued"}));
        assert_eq!(job.phase, "terminal");
        assert_eq!(job.usage, Some(usage));
    }

    #[test]
    fn missing_or_invalid_terminal_usage_is_not_zero() {
        for usage in [
            Value::Null,
            serde_json::json!({"input_tokens":2,"output_tokens":1,"input_tokens_details":{"cached_tokens":3}}),
        ] {
            let mut job = job();
            observe_job(
                &mut job,
                &serde_json::json!({"status":"cancelled","usage":usage}),
            );
            assert_eq!(job.phase, "terminal");
            assert!(job.usage.is_none());
        }
    }

    #[tokio::test]
    async fn memory_claims_are_exclusive_and_expire() {
        let runtime = BackgroundResponses::new(None, "test".to_string(), PollConfig::new(1, 1, 1));
        let job = job();
        runtime.memory.lock().await.jobs.insert(
            job.event_id,
            MemoryJob {
                job: job.clone(),
                due: Utc::now(),
                claim_until: None,
                settled_at: None,
                last_error_code: None,
            },
        );
        let (first, second) = tokio::join!(runtime.claim(), runtime.claim());
        assert_eq!(first.unwrap().len() + second.unwrap().len(), 1);
        runtime
            .memory
            .lock()
            .await
            .jobs
            .get_mut(&job.event_id)
            .unwrap()
            .claim_until = Some(Utc::now() - chrono::Duration::seconds(1));
        assert_eq!(runtime.claim().await.unwrap().len(), 1);
    }

    #[test]
    #[should_panic(expected = "background polling interval")]
    fn polling_configuration_rejects_zero() {
        PollConfig::new(0, 100, 4);
    }
}
