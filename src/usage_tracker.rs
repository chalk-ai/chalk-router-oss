//! Durable usage recording, with an in-memory counter kept for shutdown
//! diagnostics only.
//!
//! The durable ledger is the `chalk_router.usage_events` table, written once
//! per finished call by [`UsageTracker::record`] (see the type for its
//! retry/idempotency contract). Every read path — including
//! [`UsageTracker::snapshots`], the live budget/quota check — queries that
//! table directly, so a call's usage is invisible to reads until its write
//! commits; there is no in-memory read-side buffer to fill that gap.
//!
//! [`UsageTrackerInner::pending_tokens`] looks like a second source feeding
//! reads, but it isn't: it's an in-memory-only counter of tokens for writes
//! that have started but not yet committed, incremented in
//! [`UsageTracker::record`] and decremented when the call's `InFlightGuard`
//! drops. No read path consults it. Its only consumer is
//! [`UsageTracker::shutdown`], which logs it alongside the in-flight call
//! count if the drain deadline expires — a measure of how much
//! not-yet-written usage a forced shutdown is about to lose, not a value
//! anyone reads back for enforcement.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::api_keys::UsageSnapshot;

const MAX_WRITE_ATTEMPTS: usize = 5;
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(25);
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq, Eq)]
enum AccountingRetry {
    Retry,
    Fallback,
    Stop,
}

// `attempt` counts both combined and standalone statements. A combined attempt
// is retried only after a transient failure and always leaves a standalone
// attempt; any other failure falls back at once. The standalone insert is
// idempotent and keeps the historical contract: it retries every error (a
// canceled statement or a read-only failover can succeed on a fresh
// connection) until the shared budget is spent.
fn accounting_retry(combined: bool, attempt: usize, transient: bool) -> AccountingRetry {
    if combined {
        if transient && attempt + 1 < MAX_WRITE_ATTEMPTS {
            AccountingRetry::Retry
        } else {
            AccountingRetry::Fallback
        }
    } else if attempt < MAX_WRITE_ATTEMPTS {
        AccountingRetry::Retry
    } else {
        AccountingRetry::Stop
    }
}

/// Provenance of the token counts stored for one usage event.
///
/// Discriminants match `chalk.router.v1.UsageSource` and the checked SMALLINT
/// values in `usage_events`; zero/unspecified is never persisted.
#[repr(i16)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UsageSource {
    #[default]
    Reported = 1,
    Estimated = 2,
    LegacyUnknown = 3,
}

impl UsageSource {
    pub const fn as_db_code(self) -> i16 {
        self as i16
    }
}

/// One finalized inference call. The event id is allocated at request
/// acceptance and retained across every database retry.
pub struct UsageRecord<'a> {
    pub event_id: uuid::Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub key_hash: &'a str,
    pub usage_pool_id: Option<&'a str>,
    pub connection_id: &'a str,
    /// The upstream kind that served the call. Half of the rate-card identity:
    /// the same model costs different amounts through Bedrock and Anthropic.
    pub provider_kind: &'a str,
    pub model: &'a str,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Cache reads are a subset of `prompt_tokens`, not additional tokens.
    pub cached_tokens: u64,
    /// Cache writes are a subset of `prompt_tokens`, not additional tokens.
    pub cache_write_tokens: u64,
    /// Reasoning tokens are a subset of `completion_tokens`.
    pub reasoning_tokens: u64,
    pub usage_source: UsageSource,
    pub input_token_attribution: crate::pricing::InputTokenAttribution,
}

#[derive(Debug)]
pub enum UsageWriteError {
    Invalid(&'static str),
    Database(sqlx::Error),
}

impl std::fmt::Display for UsageWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::Database(_) => formatter.write_str("usage accounting is temporarily unavailable"),
        }
    }
}

impl std::error::Error for UsageWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invalid(_) => None,
            Self::Database(error) => Some(error),
        }
    }
}

/// Validated once, including the rate card in force at terminal accounting.
struct PreparedUsageRecord<'a> {
    record: UsageRecord<'a>,
    counts: [i64; 6],
    estimated_cost_usd: Option<f64>,
}

impl<'a> PreparedUsageRecord<'a> {
    fn new(rec: UsageRecord<'a>) -> Result<Self, UsageWriteError> {
        if rec.cached_tokens > rec.prompt_tokens {
            return Err(UsageWriteError::Invalid(
                "cached tokens exceed prompt tokens",
            ));
        }
        if rec.cache_write_tokens > rec.prompt_tokens {
            return Err(UsageWriteError::Invalid(
                "cache-write tokens exceed prompt tokens",
            ));
        }
        if rec.reasoning_tokens > rec.completion_tokens {
            return Err(UsageWriteError::Invalid(
                "reasoning tokens exceed completion tokens",
            ));
        }

        let prompt_tokens = checked_i64(rec.prompt_tokens)?;
        let completion_tokens = checked_i64(rec.completion_tokens)?;
        let cached_tokens = checked_i64(rec.cached_tokens)?;
        let cache_write_tokens = checked_i64(rec.cache_write_tokens)?;
        let reasoning_tokens = checked_i64(rec.reasoning_tokens)?;
        let total_tokens = rec
            .prompt_tokens
            .checked_add(rec.completion_tokens)
            .ok_or(UsageWriteError::Invalid("derived token total overflowed"))?;
        let total_tokens = checked_i64(total_tokens)?;
        // Priced here, against the card in force now, so the stored figure is
        // what the call cost when it happened. Re-pricing at read time would
        // restate history every time a vendor moves a number, and could not
        // price a row grouped by anything but model at all.
        let estimated_cost_usd = crate::pricing::cost_usd(
            rec.provider_kind,
            rec.model,
            &crate::pricing::TokenUsage {
                prompt: rec.prompt_tokens,
                completion: rec.completion_tokens,
                cache_read: rec.cached_tokens,
                cache_write: rec.cache_write_tokens,
                input_token_attribution: rec.input_token_attribution,
            },
        )
        .map(|cost| cost.total_usd)
        // A non-finite product of a corrupt rate would poison every aggregate
        // that sums it, and NULL already means "unpriced".
        .filter(|cost| cost.is_finite() && *cost >= 0.0);
        if estimated_cost_usd.is_none() {
            tracing::debug!(
                provider_kind = %rec.provider_kind,
                model = %rec.model,
                "model rates or token billing attribution unavailable; usage event stored unpriced"
            );
        }

        Ok(Self {
            record: rec,
            counts: [
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                cache_write_tokens,
                reasoning_tokens,
                total_tokens,
            ],
            estimated_cost_usd,
        })
    }
}

fn checked_i64(value: u64) -> Result<i64, UsageWriteError> {
    i64::try_from(value).map_err(|_| UsageWriteError::Invalid("token count exceeds BIGINT"))
}

fn usage_snapshots_query() -> String {
    "SELECT api_key_hash, \
            CAST(SUM(prompt_tokens) AS BIGINT) AS prompt_tokens, \
            CAST(SUM(completion_tokens) AS BIGINT) AS completion_tokens, \
            CAST(SUM(total_tokens) AS BIGINT) AS total_tokens, \
            CAST(SUM(request_count) AS BIGINT) AS request_count \
         FROM chalk_router.usage_events \
         WHERE environment_id = $1 AND api_key_hash = ANY($2) \
         GROUP BY api_key_hash"
        .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageDimension {
    None,
    Connection,
    Model,
    ApiKey,
    UsagePool,
}

#[derive(Debug, Default, Clone)]
pub struct UsageFilters {
    pub api_key_hashes: Vec<String>,
    pub usage_pool_ids: Vec<String>,
    pub connection_ids: Vec<String>,
    pub models: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct UsageQuery {
    pub start: chrono::DateTime<chrono::Utc>,
    pub end: chrono::DateTime<chrono::Utc>,
    pub bucket_seconds: i64,
    pub group_by: UsageDimension,
    pub filters: UsageFilters,
    pub row_limit: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageAggregateRow {
    pub bucket: chrono::DateTime<chrono::Utc>,
    pub dimension_value: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
    pub request_count: i64,
    /// Summed over the calls in this row that were priced. `None` when none of
    /// them were, which is what a router that priced nothing looks like — and
    /// is not the same as a priced zero.
    pub estimated_cost_usd: Option<f64>,
    /// Calls in this row missing from `estimated_cost_usd`, so a partial
    /// estimate cannot be read as a complete one.
    pub unpriced_request_count: i64,
    pub(crate) connection_id: Option<String>,
}

#[derive(Debug)]
pub enum UsageQueryError {
    Database(sqlx::Error),
    CounterOverflow,
}

impl std::fmt::Display for UsageQueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "usage query failed: {error}"),
            Self::CounterOverflow => formatter.write_str("usage aggregate exceeds int64"),
        }
    }
}

impl std::error::Error for UsageQueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::CounterOverflow => None,
        }
    }
}

impl From<sqlx::Error> for UsageQueryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(sqlx::FromRow)]
struct RawUsageAggregateRow {
    bucket: chrono::DateTime<chrono::Utc>,
    dimension_value: String,
    prompt_tokens: String,
    completion_tokens: String,
    cached_tokens: String,
    cache_write_tokens: String,
    reasoning_tokens: String,
    total_tokens: String,
    request_count: String,
    estimated_cost_usd: Option<f64>,
    unpriced_request_count: String,
    connection_id: Option<String>,
}

fn aggregate_i64(value: &str) -> Result<i64, UsageQueryError> {
    value
        .parse::<u128>()
        .ok()
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(UsageQueryError::CounterOverflow)
}

impl TryFrom<RawUsageAggregateRow> for UsageAggregateRow {
    type Error = UsageQueryError;

    fn try_from(row: RawUsageAggregateRow) -> Result<Self, Self::Error> {
        Ok(Self {
            bucket: row.bucket,
            dimension_value: row.dimension_value,
            prompt_tokens: aggregate_i64(&row.prompt_tokens)?,
            completion_tokens: aggregate_i64(&row.completion_tokens)?,
            cached_tokens: aggregate_i64(&row.cached_tokens)?,
            cache_write_tokens: aggregate_i64(&row.cache_write_tokens)?,
            reasoning_tokens: aggregate_i64(&row.reasoning_tokens)?,
            total_tokens: aggregate_i64(&row.total_tokens)?,
            request_count: aggregate_i64(&row.request_count)?,
            estimated_cost_usd: row.estimated_cost_usd,
            unpriced_request_count: aggregate_i64(&row.unpriced_request_count)?,
            connection_id: row.connection_id,
        })
    }
}

/// Durable plain-PostgreSQL usage ledger. Each call is inserted independently;
/// no process-local aggregate is an accounting boundary. Callers await
/// [`record`](Self::record) before releasing a successful terminal response.
#[derive(Clone)]
pub struct UsageTracker {
    inner: Arc<UsageTrackerInner>,
}

struct UsageTrackerInner {
    pool: sqlx::PgPool,
    environment_id: String,
    in_flight: AtomicUsize,
    pending_tokens: AtomicU64,
    idle: tokio::sync::Notify,
}

struct InFlightGuard {
    inner: Arc<UsageTrackerInner>,
    tokens: u64,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.inner.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.inner
            .pending_tokens
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                Some(pending.saturating_sub(self.tokens))
            })
            .ok();
        self.inner.idle.notify_waiters();
    }
}

impl UsageTracker {
    pub async fn new(pool: sqlx::PgPool, environment_id: String) -> Result<Self, sqlx::Error> {
        Ok(Self {
            inner: Arc::new(UsageTrackerInner {
                pool,
                environment_id,
                in_flight: AtomicUsize::new(0),
                pending_tokens: AtomicU64::new(0),
                idle: tokio::sync::Notify::new(),
            }),
        })
    }

    /// Insert one call durably. The idempotency key makes both explicit retries
    /// and ambiguous connection failures safe: a committed first attempt turns
    /// every later attempt into a successful no-op.
    pub async fn record(&self, rec: UsageRecord<'_>) -> Result<(), UsageWriteError> {
        self.record_with_quota(rec, None, None).await
    }

    /// Keep permits owned and renewed until the combined commit is acknowledged.
    /// Deleted reservation/lease receipts make replays safe even when an earlier
    /// commit acknowledgement was lost or a standalone usage row already exists.
    pub async fn record_with_quota(
        &self,
        rec: UsageRecord<'_>,
        mut permit: Option<crate::quota::QuotaPermit>,
        actual_amount: Option<u64>,
    ) -> Result<(), UsageWriteError> {
        let prepared = match PreparedUsageRecord::new(rec) {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(permit) = permit {
                    permit.finish(actual_amount).await;
                }
                return Err(error);
            }
        };
        let rec = &prepared.record;
        let total_tokens = prepared.counts[5];
        self.inner.in_flight.fetch_add(1, Ordering::AcqRel);
        self.inner
            .pending_tokens
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                Some(pending.saturating_add(total_tokens as u64))
            })
            .ok();
        let _in_flight = InFlightGuard {
            inner: self.inner.clone(),
            tokens: total_tokens as u64,
        };

        let can_combine = permit.as_ref().is_some_and(|permit| {
            permit.can_account_with(&self.inner.pool, &self.inner.environment_id)
        });
        // The second phase preserves the historical contract: quota errors are
        // logged, but durable usage is still attempted before returning success.
        let mut attempt = 0;
        let mut delay = INITIAL_RETRY_DELAY;
        for combined in [true, false] {
            if combined && !can_combine {
                continue;
            }
            if !combined {
                if let Some(permit) = permit.take() {
                    permit.finish(actual_amount).await;
                }
            }
            loop {
                attempt += 1;
                let grants = combined.then(|| {
                    permit
                        .as_ref()
                        .expect("combined permit")
                        .accounting_grants()
                });
                let statement = if combined {
                    "SELECT chalk_router.finish_quota_usage($16,$17,$1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)"
                } else {
                    "INSERT INTO chalk_router.usage_events \
                        (event_id, time, environment_id, api_key_hash, usage_pool_id, connection_id, model, \
                         prompt_tokens, completion_tokens, cached_tokens, cache_write_tokens, reasoning_tokens, \
                         total_tokens, request_count, usage_source, estimated_cost_usd) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,1,$14,$15) \
                     ON CONFLICT (environment_id, event_id) DO NOTHING"
                };
                let mut query = sqlx::query(statement)
                    .bind(rec.event_id)
                    .bind(rec.time)
                    .bind(&self.inner.environment_id)
                    .bind(rec.key_hash)
                    .bind(rec.usage_pool_id)
                    .bind(rec.connection_id)
                    .bind(rec.model)
                    .bind(prepared.counts[0])
                    .bind(prepared.counts[1])
                    .bind(prepared.counts[2])
                    .bind(prepared.counts[3])
                    .bind(prepared.counts[4])
                    .bind(prepared.counts[5])
                    .bind(rec.usage_source.as_db_code())
                    .bind(prepared.estimated_cost_usd);
                if let Some(ref grants) = grants {
                    query = query.bind(sqlx::types::Json(grants)).bind(
                        actual_amount.map(|amount| i64::try_from(amount).unwrap_or(i64::MAX)),
                    );
                }
                let result = query.execute(&self.inner.pool).await;
                drop(grants);
                match result {
                    Ok(_) => {
                        if let Some(permit) = permit.take() {
                            permit.accounting_committed();
                        }
                        return Ok(());
                    }
                    Err(error) => {
                        let transient = crate::db_error::is_transient(&error);
                        let retry = accounting_retry(combined, attempt, transient);
                        match retry {
                            AccountingRetry::Stop => {
                                tracing::error!(event_id = %rec.event_id, connection_id = %rec.connection_id,
                                    attempts = attempt, error = %error, "Durable usage write failed");
                                return Err(UsageWriteError::Database(error));
                            }
                            AccountingRetry::Fallback => {
                                tracing::error!(event_id = %rec.event_id, attempt, error = %error,
                                    "Combined quota accounting failed; attempting separate settlement and usage");
                            }
                            AccountingRetry::Retry => {
                                tracing::warn!(event_id = %rec.event_id, connection_id = %rec.connection_id,
                                    attempt, combined, error = %error, "Retrying durable terminal accounting");
                            }
                        }
                        // A permanent combined failure falls back without
                        // waiting; every retry backs off.
                        if transient || retry == AccountingRetry::Retry {
                            tokio::time::sleep(delay).await;
                            delay = delay.saturating_mul(2);
                        }
                        if retry == AccountingRetry::Fallback {
                            break;
                        }
                    }
                }
            }
        }
        unreachable!("standalone usage phase always returns")
    }

    /// Wait for any disconnect-triggered writes after Axum has drained active
    /// requests. Ordinary terminal writes are already awaited by their handler.
    pub async fn shutdown(&self) {
        // Detached futures are spawned synchronously from stream Drop. Once
        // Axum has drained requests, one scheduler turn lets those already
        // queued tasks enter `record` and register below before the zero check.
        tokio::task::yield_now().await;
        let deadline = tokio::time::Instant::now() + SHUTDOWN_DRAIN_TIMEOUT;
        loop {
            if self.inner.in_flight.load(Ordering::Acquire) == 0 {
                return;
            }
            let notified = self.inner.idle.notified();
            if self.inner.in_flight.load(Ordering::Acquire) == 0 {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                tracing::error!(
                    pending_calls = self.inner.in_flight.load(Ordering::Acquire),
                    pending_tokens = self.inner.pending_tokens.load(Ordering::Acquire),
                    "Usage shutdown deadline expired"
                );
                return;
            }
        }
    }

    pub(crate) async fn record_at_version(
        &self,
        rec: UsageRecord<'_>,
        version: &str,
    ) -> Result<(), UsageWriteError> {
        let mut transaction = self
            .inner
            .pool
            .begin()
            .await
            .map_err(UsageWriteError::Database)?;
        self.record_in_transaction(&mut transaction, rec, version)
            .await?;
        transaction
            .commit()
            .await
            .map_err(UsageWriteError::Database)?;
        Ok(())
    }

    pub(crate) async fn record_in_transaction(
        &self,
        connection: &mut sqlx::PgConnection,
        rec: UsageRecord<'_>,
        version: &str,
    ) -> Result<(), UsageWriteError> {
        if rec.cached_tokens > rec.prompt_tokens
            || rec.cache_write_tokens > rec.prompt_tokens
            || rec.reasoning_tokens > rec.completion_tokens
        {
            return Err(UsageWriteError::Invalid("invalid background token details"));
        }
        let total = rec
            .prompt_tokens
            .checked_add(rec.completion_tokens)
            .ok_or(UsageWriteError::Invalid("derived token total overflowed"))?;
        let cost = crate::pricing::card_effective_at(version)
            .filter(|card| card.effective_from == version)
            .and_then(|card| {
                crate::pricing::cost_usd_with(
                    card,
                    rec.provider_kind,
                    rec.model,
                    &crate::pricing::TokenUsage {
                        prompt: rec.prompt_tokens,
                        completion: rec.completion_tokens,
                        cache_read: rec.cached_tokens,
                        cache_write: rec.cache_write_tokens,
                        input_token_attribution: rec.input_token_attribution,
                    },
                )
            })
            .map(|cost| cost.total_usd)
            .filter(|cost| cost.is_finite() && *cost >= 0.0);
        sqlx::query("INSERT INTO chalk_router.usage_events (event_id,time,environment_id,api_key_hash,usage_pool_id,connection_id,model,prompt_tokens,completion_tokens,cached_tokens,cache_write_tokens,reasoning_tokens,total_tokens,request_count,usage_source,estimated_cost_usd) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,1,$14,$15) ON CONFLICT (environment_id,event_id) DO NOTHING")
            .bind(rec.event_id).bind(rec.time).bind(&self.inner.environment_id).bind(rec.key_hash).bind(rec.usage_pool_id).bind(rec.connection_id).bind(rec.model)
            .bind(checked_i64(rec.prompt_tokens)?).bind(checked_i64(rec.completion_tokens)?).bind(checked_i64(rec.cached_tokens)?)
            .bind(checked_i64(rec.cache_write_tokens)?).bind(checked_i64(rec.reasoning_tokens)?).bind(checked_i64(total)?)
            .bind(rec.usage_source.as_db_code()).bind(cost).execute(connection).await.map_err(UsageWriteError::Database)?;
        Ok(())
    }

    pub async fn snapshots(
        &self,
        key_hashes: &[String],
    ) -> Result<HashMap<String, UsageSnapshot>, sqlx::Error> {
        if key_hashes.is_empty() {
            return Ok(HashMap::new());
        }
        let rows =
            sqlx::query_as::<_, UsageSnapshotRow>(sqlx::AssertSqlSafe(usage_snapshots_query()))
                .bind(&self.inner.environment_id)
                .bind(key_hashes)
                .fetch_all(&self.inner.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.api_key_hash,
                    UsageSnapshot {
                        prompt_tokens: row.prompt_tokens.max(0) as u64,
                        completion_tokens: row.completion_tokens.max(0) as u64,
                        total_tokens: row.total_tokens.max(0) as u64,
                        request_count: row.request_count.max(0) as u64,
                    },
                )
            })
            .collect())
    }

    /// Query bounded call-ledger aggregates. Every dynamic SQL fragment is
    /// selected from a closed enum; all caller data is bound.
    pub async fn query(
        &self,
        query: &UsageQuery,
    ) -> Result<Vec<UsageAggregateRow>, UsageQueryError> {
        let mut sql = sqlx::QueryBuilder::<sqlx::Postgres>::new("SELECT ");
        sql.push(format!(
            "to_timestamp(floor(EXTRACT(EPOCH FROM time) / {seconds}) * {seconds}) AS bucket, ",
            seconds = query.bucket_seconds,
        ));
        match query.group_by {
            UsageDimension::None => {
                sql.push("''::TEXT");
            }
            UsageDimension::Connection => {
                sql.push("COALESCE(connection_id, '')");
            }
            UsageDimension::Model => {
                sql.push("model");
            }
            UsageDimension::ApiKey => {
                sql.push("api_key_hash");
            }
            UsageDimension::UsagePool => {
                sql.push("COALESCE(usage_pool_id, '')");
            }
        };
        sql.push(
            " AS dimension_value, \
             SUM(prompt_tokens)::TEXT AS prompt_tokens, \
             SUM(completion_tokens)::TEXT AS completion_tokens, \
             SUM(cached_tokens)::TEXT AS cached_tokens, \
             SUM(cache_write_tokens)::TEXT AS cache_write_tokens, \
             SUM(reasoning_tokens)::TEXT AS reasoning_tokens, \
             SUM(total_tokens)::TEXT AS total_tokens, \
             SUM(request_count)::TEXT AS request_count, \
             SUM(estimated_cost_usd) AS estimated_cost_usd, \
             COALESCE(SUM(request_count) \
                 FILTER (WHERE estimated_cost_usd IS NULL), 0)::TEXT \
                 AS unpriced_request_count, \
             MIN(connection_id) AS connection_id \
             FROM chalk_router.usage_events WHERE environment_id = ",
        );
        sql.push_bind(&self.inner.environment_id);
        sql.push(" AND time >= ");
        sql.push_bind(query.start);
        sql.push(" AND time < ");
        sql.push_bind(query.end);

        push_array_filter(&mut sql, "api_key_hash", &query.filters.api_key_hashes);
        push_array_filter(&mut sql, "usage_pool_id", &query.filters.usage_pool_ids);
        push_array_filter(&mut sql, "connection_id", &query.filters.connection_ids);
        push_array_filter(&mut sql, "model", &query.filters.models);
        sql.push(" GROUP BY bucket, dimension_value ORDER BY bucket, dimension_value LIMIT ");
        sql.push_bind(query.row_limit.saturating_add(1));

        let rows = sql
            .build_query_as::<RawUsageAggregateRow>()
            .fetch_all(&self.inner.pool)
            .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Histogram grouped by connection and completion-time bucket.
    pub async fn histogram(
        &self,
        api_token: Option<&str>,
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
        bucket_size: &str,
    ) -> Result<Vec<UsageHistogramRow>, sqlx::Error> {
        let interval_secs: i64 = match bucket_size {
            "1m" => 60,
            "5m" => 300,
            "15m" => 900,
            "1h" => 3600,
            "6h" => 21600,
            "1d" => 86400,
            "7d" => 604800,
            other => {
                tracing::warn!(bucket_size = %other, "Invalid bucket_size, defaulting to 1h");
                3600
            }
        };
        let rows = self
            .query(&UsageQuery {
                start,
                end,
                bucket_seconds: interval_secs,
                group_by: UsageDimension::Connection,
                filters: UsageFilters {
                    api_key_hashes: api_token.into_iter().map(str::to_owned).collect(),
                    ..Default::default()
                },
                row_limit: 10_000,
            })
            .await
            .map_err(|error| match error {
                UsageQueryError::Database(error) => error,
                UsageQueryError::CounterOverflow => sqlx::Error::Protocol(error.to_string()),
            })?;
        Ok(rows
            .into_iter()
            .map(|row| UsageHistogramRow {
                connection_id: row.connection_id,
                bucket: row.bucket,
                prompt_tokens: row.prompt_tokens,
                completion_tokens: row.completion_tokens,
                cached_tokens: row.cached_tokens,
                cache_write_tokens: row.cache_write_tokens,
                reasoning_tokens: row.reasoning_tokens,
                total_tokens: row.total_tokens,
                request_count: row.request_count,
            })
            .collect())
    }
}

fn push_array_filter(
    query: &mut sqlx::QueryBuilder<sqlx::Postgres>,
    column: &'static str,
    values: &[String],
) {
    if !values.is_empty() {
        query.push(" AND ");
        query.push(column);
        query.push(" = ANY(");
        query.push_bind(values);
        query.push(")");
    }
}

#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct UsageHistogramRow {
    pub connection_id: Option<String>,
    pub bucket: chrono::DateTime<chrono::Utc>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
    pub request_count: i64,
}

#[derive(sqlx::FromRow)]
struct UsageSnapshotRow {
    api_key_hash: String,
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    request_count: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounting_retry_shares_budget_and_reserves_standalone_attempt() {
        for attempt in 1..MAX_WRITE_ATTEMPTS - 1 {
            assert_eq!(
                accounting_retry(true, attempt, true),
                AccountingRetry::Retry
            );
        }
        assert_eq!(accounting_retry(true, 4, true), AccountingRetry::Fallback);
        for transient in [true, false] {
            assert_eq!(
                accounting_retry(false, MAX_WRITE_ATTEMPTS, transient),
                AccountingRetry::Stop
            );
        }
        for attempt in 1..MAX_WRITE_ATTEMPTS {
            assert_eq!(
                accounting_retry(true, attempt, false),
                AccountingRetry::Fallback
            );
            // Standalone usage keeps retrying any error within the budget.
            for transient in [true, false] {
                assert_eq!(
                    accounting_retry(false, attempt, transient),
                    AccountingRetry::Retry
                );
            }
        }
    }

    #[test]
    fn checked_token_values_reject_bigint_overflow() {
        assert_eq!(checked_i64(i64::MAX as u64).unwrap(), i64::MAX);
        assert!(checked_i64(i64::MAX as u64 + 1).is_err());
    }

    #[test]
    fn usage_source_uses_protobuf_numbers() {
        assert_eq!(
            UsageSource::Reported.as_db_code(),
            crate::grpc::proto::UsageSource::Reported as i16
        );
        assert_eq!(
            UsageSource::Estimated.as_db_code(),
            crate::grpc::proto::UsageSource::Estimated as i16
        );
        assert_eq!(
            UsageSource::LegacyUnknown.as_db_code(),
            crate::grpc::proto::UsageSource::LegacyUnknown as i16
        );
    }
}
