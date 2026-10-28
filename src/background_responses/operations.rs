use sqlx::{Postgres, Transaction};

use super::*;
use crate::service::{pagination::StablePageCursor, ServiceError};

#[cfg(test)]
mod tests;

const CLEANUP_BATCH_SIZE: usize = 100;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BackgroundResponseEntry {
    pub event_id: Uuid,
    pub phase: String,
    pub connection_id: Option<String>,
    pub upstream_response_id: Option<String>,
    pub requested_model: String,
    pub served_model: String,
    pub upstream_status: Option<String>,
    pub submitted_at: DateTime<Utc>,
    pub terminal_at: Option<DateTime<Utc>>,
    pub settled_at: Option<DateTime<Utc>>,
    pub next_poll_at: DateTime<Utc>,
    pub claim_until: Option<DateTime<Utc>>,
    #[sqlx(try_from = "i64")]
    pub poll_attempts: u64,
    pub last_error_code: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
}

#[derive(Debug)]
pub struct BackgroundResponsePage {
    pub jobs: Vec<BackgroundResponseEntry>,
    pub next_cursor: Option<String>,
}

fn store_error(error: sqlx::Error) -> ServiceError {
    let constraint = crate::db_error::classify(&error);
    tracing::error!(%error, ?constraint, "background response operation failed");
    ServiceError::internal("background response storage unavailable")
}

async fn bounded_transaction(pool: &PgPool) -> Result<Transaction<'_, Postgres>, ServiceError> {
    let mut transaction = pool.begin().await.map_err(store_error)?;
    sqlx::query("SET LOCAL statement_timeout = '2s'")
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?;
    sqlx::query("SET LOCAL lock_timeout = '500ms'")
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?;
    Ok(transaction)
}

fn safe_status(status: Option<String>) -> Option<String> {
    status.map(|status| match status.as_str() {
        "queued" | "in_progress" | "completed" | "failed" | "cancelled" | "incomplete" => status,
        _ => "unknown".to_string(),
    })
}

impl BackgroundResponses {
    pub(crate) async fn list_jobs(
        &self,
        phase: Option<&str>,
        cursor: Option<(DateTime<Utc>, Uuid)>,
        limit: i64,
    ) -> Result<BackgroundResponsePage, ServiceError> {
        tokio::time::timeout(OPERATION_TIMEOUT, async {
            let mut jobs = if let Some(pool) = &self.pool {
                let mut transaction = bounded_transaction(pool).await?;
                let entries = sqlx::query_as::<_, BackgroundResponseEntry>(
                    r#"SELECT event_id, phase, connection_id, upstream_response_id,
                              payload->>'requested_model' AS requested_model,
                              payload->>'served_model' AS served_model,
                              payload->>'upstream_status' AS upstream_status,
                              (payload->>'submitted_at')::timestamptz AS submitted_at,
                              (payload->>'terminal_at')::timestamptz AS terminal_at,
                              settled_at, next_poll_at, claim_until, poll_attempts,
                              last_error_code, created_at
                       FROM chalk_router.background_response_jobs
                       WHERE environment_id=$1
                         AND (($2::text IS NULL AND phase<>'settled') OR phase=$2)
                         AND ($3::timestamptz IS NULL OR (created_at,event_id)<($3,$4::uuid))
                       ORDER BY created_at DESC, event_id DESC
                       LIMIT $5"#,
                )
                .bind(&self.environment_id)
                .bind(phase)
                .bind(cursor.map(|(created_at, _)| created_at))
                .bind(cursor.map(|(_, event_id)| event_id))
                .bind(limit + 1)
                .fetch_all(&mut *transaction)
                .await
                .map_err(store_error)?;
                transaction.commit().await.map_err(store_error)?;
                entries
            } else {
                let memory = self.memory.lock().await;
                let mut selected = std::collections::BTreeMap::new();
                for entry in memory.jobs.values().filter(|entry| {
                    phase.map_or(entry.job.phase != "settled", |phase| {
                        entry.job.phase == phase
                    }) && cursor
                        .is_none_or(|cursor| (entry.job.submitted_at, entry.job.event_id) < cursor)
                }) {
                    selected.insert((entry.job.submitted_at, entry.job.event_id), entry);
                    if selected.len() > limit as usize + 1 {
                        selected.pop_first();
                    }
                }
                selected
                    .into_values()
                    .rev()
                    .map(|entry| BackgroundResponseEntry {
                        event_id: entry.job.event_id,
                        phase: entry.job.phase.clone(),
                        connection_id: entry.job.connection_id.clone(),
                        upstream_response_id: entry.job.upstream_response_id.clone(),
                        requested_model: entry.job.requested_model.clone(),
                        served_model: entry.job.served_model.clone(),
                        upstream_status: entry.job.upstream_status.clone(),
                        submitted_at: entry.job.submitted_at,
                        terminal_at: entry.job.terminal_at,
                        settled_at: entry.settled_at,
                        next_poll_at: entry.due,
                        claim_until: entry.claim_until,
                        poll_attempts: entry.job.poll_attempts,
                        last_error_code: entry.last_error_code.clone(),
                        created_at: entry.job.submitted_at,
                    })
                    .collect()
            };
            let has_more = jobs.len() > limit as usize;
            jobs.truncate(limit as usize);
            for job in &mut jobs {
                job.upstream_status = safe_status(job.upstream_status.take());
                job.last_error_code = job
                    .last_error_code
                    .take()
                    .map(|_| "upstream_or_settlement_unavailable".to_string());
            }
            let next_cursor = if has_more {
                jobs.last().map(|entry| {
                    StablePageCursor {
                        created_at: entry.created_at,
                        id: entry.event_id.to_string(),
                    }
                    .encode()
                })
            } else {
                None
            };
            Ok(BackgroundResponsePage { jobs, next_cursor })
        })
        .await
        .map_err(|_| ServiceError::internal("background response listing timed out"))?
    }

    pub async fn cleanup(&self) -> Result<u64, ServiceError> {
        if self.config.retention_days == 0 {
            return Ok(0);
        }
        let cutoff = Utc::now()
            .checked_sub_signed(chrono::Duration::days(self.config.retention_days.into()))
            .ok_or_else(|| {
                ServiceError::invalid_field("retention_days", "outside supported date range")
            })?;
        tokio::time::timeout(OPERATION_TIMEOUT, async {
            if let Some(pool) = &self.pool {
                let mut transaction = bounded_transaction(pool).await?;
                let deleted = sqlx::query(
                    r#"WITH candidates AS (
                           SELECT job.environment_id, job.event_id
                           FROM chalk_router.background_response_jobs AS job
                           WHERE job.environment_id=$1 AND job.phase='settled'
                             AND job.settled_at<$2
                             AND (job.claim_until IS NULL OR job.claim_until<=now())
                             AND NOT EXISTS (
                                 SELECT 1 FROM chalk_router.quota_reservations
                                 WHERE background_environment_id=job.environment_id
                                   AND background_event_id=job.event_id
                             )
                             AND NOT EXISTS (
                                 SELECT 1 FROM chalk_router.quota_leases
                                 WHERE background_environment_id=job.environment_id
                                   AND background_event_id=job.event_id
                             )
                           ORDER BY job.settled_at, job.event_id
                           LIMIT $3 FOR UPDATE OF job SKIP LOCKED
                       )
                       DELETE FROM chalk_router.background_response_jobs AS job
                       USING candidates
                       WHERE job.environment_id=candidates.environment_id
                         AND job.event_id=candidates.event_id"#,
                )
                .bind(&self.environment_id)
                .bind(cutoff)
                .bind(CLEANUP_BATCH_SIZE as i64)
                .execute(&mut *transaction)
                .await
                .map_err(store_error)?
                .rows_affected();
                transaction.commit().await.map_err(store_error)?;
                Ok(deleted)
            } else {
                let mut memory = self.memory.lock().await;
                let candidates: Vec<_> = memory
                    .settled
                    .iter()
                    .take_while(|(settled_at, _)| *settled_at < cutoff)
                    .take(CLEANUP_BATCH_SIZE)
                    .copied()
                    .collect();
                let mut deleted = 0;
                for (settled_at, event_id) in candidates {
                    if memory.jobs.get(&event_id).is_some_and(|entry| {
                        entry.job.phase == "settled"
                            && entry.settled_at == Some(settled_at)
                            && entry.claim_until.is_none_or(|expiry| expiry <= Utc::now())
                    }) {
                        memory.jobs.remove(&event_id);
                        memory.settled.remove(&(settled_at, event_id));
                        deleted += 1;
                    }
                }
                Ok(deleted)
            }
        })
        .await
        .map_err(|_| ServiceError::internal("background response cleanup timed out"))?
    }
}
