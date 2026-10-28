//! `openai-server migrate`: apply the embedded `migrations/default` migrations
//! and exit. Migrations never run on server boot; deployment runs this command
//! before rolling the router.
//!
//! SQLx 0.9 owns migration ordering, checksums, transactional execution, dirty
//! state, and the qualified history table. The router only supplies the pieces
//! SQLx cannot provide safely through a transaction-pooling proxy:
//!
//! - a renewable lease instead of SQLx's session-scoped PostgreSQL advisory
//!   lock;
//! - one-shot import of exact router history rows accidentally written to
//!   `public` by an older search-path-based runner (logged when it fires); and
//! - deterministic recovery for the existing non-transactional index builds.

use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use sqlx::migrate::{Migrate, Migration, Migrator};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::sync::watch;
use uuid::Uuid;

const MIGRATIONS_TABLE: &str = "chalk_router._sqlx_migrations";
// Application-specific advisory-lock id. PostgreSQL advisory locks are
// database-scoped, so this id must stay unique among every application that
// may share a database with the router; schema qualification makes sharing
// plausible. Allocate a distinct id for any other application's runner rather
// than reusing this one.
const BOOTSTRAP_ADVISORY_LOCK: i64 = 4_207_332_001;
const LEASE_SECONDS: i64 = 60;
const LEASE_HEARTBEAT: Duration = Duration::from_secs(10);
const LEASE_RETRY: Duration = Duration::from_secs(1);
const HEARTBEAT_QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const LEASE_FAILURE_GRACE: Duration = Duration::from_secs(50);

type RunnerResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// What a `migrate` run did, for the operator-facing summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationReport {
    /// Migrations SQLx applied during this run.
    pub applied: usize,
    /// Migrations already recorded as applied before SQLx ran, including any
    /// history rows the compatibility preflight adopted or recovered.
    pub already_applied: usize,
}

static MIGRATOR: LazyLock<Migrator> = LazyLock::new(|| {
    let mut migrator = sqlx::migrate!("migrations/default");
    migrator.create_schema("chalk_router");
    migrator.dangerous_set_table_name(MIGRATIONS_TABLE);
    // PostgreSQL's SQLx lock is session-scoped. A transaction-pooling proxy may
    // return that server session while retaining the lock, so the renewable
    // lease below coordinates healthy runners instead. Lease expiry cannot
    // cancel an in-flight MIGRATOR.run; SQLx's transactional DDL plus its
    // version primary key is the duplicate-apply backstop in that case.
    migrator.set_locking(false);
    migrator
});

#[derive(Debug)]
struct RunnerError(String);

impl fmt::Display for RunnerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for RunnerError {}

fn runner_error(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(RunnerError(message.into()))
}

#[derive(Clone)]
struct Lease {
    owner: Uuid,
    fence: i64,
    lost: Arc<AtomicBool>,
}

struct LeaseHeartbeat {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone, Copy)]
struct RecoverableIndex {
    version: i64,
    name: &'static str,
    canonical_definition: &'static str,
}

const RECOVERABLE_INDEXES: &[RecoverableIndex] = &[
    RecoverableIndex {
        version: 4,
        name: "idx_router_provider_connections_env_created",
        canonical_definition: "create index idx_router_provider_connections_env_created on chalk_router.provider_connections (environment_id, created_at desc, id desc)",
    },
    RecoverableIndex {
        version: 5,
        name: "idx_router_model_routes_env_created",
        canonical_definition: "create index idx_router_model_routes_env_created on chalk_router.model_routes (environment_id, created_at desc, id desc)",
    },
    RecoverableIndex {
        version: 6,
        name: "idx_router_api_keys_page",
        canonical_definition: "create index idx_router_api_keys_page on chalk_router.api_keys (environment_id, created_at desc, key_hash desc)",
    },
    RecoverableIndex {
        version: 15,
        name: "uq_router_usage_events_event",
        canonical_definition: "create unique index uq_router_usage_events_event on chalk_router.usage_events (environment_id, event_id)",
    },
    RecoverableIndex {
        version: 16,
        name: "idx_router_usage_events_connection_time",
        canonical_definition: "create index idx_router_usage_events_connection_time on chalk_router.usage_events (environment_id, connection_id, time) where (connection_id is not null)",
    },
    RecoverableIndex {
        version: 30,
        name: "idx_router_api_keys_key_hash",
        canonical_definition: "create index idx_router_api_keys_key_hash on chalk_router.api_keys (key_hash)",
    },
];

enum IndexStatus {
    Absent,
    Valid,
    Invalid,
    Unexpected(String),
}

/// Apply the embedded migrations to `db_url` and report what the run did.
pub async fn run(db_url: &str) -> RunnerResult<MigrationReport> {
    let connect_options = crate::database::connect_options(db_url)?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(connect_options)
        .await?;

    bootstrap_control_tables(&pool).await?;
    let lease = acquire_lease(&pool).await?;
    let heartbeat = start_heartbeat(pool.clone(), lease.clone());

    let result = migrate_under_lease(&pool, &lease).await;

    let _ = heartbeat.stop.send(true);
    let _ = heartbeat.task.await;
    let _ = release_lease(&pool, &lease).await;

    result
}

async fn bootstrap_control_tables(pool: &PgPool) -> RunnerResult<()> {
    // This lock is transaction-scoped and only serializes creation of the two
    // control tables. The heartbeat-backed lease coordinates healthy runners
    // across the full multi-transaction run.
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .persistent(false)
        .bind(BOOTSTRAP_ADVISORY_LOCK)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("CREATE SCHEMA IF NOT EXISTS chalk_router")
        .persistent(false)
        .execute(&mut *transaction)
        .await?;
    // The compatibility preflight needs the history table before MIGRATOR.run
    // validates it. Delegate the definition to SQLx so it cannot drift from
    // the shape SQLx expects: a hand-written IF NOT EXISTS table would
    // silently shadow a newer SQLx definition.
    (*transaction)
        .ensure_migrations_table(MIGRATIONS_TABLE)
        .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS chalk_router._router_migration_lease (\
            singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),\
            owner UUID,\
            fence BIGINT NOT NULL DEFAULT 0,\
            expires_at TIMESTAMPTZ NOT NULL DEFAULT '-infinity'\
        )",
    )
    .persistent(false)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO chalk_router._router_migration_lease (singleton) VALUES (TRUE) \
         ON CONFLICT (singleton) DO NOTHING",
    )
    .persistent(false)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

async fn acquire_lease(pool: &PgPool) -> RunnerResult<Lease> {
    let owner = Uuid::new_v4();
    let mut last_wait_log = Instant::now();

    loop {
        let fence = sqlx::query_scalar::<_, i64>(
            "UPDATE chalk_router._router_migration_lease \
             SET owner = $1, fence = fence + 1, \
                 expires_at = clock_timestamp() + $2::bigint * INTERVAL '1 second' \
             WHERE singleton = TRUE \
               AND (owner IS NULL OR expires_at <= clock_timestamp()) \
             RETURNING fence",
        )
        .persistent(false)
        .bind(owner)
        .bind(LEASE_SECONDS)
        .fetch_optional(pool)
        .await?;

        if let Some(fence) = fence {
            return Ok(Lease {
                owner,
                fence,
                lost: Arc::new(AtomicBool::new(false)),
            });
        }
        if last_wait_log.elapsed() >= Duration::from_secs(30) {
            tracing::info!("waiting for the active router migration runner");
            last_wait_log = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn start_heartbeat(pool: PgPool, lease: Lease) -> LeaseHeartbeat {
    let (stop, mut stop_receiver) = watch::channel(false);
    let heartbeat_lease = lease.clone();
    let task = tokio::spawn(async move {
        let mut delay = LEASE_HEARTBEAT;
        let mut last_confirmed = Instant::now();
        loop {
            tokio::select! {
                changed = stop_receiver.changed() => {
                    if changed.is_err() || *stop_receiver.borrow() {
                        break;
                    }
                }
                _ = tokio::time::sleep(delay) => {
                    let update = sqlx::query(
                        "UPDATE chalk_router._router_migration_lease \
                         SET expires_at = clock_timestamp() + $3::bigint * INTERVAL '1 second' \
                         WHERE singleton = TRUE AND owner = $1 AND fence = $2 \
                           AND expires_at > clock_timestamp()",
                    )
                    .persistent(false)
                    .bind(heartbeat_lease.owner)
                    .bind(heartbeat_lease.fence)
                    .bind(LEASE_SECONDS)
                    .execute(&pool);

                    match tokio::time::timeout(HEARTBEAT_QUERY_TIMEOUT, update).await {
                        Ok(Ok(done)) if done.rows_affected() == 1 => {
                            last_confirmed = Instant::now();
                            delay = LEASE_HEARTBEAT;
                        }
                        Ok(Ok(_)) => {
                            heartbeat_lease.lost.store(true, Ordering::Release);
                            break;
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(%error, "router migration lease heartbeat failed; retrying");
                            if heartbeat_failure_exhausted(last_confirmed.elapsed()) {
                                heartbeat_lease.lost.store(true, Ordering::Release);
                                break;
                            }
                            delay = LEASE_RETRY;
                        }
                        Err(_) => {
                            tracing::warn!("router migration lease heartbeat timed out; retrying");
                            if heartbeat_failure_exhausted(last_confirmed.elapsed()) {
                                heartbeat_lease.lost.store(true, Ordering::Release);
                                break;
                            }
                            delay = LEASE_RETRY;
                        }
                    }
                }
            }
        }
    });
    LeaseHeartbeat { stop, task }
}

fn heartbeat_failure_exhausted(elapsed_since_confirmation: Duration) -> bool {
    elapsed_since_confirmation >= LEASE_FAILURE_GRACE
}

async fn release_lease(pool: &PgPool, lease: &Lease) -> RunnerResult<()> {
    sqlx::query(
        "UPDATE chalk_router._router_migration_lease \
         SET owner = NULL, expires_at = '-infinity' \
         WHERE singleton = TRUE AND owner = $1 AND fence = $2",
    )
    .persistent(false)
    .bind(lease.owner)
    .bind(lease.fence)
    .execute(pool)
    .await?;
    Ok(())
}

async fn assert_lease(pool: &PgPool, lease: &Lease) -> RunnerResult<()> {
    if lease.lost.load(Ordering::Acquire) {
        return Err(runner_error("router migration lease was lost"));
    }
    let owned = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (\
            SELECT 1 FROM chalk_router._router_migration_lease \
            WHERE singleton = TRUE AND owner = $1 AND fence = $2 \
              AND expires_at > clock_timestamp()\
        )",
    )
    .persistent(false)
    .bind(lease.owner)
    .bind(lease.fence)
    .fetch_one(pool)
    .await?;
    if !owned {
        return Err(runner_error("router migration lease was lost"));
    }
    Ok(())
}

async fn migrate_under_lease(pool: &PgPool, lease: &Lease) -> RunnerResult<MigrationReport> {
    assert_lease(pool, lease).await?;
    import_public_history(pool, lease).await?;
    ensure_no_invalid_indexes(pool).await?;
    recover_completed_no_transaction_migrations(pool, lease).await?;
    assert_lease(pool, lease).await?;

    let already_applied = count_applied(pool).await?;
    // The lease is checked around this call, but cannot hard-fence SQLx while a
    // migration is in flight. If the lease expires mid-migration, SQLx still
    // commits transactional DDL and its version row atomically; a concurrent
    // runner's conflicting history insert rolls that runner's transaction back.
    MIGRATOR.run(pool).await?;

    assert_lease(pool, lease).await?;
    ensure_no_invalid_indexes(pool).await?;
    let applied = count_applied(pool).await?.saturating_sub(already_applied);
    Ok(MigrationReport {
        applied,
        already_applied,
    })
}

async fn count_applied(pool: &PgPool) -> RunnerResult<usize> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM chalk_router._sqlx_migrations WHERE success",
    )
    .persistent(false)
    .fetch_one(pool)
    .await?;
    Ok(usize::try_from(count).unwrap_or(0))
}

/// One-shot rollout shim. The retired search-path runner could leave exact
/// router history rows in `public._sqlx_migrations`; copy any still missing
/// from the qualified table.
///
/// Retirement precondition: delete this function and its call once production
/// has completed one `migrate` run under this runner with the adoption log
/// below silent. Landed 2026-08-19 (#101).
async fn import_public_history(pool: &PgPool, lease: &Lease) -> RunnerResult<()> {
    let columns = sqlx::query_as::<_, (String, String)>(
        "SELECT column_name, udt_name \
         FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = '_sqlx_migrations'",
    )
    .persistent(false)
    .fetch_all(pool)
    .await?;
    let expected_columns = [
        ("version", "int8"),
        ("description", "text"),
        ("success", "bool"),
        ("checksum", "bytea"),
        ("execution_time", "int8"),
    ];
    if !expected_columns.iter().all(|expected| {
        columns
            .iter()
            .any(|actual| actual.0 == expected.0 && actual.1 == expected.1)
    }) {
        return Ok(());
    }

    let rows = sqlx::query_as::<_, (i64, bool, Vec<u8>, i64)>(
        "SELECT version, success, checksum, execution_time \
         FROM public._sqlx_migrations ORDER BY version",
    )
    .persistent(false)
    .fetch_all(pool)
    .await?;

    for (version, success, checksum, execution_time) in rows {
        let Some(migration) = migration(version) else {
            continue;
        };
        if checksum != migration.checksum.as_ref() {
            // A shared public SQLx table may contain another application's row
            // with the same version. Adopt rows only when the checksum proves
            // they came from this router's embedded manifest.
            continue;
        }
        if !success {
            return Err(runner_error(format!(
                "public._sqlx_migrations contains failed router migration {version}"
            )));
        }

        assert_lease(pool, lease).await?;
        let adopted = sqlx::query(
            "INSERT INTO chalk_router._sqlx_migrations \
             (version, description, success, checksum, execution_time) \
             SELECT $1, $2, TRUE, $3, $4 \
             WHERE EXISTS (\
                 SELECT 1 FROM chalk_router._router_migration_lease \
                 WHERE singleton = TRUE AND owner = $5 AND fence = $6 \
                   AND expires_at > clock_timestamp()\
             ) \
             ON CONFLICT (version) DO NOTHING",
        )
        .persistent(false)
        .bind(migration.version)
        .bind(migration.description.as_ref())
        .bind(migration.checksum.as_ref())
        .bind(execution_time)
        .bind(lease.owner)
        .bind(lease.fence)
        .execute(pool)
        .await?;
        if adopted.rows_affected() == 1 {
            tracing::info!(
                version,
                "adopted router migration history row from public._sqlx_migrations"
            );
        }
    }
    Ok(())
}

async fn recover_completed_no_transaction_migrations(
    pool: &PgPool,
    lease: &Lease,
) -> RunnerResult<()> {
    for spec in RECOVERABLE_INDEXES {
        let migration = migration(spec.version).ok_or_else(|| {
            runner_error(format!(
                "recovery metadata references unknown migration {}",
                spec.version
            ))
        })?;
        let applied = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (\
                SELECT 1 FROM chalk_router._sqlx_migrations WHERE version = $1\
            )",
        )
        .persistent(false)
        .bind(spec.version)
        .fetch_one(pool)
        .await?;
        if applied {
            continue;
        }

        match inspect_index(pool, spec).await? {
            IndexStatus::Absent => continue,
            IndexStatus::Invalid => {
                return Err(invalid_index_error(spec.name));
            }
            IndexStatus::Unexpected(definition) => {
                return Err(runner_error(format!(
                    "migration {} found chalk_router.{} with an unexpected definition: {definition}",
                    spec.version, spec.name
                )));
            }
            IndexStatus::Valid => {}
        }

        assert_lease(pool, lease).await?;
        let inserted = sqlx::query(
            "INSERT INTO chalk_router._sqlx_migrations \
             (version, description, success, checksum, execution_time) \
             SELECT $1, $2, TRUE, $3, -1 \
             WHERE EXISTS (\
                 SELECT 1 FROM chalk_router._router_migration_lease \
                 WHERE singleton = TRUE AND owner = $4 AND fence = $5 \
                   AND expires_at > clock_timestamp()\
             ) \
             ON CONFLICT (version) DO NOTHING",
        )
        .persistent(false)
        .bind(migration.version)
        .bind(migration.description.as_ref())
        .bind(migration.checksum.as_ref())
        .bind(lease.owner)
        .bind(lease.fence)
        .execute(pool)
        .await?;
        if inserted.rows_affected() != 1 {
            assert_lease(pool, lease).await?;
        }
    }
    Ok(())
}

fn migration(version: i64) -> Option<&'static Migration> {
    MIGRATOR.iter().find(|migration| {
        migration.version == version && migration.migration_type.is_up_migration()
    })
}

async fn inspect_index(pool: &PgPool, spec: &RecoverableIndex) -> RunnerResult<IndexStatus> {
    // Force deparsed relation names to be schema-qualified regardless of any
    // session state left on the PgBouncer server connection.
    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL search_path TO pg_catalog")
        .persistent(false)
        .execute(&mut *transaction)
        .await?;
    let row = sqlx::query_as::<_, (bool, String)>(
        "SELECT i.indisvalid, pg_get_indexdef(i.indexrelid) \
         FROM pg_catalog.pg_index i \
         JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'chalk_router' AND c.relname = $1",
    )
    .persistent(false)
    .bind(spec.name)
    .fetch_optional(&mut *transaction)
    .await?;
    transaction.commit().await?;

    match row {
        None => Ok(IndexStatus::Absent),
        Some((false, _)) => Ok(IndexStatus::Invalid),
        Some((true, definition))
            if canonical_index_definition(&definition) == spec.canonical_definition =>
        {
            Ok(IndexStatus::Valid)
        }
        Some((true, definition)) => Ok(IndexStatus::Unexpected(definition)),
    }
}

fn canonical_index_definition(definition: &str) -> String {
    definition
        .replace(['"', ';'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
        .replace("create unique index concurrently ", "create unique index ")
        .replace("create index concurrently ", "create index ")
        .replace(" using btree ", " ")
}

async fn ensure_no_invalid_indexes(pool: &PgPool) -> RunnerResult<()> {
    let invalid: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname FROM pg_catalog.pg_index i \
         JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'chalk_router' AND NOT i.indisvalid \
         ORDER BY c.relname",
    )
    .persistent(false)
    .fetch_all(pool)
    .await?;
    if !invalid.is_empty() {
        return Err(invalid_index_error(&invalid.join(", ")));
    }
    Ok(())
}

fn invalid_index_error(names: &str) -> Box<dyn Error + Send + Sync> {
    runner_error(format!(
        "invalid index(es) in chalk_router: {names}. Run REINDEX INDEX CONCURRENTLY on each, then redeploy."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_non_transactional_migration_has_matching_single_index_recovery_metadata() {
        let embedded = MIGRATOR
            .iter()
            .filter(|migration| migration.no_tx)
            .collect::<Vec<_>>();
        let recoverable = RECOVERABLE_INDEXES.iter().collect::<Vec<_>>();

        assert_eq!(embedded.len(), recoverable.len());
        for (migration, spec) in embedded.into_iter().zip(recoverable) {
            assert_eq!(migration.version, spec.version);

            // Recovery can mark the migration complete when this index already
            // exists, so keep the recoverable migration deliberately narrow:
            // exactly one executable statement, building exactly this index.
            let statements = migration
                .sql
                .as_str()
                .lines()
                .filter(|line| !line.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n")
                .split(';')
                .map(str::trim)
                .filter(|statement| !statement.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            assert_eq!(
                statements.len(),
                1,
                "recoverable no-transaction migration {} must contain exactly one executable statement",
                migration.version
            );

            let statement = &statements[0];
            let normalized = statement
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase();
            let expected_prefix = if spec
                .canonical_definition
                .starts_with("create unique index ")
            {
                format!("create unique index concurrently {} ", spec.name)
            } else {
                format!("create index concurrently {} ", spec.name)
            };
            assert!(
                normalized.starts_with(&expected_prefix),
                "recoverable no-transaction migration {} must create index {} concurrently",
                migration.version,
                spec.name
            );
            assert_eq!(
                canonical_index_definition(statement),
                spec.canonical_definition,
                "recovery metadata for migration {} must match its index DDL",
                migration.version
            );
        }
    }

    #[test]
    fn canonicalizes_postgres_index_definitions() {
        assert_eq!(
            canonical_index_definition(
                "CREATE INDEX idx ON chalk_router.widgets USING btree (environment_id, created_at DESC)"
            ),
            "create index idx on chalk_router.widgets (environment_id, created_at desc)"
        );
    }

    #[test]
    fn heartbeat_retries_until_the_lease_grace_is_exhausted() {
        assert!(!heartbeat_failure_exhausted(Duration::from_secs(49)));
        assert!(heartbeat_failure_exhausted(LEASE_FAILURE_GRACE));
    }
}
