//! DB-backed configuration for optional telemetry exporters.
//!
//! Configurable exporters are stored as typed records rather than
//! environment-variable entries. API keys are encrypted at rest and are never
//! returned by the admin API. The process-wide OTLP pipeline is surfaced as a
//! synthetic read-only exporter; Braintrust is the only mutable exporter kind
//! currently implemented.
//!
//! Adding an exporter kind: its span processor must be registered in
//! [`crate::telemetry::init_telemetry`] wrapped in
//! [`crate::telemetry::sampling::SamplingGateSpanProcessor`], sharing the one
//! gate the OTLP and Braintrust paths already share. LLM-frontend spans are
//! always recorded and thinned only at export, so a destination registered bare
//! receives every one of them regardless of the environment's sampling policy.

use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{Mutex, RwLock};

use crate::secrets::SecretCipher;
use crate::telemetry::{
    braintrust::{self, BraintrustConfig},
    TelemetryRuntime,
};

const DEFAULT_BRAINTRUST_API_URL: &str = "https://api.braintrust.dev";
const BUILTIN_OTLP_EXPORTER_ID: &str = "otel-primary";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryExporter {
    pub id: String,
    pub name: String,
    pub config: Value,
    pub enabled: bool,
    /// Synthetic environment-managed exporters cannot be mutated through the
    /// settings API. Persisted exporters are always mutable.
    pub read_only: bool,
    /// Kept in memory only. The database stores it in `api_key_envelope`.
    #[serde(skip)]
    pub api_key: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TelemetryExporter {
    pub fn exporter_type(&self) -> &str {
        self.config
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    }

    pub fn api_key_set(&self) -> bool {
        !self.api_key.is_empty()
    }

    pub fn braintrust_config(&self) -> Option<BraintrustConfig> {
        if !self.enabled || self.exporter_type() != "braintrust" || !self.api_key_set() {
            return None;
        }
        Some(BraintrustConfig::from_parts(
            self.api_key.clone(),
            self.config_string("parent"),
            self.config_string("project"),
            self.config_string("project_id"),
            self.config_string_or("api_url", DEFAULT_BRAINTRUST_API_URL),
        ))
    }

    /// The Braintrust API URL this exporter would send to, whether or not it is
    /// enabled.
    fn braintrust_api_url(&self) -> String {
        self.config_string_or("api_url", DEFAULT_BRAINTRUST_API_URL)
    }

    fn config_string(&self, key: &str) -> String {
        self.config
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    fn config_string_or(&self, key: &str, fallback: &str) -> String {
        let value = self.config_string(key);
        if value.is_empty() {
            fallback.to_string()
        } else {
            value
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() || self.name.trim().is_empty() {
            return Err("telemetry exporter id and name must be non-empty".to_string());
        }
        if !self.config.is_object() {
            return Err("telemetry exporter config must be a JSON object".to_string());
        }
        if self.exporter_type() == "unknown" || self.exporter_type().trim().is_empty() {
            return Err("telemetry exporter config.type must be a non-empty string".to_string());
        }
        if self.enabled && self.exporter_type() == "braintrust" && !self.api_key_set() {
            return Err("a Braintrust API key is required when export is enabled".to_string());
        }
        if self.exporter_type() == "braintrust" {
            let selectors = ["parent", "project", "project_id"]
                .into_iter()
                .filter(|key| !self.config_string(key).is_empty())
                .count();
            if selectors > 1 {
                return Err(
                    "configure only one Braintrust parent, project name, or project ID".to_string(),
                );
            }

            let api_url = self.config_string_or("api_url", DEFAULT_BRAINTRUST_API_URL);
            let parsed = reqwest::Url::parse(&api_url)
                .map_err(|error| format!("Braintrust API URL is invalid: {error}"))?;
            if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                return Err("Braintrust API URL must be an absolute HTTP or HTTPS URL".to_string());
            }
        }
        Ok(())
    }
}

/// Build the environment compatibility entry. The Postgres store merges it
/// with persisted exporters unless this exact ID was overridden or tombstoned.
pub fn default_telemetry_exporters() -> Vec<TelemetryExporter> {
    let Some(config) = BraintrustConfig::from_env() else {
        return Vec::new();
    };
    let api_url = std::env::var("BRAINTRUST_API_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_BRAINTRUST_API_URL.to_string());
    vec![default_telemetry_exporter(config, api_url)]
}

fn default_telemetry_exporter(config: BraintrustConfig, api_url: String) -> TelemetryExporter {
    let (parent, project, project_id) =
        if let Some(project) = config.parent.strip_prefix("project_name:") {
            (String::new(), project.to_string(), String::new())
        } else if let Some(project_id) = config.parent.strip_prefix("project_id:") {
            (String::new(), String::new(), project_id.to_string())
        } else {
            (config.parent.clone(), String::new(), String::new())
        };
    let now = Utc::now();
    TelemetryExporter {
        id: "braintrust-default".to_string(),
        name: "Braintrust".to_string(),
        config: json!({
            "type": "braintrust",
            "parent": parent,
            "project": project,
            "project_id": project_id,
            "api_url": api_url,
        }),
        enabled: true,
        read_only: false,
        api_key: config.api_key,
        created_at: now,
        updated_at: now,
    }
}

fn builtin_otel_exporter() -> TelemetryExporter {
    let now = Utc::now();
    TelemetryExporter {
        id: BUILTIN_OTLP_EXPORTER_ID.to_string(),
        name: "OpenTelemetry Pipeline".to_string(),
        config: json!({
            "type": "otlp",
            "endpoint": crate::telemetry::otlp_endpoint_from_env(),
            "protocol": "grpc",
            "service_name": "chalk-router",
            "managed_by": "environment",
        }),
        enabled: true,
        read_only: true,
        api_key: String::new(),
        created_at: now,
        updated_at: now,
    }
}

#[derive(Debug, Clone)]
pub enum TelemetrySettingsStoreError {
    NotFound,
    AlreadyExists(String),
    InvalidArgument(String),
    FailedPrecondition(String),
    Other(String),
}

impl TelemetrySettingsStoreError {
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }
}

impl std::fmt::Display for TelemetrySettingsStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "telemetry exporter not found"),
            Self::AlreadyExists(message)
            | Self::InvalidArgument(message)
            | Self::FailedPrecondition(message)
            | Self::Other(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for TelemetrySettingsStoreError {}

#[async_trait]
pub trait TelemetrySettingsStore: Send + Sync {
    async fn list(&self) -> Result<Vec<TelemetryExporter>, TelemetrySettingsStoreError>;
    async fn upsert(
        &self,
        exporter: TelemetryExporter,
        api_key: Option<String>,
    ) -> Result<(), TelemetrySettingsStoreError>;
    async fn delete(&self, id: &str) -> Result<bool, TelemetrySettingsStoreError>;
}

/// The exporter list lives behind a copy-on-write `Arc<Vec<_>>`: `list()`
/// only clones the `Arc` pointer while the lock is held, then deep-clones
/// each entry's JSON `config` after releasing it, so a large read never
/// blocks a concurrent `upsert`/`delete`. Those two mutators pay for it by
/// cloning the current vector before mutating and swapping in the result,
/// which is the right trade-off for a list read far more often than it's
/// written.
#[derive(Clone)]
pub struct InMemoryTelemetryExporterStore {
    exporters: Arc<RwLock<Arc<Vec<TelemetryExporter>>>>,
}

impl InMemoryTelemetryExporterStore {
    pub fn new(exporters: Vec<TelemetryExporter>) -> Self {
        Self {
            exporters: Arc::new(RwLock::new(Arc::new(exporters))),
        }
    }
}

#[async_trait]
impl TelemetrySettingsStore for InMemoryTelemetryExporterStore {
    async fn list(&self) -> Result<Vec<TelemetryExporter>, TelemetrySettingsStoreError> {
        let snapshot = self.exporters.read().await.clone();
        Ok((*snapshot).clone())
    }

    async fn upsert(
        &self,
        mut exporter: TelemetryExporter,
        api_key: Option<String>,
    ) -> Result<(), TelemetrySettingsStoreError> {
        let mut guard = self.exporters.write().await;
        let mut exporters = (**guard).clone();
        let now = Utc::now();
        if let Some(current) = exporters
            .iter_mut()
            .find(|current| current.id == exporter.id)
        {
            if api_key.is_none() {
                exporter.api_key = current.api_key.clone();
            }
            exporter.created_at = current.created_at;
            exporter.updated_at = now;
            *current = exporter;
        } else {
            exporter.created_at = now;
            exporter.updated_at = now;
            exporters.push(exporter);
        }
        *guard = Arc::new(exporters);
        Ok(())
    }

    /// Removes the exporter outright rather than tombstoning it like
    /// [`PostgresTelemetryExporterStore::delete`]. Postgres needs a
    /// tombstone because its `list()` recomputes `self.defaults` from the
    /// environment on every call and must remember which default ids were
    /// explicitly deleted so they don't silently reappear. This store never
    /// re-merges defaults after construction — the starting set is handed to
    /// `new()` once — so a deleted id has nothing to reappear from while the
    /// process runs, and the store holds no state that outlives the process
    /// anyway. A deleted default is final here in the same sense a Postgres
    /// tombstone is final: only a restart (which discards this store
    /// entirely) can bring it back.
    async fn delete(&self, id: &str) -> Result<bool, TelemetrySettingsStoreError> {
        let mut guard = self.exporters.write().await;
        let length = guard.len();
        let mut exporters = (**guard).clone();
        exporters.retain(|exporter| exporter.id != id);
        let removed = length != exporters.len();
        *guard = Arc::new(exporters);
        Ok(removed)
    }
}

#[derive(Clone)]
pub struct PostgresTelemetryExporterStore {
    pool: sqlx::PgPool,
    environment_id: String,
    cipher: SecretCipher,
    defaults: Vec<TelemetryExporter>,
    decrypt_warning_ids: Arc<StdMutex<HashSet<String>>>,
}

impl PostgresTelemetryExporterStore {
    pub fn new(
        pool: sqlx::PgPool,
        environment_id: String,
        cipher: SecretCipher,
        defaults: Vec<TelemetryExporter>,
    ) -> Self {
        Self {
            pool,
            environment_id,
            cipher,
            defaults,
            decrypt_warning_ids: Arc::new(StdMutex::new(HashSet::new())),
        }
    }
}

fn decrypt_api_key(
    cipher: &SecretCipher,
    warning_ids: &StdMutex<HashSet<String>>,
    id: &str,
    envelope: &[u8],
) -> String {
    match cipher.decrypt(envelope) {
        Ok(api_key) => match String::from_utf8(api_key) {
            Ok(api_key) => {
                warning_ids
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(id);
                api_key
            }
            Err(_) => {
                let first_warning = warning_ids
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .insert(id.to_string());
                if first_warning {
                    tracing::warn!(
                        exporter_id = %id,
                        "decrypted telemetry exporter API key is not valid UTF-8"
                    );
                }
                String::new()
            }
        },
        Err(error) => {
            let first_warning = warning_ids
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(id.to_string());
            if first_warning {
                tracing::warn!(
                    exporter_id = %id,
                    error = %error,
                    "failed to decrypt telemetry exporter API key (check ROUTER_SECRETS_KEK)"
                );
            }
            String::new()
        }
    }
}

fn append_unsuppressed_defaults(
    exporters: &mut Vec<TelemetryExporter>,
    stored_ids: &HashSet<String>,
    defaults: &[TelemetryExporter],
) {
    exporters.extend(
        defaults
            .iter()
            .filter(|exporter| !stored_ids.contains(&exporter.id))
            .cloned(),
    );
}

#[async_trait]
impl TelemetrySettingsStore for PostgresTelemetryExporterStore {
    async fn list(&self) -> Result<Vec<TelemetryExporter>, TelemetrySettingsStoreError> {
        let rows: Vec<(String, String, Value, bool, Vec<u8>, DateTime<Utc>, DateTime<Utc>, Option<DateTime<Utc>>)> = sqlx::query_as(
            "SELECT id, name, config, enabled, api_key_envelope, created_at, updated_at, deleted_at FROM chalk_router.telemetry_exporters WHERE environment_id = $1 ORDER BY created_at, id",
        )
        .bind(&self.environment_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| TelemetrySettingsStoreError::other(format!("failed to read telemetry exporters: {e}")))?;
        let mut stored_ids = HashSet::with_capacity(rows.len());
        let mut exporters = Vec::with_capacity(rows.len() + self.defaults.len());
        for (id, name, config, enabled, envelope, created_at, updated_at, deleted_at) in rows {
            stored_ids.insert(id.clone());
            if deleted_at.is_some() {
                self.decrypt_warning_ids
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&id);
                continue;
            }
            let api_key = decrypt_api_key(&self.cipher, &self.decrypt_warning_ids, &id, &envelope);
            exporters.push(TelemetryExporter {
                id,
                name,
                config,
                enabled,
                read_only: false,
                api_key,
                created_at,
                updated_at,
            });
        }
        append_unsuppressed_defaults(&mut exporters, &stored_ids, &self.defaults);
        Ok(exporters)
    }

    async fn upsert(
        &self,
        exporter: TelemetryExporter,
        api_key: Option<String>,
    ) -> Result<(), TelemetrySettingsStoreError> {
        let envelope = self
            .cipher
            .encrypt(api_key.as_deref().unwrap_or(&exporter.api_key).as_bytes())
            .map_err(TelemetrySettingsStoreError::other)?;
        sqlx::query(
            "INSERT INTO chalk_router.telemetry_exporters AS current (environment_id, id, name, config, enabled, api_key_envelope) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (environment_id, id) DO UPDATE SET name = EXCLUDED.name, config = EXCLUDED.config, enabled = EXCLUDED.enabled, api_key_envelope = CASE WHEN $7 THEN EXCLUDED.api_key_envelope ELSE current.api_key_envelope END, updated_at = now(), deleted_at = NULL",
        )
        .bind(&self.environment_id)
        .bind(&exporter.id)
        .bind(&exporter.name)
        .bind(&exporter.config)
        .bind(exporter.enabled)
        .bind(envelope)
        .bind(api_key.is_some())
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if matches!(
                crate::db_error::classify(&e),
                Some(crate::db_error::DbConstraint::Unique)
            ) && e.as_database_error().and_then(|error| error.constraint())
                == Some("uq_router_telemetry_exporters_name")
            {
                TelemetrySettingsStoreError::AlreadyExists(format!(
                    "a telemetry exporter named '{}' already exists",
                    exporter.name
                ))
            } else {
                TelemetrySettingsStoreError::other(format!(
                    "failed to persist telemetry exporter: {e}"
                ))
            }
        })?;
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<bool, TelemetrySettingsStoreError> {
        let result = sqlx::query(
            "UPDATE chalk_router.telemetry_exporters SET deleted_at = now(), updated_at = now() WHERE environment_id = $1 AND id = $2 AND deleted_at IS NULL",
        )
        .bind(&self.environment_id)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            TelemetrySettingsStoreError::other(format!("failed to delete telemetry exporter: {e}"))
        })?;
        if result.rows_affected() > 0 {
            return Ok(true);
        }

        let Some(default) = self.defaults.iter().find(|exporter| exporter.id == id) else {
            return Ok(false);
        };
        let envelope = self
            .cipher
            .encrypt(b"")
            .map_err(TelemetrySettingsStoreError::other)?;
        let result = sqlx::query(
            "INSERT INTO chalk_router.telemetry_exporters (environment_id, id, name, config, enabled, api_key_envelope, deleted_at) VALUES ($1, $2, $3, $4, false, $5, now()) ON CONFLICT (environment_id, id) DO NOTHING",
        )
        .bind(&self.environment_id)
        .bind(&default.id)
        .bind(&default.name)
        .bind(&default.config)
        .bind(envelope)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            TelemetrySettingsStoreError::other(format!(
                "failed to suppress default telemetry exporter: {e}"
            ))
        })?;
        Ok(result.rows_affected() > 0)
    }
}

/// Wire-agnostic service used by gRPC-Web and the runtime refresh loop.
#[derive(Clone)]
pub struct TelemetrySettingsService {
    store: Arc<dyn TelemetrySettingsStore>,
    runtime: TelemetryRuntime,
    read_only_exporters: Vec<TelemetryExporter>,
    refresh_lock: Arc<Mutex<()>>,
}

pub struct TelemetryTestOutcome {
    pub ok: bool,
    pub error: Option<String>,
    pub latency_ms: Option<u64>,
}

impl TelemetryTestOutcome {
    fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
            latency_ms: None,
        }
    }
}

impl TelemetrySettingsService {
    pub fn new(store: Arc<dyn TelemetrySettingsStore>, runtime: TelemetryRuntime) -> Self {
        Self {
            store,
            runtime,
            read_only_exporters: vec![builtin_otel_exporter()],
            refresh_lock: Arc::new(Mutex::new(())),
        }
    }

    pub async fn list(&self) -> Result<Vec<TelemetryExporter>, TelemetrySettingsStoreError> {
        let mut exporters = self.read_only_exporters.clone();
        exporters.extend(self.store.list().await?);
        Ok(exporters)
    }

    pub async fn refresh(&self) -> Result<Vec<TelemetryExporter>, TelemetrySettingsStoreError> {
        let _refresh = self.refresh_lock.lock().await;
        let exporters = self.store.list().await?;
        self.apply_runtime(&exporters);
        Ok(exporters)
    }

    pub async fn upsert(
        &self,
        mut exporter: TelemetryExporter,
        api_key: Option<String>,
    ) -> Result<TelemetryExporter, TelemetrySettingsStoreError> {
        exporter.id = exporter.id.trim().to_string();
        exporter.name = exporter.name.trim().to_string();
        let api_key = match api_key {
            Some(api_key) if api_key.trim().is_empty() => {
                return Err(TelemetrySettingsStoreError::InvalidArgument(
                    "a supplied Braintrust API key must be non-empty; omit it to preserve the existing key"
                        .to_string(),
                ));
            }
            Some(api_key) => Some(api_key.trim().to_string()),
            None => None,
        };
        if self
            .read_only_exporters
            .iter()
            .any(|current| current.id == exporter.id)
        {
            return Err(TelemetrySettingsStoreError::FailedPrecondition(
                "the OpenTelemetry pipeline is managed by environment variables and is read-only"
                    .to_string(),
            ));
        }
        let exporter_type = exporter
            .config
            .get("type")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                TelemetrySettingsStoreError::InvalidArgument(
                    "telemetry exporter config.type must be a non-empty string".to_string(),
                )
            })?
            .to_ascii_lowercase();
        exporter
            .config
            .as_object_mut()
            .ok_or_else(|| {
                TelemetrySettingsStoreError::InvalidArgument(
                    "telemetry exporter config must be a JSON object".to_string(),
                )
            })?
            .insert("type".to_string(), Value::String(exporter_type.clone()));
        if exporter_type != "braintrust" {
            return Err(TelemetrySettingsStoreError::InvalidArgument(
                "Braintrust is the only configurable telemetry exporter type".to_string(),
            ));
        }
        let existing = self.store.list().await?;
        if self
            .read_only_exporters
            .iter()
            .chain(existing.iter())
            .any(|current| {
                current.id != exporter.id
                    && current.name.trim().eq_ignore_ascii_case(&exporter.name)
            })
        {
            return Err(TelemetrySettingsStoreError::AlreadyExists(format!(
                "a telemetry exporter named '{}' already exists",
                exporter.name
            )));
        }
        if let Some(current) = existing.iter().find(|current| current.id == exporter.id) {
            if api_key.is_none() {
                exporter.api_key = current.api_key.clone();
            }
            exporter.created_at = current.created_at;
        }
        if let Some(api_key) = &api_key {
            exporter.api_key = api_key.clone();
        }
        exporter
            .validate()
            .map_err(TelemetrySettingsStoreError::InvalidArgument)?;
        self.check_destination(&exporter.braintrust_api_url())
            .map_err(TelemetrySettingsStoreError::InvalidArgument)?;
        self.store.upsert(exporter.clone(), api_key).await?;
        self.refresh().await?;
        Ok(exporter)
    }

    /// Probe an unsaved Braintrust draft without persisting it or changing the
    /// active runtime snapshot. When editing an existing exporter, omitting the
    /// key reuses its decrypted stored credential just like an upsert does.
    pub async fn test(
        &self,
        mut exporter: TelemetryExporter,
        api_key: Option<String>,
    ) -> TelemetryTestOutcome {
        exporter.id = exporter.id.trim().to_string();
        exporter.name = exporter.name.trim().to_string();

        let api_key = match api_key {
            Some(api_key) if api_key.trim().is_empty() => {
                return TelemetryTestOutcome::failed(
                    "a supplied Braintrust API key must be non-empty; omit it to use the existing key",
                );
            }
            Some(api_key) => Some(api_key.trim().to_string()),
            None => None,
        };

        let Some(config) = exporter.config.as_object_mut() else {
            return TelemetryTestOutcome::failed("telemetry exporter config must be a JSON object");
        };
        let exporter_type = config
            .get("type")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase);
        let Some(exporter_type) = exporter_type else {
            return TelemetryTestOutcome::failed(
                "telemetry exporter config.type must be a non-empty string",
            );
        };
        config.insert("type".to_string(), Value::String(exporter_type.clone()));
        if exporter_type != "braintrust" {
            return TelemetryTestOutcome::failed(
                "Braintrust is the only configurable telemetry exporter type",
            );
        }

        if let Some(api_key) = api_key {
            exporter.api_key = api_key;
        } else if !exporter.id.is_empty() {
            let existing = match self.store.list().await {
                Ok(existing) => existing,
                Err(error) => {
                    return TelemetryTestOutcome::failed(format!(
                        "failed to read the existing telemetry exporter: {error}"
                    ));
                }
            };
            let Some(current) = existing
                .into_iter()
                .find(|current| current.id == exporter.id)
            else {
                return TelemetryTestOutcome::failed("telemetry exporter not found");
            };
            exporter.api_key = current.api_key;
        }

        // A preflight always tests the destination even when the saved draft is
        // paused. Placeholder identity fields keep persistence-only validation
        // from obscuring endpoint or credential failures.
        exporter.enabled = true;
        if exporter.id.is_empty() {
            exporter.id = "braintrust-preflight".to_string();
        }
        if exporter.name.is_empty() {
            exporter.name = "Braintrust preflight".to_string();
        }
        if let Err(error) = exporter
            .validate()
            .and_then(|()| self.check_destination(&exporter.braintrust_api_url()))
        {
            return TelemetryTestOutcome::failed(error);
        }
        let Some(config) = exporter.braintrust_config() else {
            return TelemetryTestOutcome::failed("invalid Braintrust exporter configuration");
        };

        let start = Instant::now();
        match braintrust::preflight(config, self.runtime.public_egress_only()).await {
            Ok(()) => TelemetryTestOutcome {
                ok: true,
                error: None,
                latency_ms: Some(start.elapsed().as_millis() as u64),
            },
            Err(error) => TelemetryTestOutcome {
                ok: false,
                error: Some(error.to_string()),
                latency_ms: Some(start.elapsed().as_millis() as u64),
            },
        }
    }

    pub async fn delete(&self, id: &str) -> Result<(), TelemetrySettingsStoreError> {
        if self
            .read_only_exporters
            .iter()
            .any(|exporter| exporter.id == id)
        {
            return Err(TelemetrySettingsStoreError::FailedPrecondition(
                "the OpenTelemetry pipeline is managed by environment variables and is read-only"
                    .to_string(),
            ));
        }
        if !self.store.delete(id).await? {
            return Err(TelemetrySettingsStoreError::NotFound);
        }
        self.refresh().await?;
        Ok(())
    }

    /// An environment whose destinations a tenant chooses may only export to a
    /// public address (see [`crate::egress`]).
    fn check_destination(&self, api_url: &str) -> Result<(), String> {
        if !self.runtime.public_egress_only() {
            return Ok(());
        }
        crate::egress::check_url(api_url)
            .map_err(|error| format!("Braintrust API URL is not allowed: {error}"))
    }

    fn apply_runtime(&self, exporters: &[TelemetryExporter]) {
        let mut configs = Vec::new();
        for config in exporters
            .iter()
            .filter_map(TelemetryExporter::braintrust_config)
        {
            // A row saved before the policy applied, or by another path, is not
            // exported to; the guarded client still vets hostnames at connect.
            if let Err(error) = self.check_destination(&config.traces_url) {
                tracing::warn!(endpoint = %config.traces_url, %error, "skipping Braintrust destination");
                continue;
            }
            if !configs.contains(&config) {
                configs.push(config);
            }
        }
        self.runtime.set_braintrust_configs(configs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::http::{HeaderMap, StatusCode};
    use axum::{routing::post, Router};
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use prost::Message;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    fn exporter(id: &str, enabled: bool, api_key: &str) -> TelemetryExporter {
        let now = Utc::now();
        TelemetryExporter {
            id: id.to_string(),
            name: "Braintrust".to_string(),
            config: json!({
                "type": "braintrust",
                "project": "router-tests",
                "api_url": "https://api.braintrust.dev",
            }),
            enabled,
            read_only: false,
            api_key: api_key.to_string(),
            created_at: now,
            updated_at: now,
        }
    }

    async fn preflight_stub(
        expected_api_key: &'static str,
        expected_parent: &'static str,
        response_status: StatusCode,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/otel/v1/traces",
            post(move |headers: HeaderMap, body: Bytes| async move {
                if headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    != Some(expected_api_key)
                {
                    return (StatusCode::UNAUTHORIZED, "wrong authorization header");
                }
                if headers
                    .get("x-bt-parent")
                    .and_then(|value| value.to_str().ok())
                    != Some(expected_parent)
                {
                    return (StatusCode::BAD_REQUEST, "wrong parent header");
                }
                let Ok(request) = ExportTraceServiceRequest::decode(body) else {
                    return (StatusCode::BAD_REQUEST, "invalid OTLP preflight payload");
                };
                let spans = request
                    .resource_spans
                    .iter()
                    .flat_map(|resource| &resource.scope_spans)
                    .flat_map(|scope| &scope.spans)
                    .collect::<Vec<_>>();
                if spans.len() != 1
                    || spans[0].trace_id.len() != 16
                    || spans[0].span_id.len() != 8
                    || spans[0].name != "chalk-router.telemetry-preflight"
                    || spans[0].start_time_unix_nano == 0
                    || spans[0].end_time_unix_nano < spans[0].start_time_unix_nano
                {
                    return (StatusCode::BAD_REQUEST, "invalid OTLP preflight span");
                }
                (response_status, "preflight response")
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{address}")
    }

    struct SequencedListStore {
        calls: AtomicUsize,
        first_started: Notify,
        release_first: Notify,
        first: Vec<TelemetryExporter>,
        second: Vec<TelemetryExporter>,
    }

    #[async_trait]
    impl TelemetrySettingsStore for SequencedListStore {
        async fn list(&self) -> Result<Vec<TelemetryExporter>, TelemetrySettingsStoreError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.first_started.notify_one();
                self.release_first.notified().await;
                Ok(self.first.clone())
            } else {
                Ok(self.second.clone())
            }
        }

        async fn upsert(
            &self,
            _exporter: TelemetryExporter,
            _api_key: Option<String>,
        ) -> Result<(), TelemetrySettingsStoreError> {
            unreachable!("the refresh ordering test does not write")
        }

        async fn delete(&self, _id: &str) -> Result<bool, TelemetrySettingsStoreError> {
            unreachable!("the refresh ordering test does not delete")
        }
    }

    #[test]
    fn disabled_exporters_can_be_saved_without_an_api_key() {
        assert!(exporter("disabled", false, "").validate().is_ok());
    }

    #[test]
    fn enabled_braintrust_exporters_require_an_api_key() {
        let error = exporter("enabled", true, "")
            .validate()
            .expect_err("missing API key should be rejected");
        assert!(error.contains("API key"));
    }

    #[test]
    fn an_explicit_braintrust_parent_is_preserved() {
        let mut exporter = exporter("experiment", true, "secret");
        exporter.config = json!({
            "type": "braintrust",
            "parent": "experiment_id:xyz",
            "api_url": "https://api.braintrust.dev",
        });

        assert_eq!(
            exporter
                .braintrust_config()
                .expect("enabled exporter should produce runtime config")
                .parent,
            "experiment_id:xyz"
        );
    }

    #[test]
    fn environment_default_keeps_the_api_root_not_the_derived_ingest_url() {
        let exporter = default_telemetry_exporter(
            BraintrustConfig {
                traces_url: "https://api.braintrust.dev/otel/v1/traces".to_string(),
                api_key: "secret".to_string(),
                parent: "project_name:router-tests".to_string(),
            },
            "https://api.braintrust.dev".to_string(),
        );

        assert_eq!(
            exporter.config["api_url"],
            Value::String("https://api.braintrust.dev".to_string())
        );
    }

    #[test]
    fn braintrust_destination_selectors_are_mutually_exclusive() {
        for config in [
            json!({
                "type": "braintrust",
                "parent": "experiment_id:xyz",
                "project": "router-tests",
            }),
            json!({
                "type": "braintrust",
                "parent": "experiment_id:xyz",
                "project_id": "project-123",
            }),
            json!({
                "type": "braintrust",
                "project": "router-tests",
                "project_id": "project-123",
            }),
        ] {
            let mut exporter = exporter("exclusive", true, "secret");
            exporter.config = config;
            assert!(exporter.validate().unwrap_err().contains("only one"));
        }
    }

    #[test]
    fn braintrust_api_url_must_be_an_absolute_http_url() {
        for api_url in ["not a URL", "ftp://api.braintrust.dev", "/otel/v1/traces"] {
            let mut exporter = exporter("invalid-url", true, "secret");
            exporter.config["api_url"] = Value::String(api_url.to_string());
            assert!(exporter.validate().unwrap_err().contains("URL"));
        }

        let mut exporter = exporter("valid-url", true, "secret");
        exporter.config["api_url"] = Value::String("http://localhost:4318".to_string());
        assert!(exporter.validate().is_ok());
    }

    #[test]
    fn an_unreadable_api_key_is_isolated_to_its_exporter() {
        let cipher =
            SecretCipher::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap();
        let warnings = StdMutex::new(HashSet::new());
        assert_eq!(
            decrypt_api_key(&cipher, &warnings, "corrupt", &[1, 2, 3]),
            ""
        );
        assert_eq!(
            decrypt_api_key(&cipher, &warnings, "corrupt", &[1, 2, 3]),
            ""
        );
        assert_eq!(warnings.lock().unwrap().len(), 1);
        let invalid_utf8 = cipher.encrypt(&[0xff]).unwrap();
        assert_eq!(
            decrypt_api_key(&cipher, &warnings, "invalid-utf8", &invalid_utf8),
            ""
        );
        let valid = cipher.encrypt(b"recovered").unwrap();
        assert_eq!(
            decrypt_api_key(&cipher, &warnings, "corrupt", &valid),
            "recovered"
        );
        assert!(!warnings.lock().unwrap().contains("corrupt"));
    }

    #[test]
    fn unrelated_rows_do_not_hide_environment_defaults() {
        let stored = exporter("stored", true, "stored-secret");
        let default = exporter("braintrust-default", true, "env-secret");
        let mut exporters = vec![stored.clone()];
        let stored_ids = HashSet::from([stored.id]);

        append_unsuppressed_defaults(&mut exporters, &stored_ids, &[default.clone()]);

        assert_eq!(exporters.len(), 2);
        assert!(exporters.iter().any(|exporter| exporter.id == default.id));
    }

    #[test]
    fn a_tombstone_suppresses_its_environment_default() {
        let default = exporter("braintrust-default", true, "env-secret");
        let mut exporters = Vec::new();
        let stored_ids = HashSet::from([default.id.clone()]);

        append_unsuppressed_defaults(&mut exporters, &stored_ids, &[default]);

        assert!(exporters.is_empty());
    }

    #[tokio::test]
    async fn service_lists_the_environment_managed_otel_pipeline() {
        let service = TelemetrySettingsService::new(
            Arc::new(InMemoryTelemetryExporterStore::new(Vec::new())),
            TelemetryRuntime::new(Vec::new()),
        );

        let exporters = service.list().await.expect("list should succeed");
        assert_eq!(exporters.len(), 1);
        assert_eq!(exporters[0].id, BUILTIN_OTLP_EXPORTER_ID);
        assert_eq!(exporters[0].exporter_type(), "otlp");
        assert!(exporters[0].read_only);
    }

    #[tokio::test]
    async fn environment_managed_otel_pipeline_cannot_be_deleted() {
        let service = TelemetrySettingsService::new(
            Arc::new(InMemoryTelemetryExporterStore::new(Vec::new())),
            TelemetryRuntime::new(Vec::new()),
        );

        assert!(matches!(
            service.delete(BUILTIN_OTLP_EXPORTER_ID).await,
            Err(TelemetrySettingsStoreError::FailedPrecondition(_))
        ));
    }

    #[tokio::test]
    async fn service_rejects_duplicate_names_case_insensitively() {
        let mut first = exporter("first", true, "secret");
        first.name = "Production".to_string();
        let store = Arc::new(InMemoryTelemetryExporterStore::new(vec![first]));
        let service = TelemetrySettingsService::new(store, TelemetryRuntime::new(Vec::new()));
        let mut duplicate = exporter("second", true, "secret");
        duplicate.name = " production ".to_string();

        assert!(matches!(
            service.upsert(duplicate, Some("secret".to_string())).await,
            Err(TelemetrySettingsStoreError::AlreadyExists(_))
        ));
    }

    #[tokio::test]
    async fn service_applies_multiple_enabled_braintrust_exporters() {
        let store = Arc::new(InMemoryTelemetryExporterStore::new(Vec::new()));
        let runtime = TelemetryRuntime::new(Vec::new());
        let service = TelemetrySettingsService::new(store, runtime.clone());
        let mut first = exporter("first", true, "first-secret");
        first.name = "First".to_string();
        let mut second = exporter("second", true, "second-secret");
        second.name = "Second".to_string();

        service
            .upsert(first, Some("first-secret".to_string()))
            .await
            .expect("first exporter should be accepted");
        service
            .upsert(second, Some("second-secret".to_string()))
            .await
            .expect("second exporter should be accepted");

        assert_eq!(runtime.braintrust_state().snapshot().1.len(), 2);
    }

    #[tokio::test]
    async fn service_deduplicates_identical_braintrust_destinations() {
        let mut first = exporter("first", true, "shared-secret");
        first.name = "First".to_string();
        let mut duplicate = exporter("duplicate", true, "shared-secret");
        duplicate.name = "Duplicate".to_string();
        let store = Arc::new(InMemoryTelemetryExporterStore::new(vec![first, duplicate]));
        let runtime = TelemetryRuntime::new(Vec::new());
        let service = TelemetrySettingsService::new(store, runtime.clone());

        service.refresh().await.unwrap();

        assert_eq!(runtime.braintrust_state().snapshot().1.len(), 1);
    }

    #[tokio::test]
    async fn service_rejects_unimplemented_mutable_exporter_types() {
        let service = TelemetrySettingsService::new(
            Arc::new(InMemoryTelemetryExporterStore::new(Vec::new())),
            TelemetryRuntime::new(Vec::new()),
        );
        let mut exporter = exporter("datadog", true, "secret");
        exporter.config = json!({"type": "datadog"});

        assert!(matches!(
            service.upsert(exporter, Some("secret".to_string())).await,
            Err(TelemetrySettingsStoreError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn service_preserves_an_existing_api_key_when_omitted() {
        let store = Arc::new(InMemoryTelemetryExporterStore::new(vec![exporter(
            "braintrust",
            true,
            "secret",
        )]));
        let runtime = TelemetryRuntime::new(Vec::new());
        let service = TelemetrySettingsService::new(store.clone(), runtime.clone());
        let mut replacement = exporter("braintrust", true, "");
        replacement.name = "Updated Braintrust".to_string();

        service
            .upsert(replacement, None)
            .await
            .expect("upsert should preserve the stored key");

        let stored = store.list().await.expect("list should succeed");
        assert_eq!(stored[0].api_key, "secret");
        assert_eq!(stored[0].name, "Updated Braintrust");
        assert_eq!(runtime.braintrust_state().snapshot().1.len(), 1);
    }

    #[tokio::test]
    async fn preflight_reuses_the_stored_key_without_persisting_the_draft() {
        let mut existing = exporter("braintrust", true, "stored-secret");
        existing.config["project"] = Value::String("saved-project".to_string());
        let store = Arc::new(InMemoryTelemetryExporterStore::new(vec![existing]));
        let runtime = TelemetryRuntime::new(Vec::new());
        let service = TelemetrySettingsService::new(store.clone(), runtime.clone());
        let mut draft = exporter("braintrust", false, "");
        draft.config["project"] = Value::String("draft-project".to_string());
        draft.config["api_url"] = Value::String(
            preflight_stub(
                "Bearer stored-secret",
                "project_name:draft-project",
                StatusCode::OK,
            )
            .await,
        );

        let outcome = service.test(draft, None).await;

        assert!(outcome.ok, "preflight failed: {:?}", outcome.error);
        assert!(outcome.latency_ms.is_some());
        let stored = store.list().await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].config["project"], "saved-project");
        assert!(runtime.braintrust_state().snapshot().1.is_empty());
    }

    #[tokio::test]
    async fn preflight_reports_braintrust_rejections_with_the_response_body() {
        let service = TelemetrySettingsService::new(
            Arc::new(InMemoryTelemetryExporterStore::new(Vec::new())),
            TelemetryRuntime::new(Vec::new()),
        );
        let mut draft = exporter("", true, "");
        draft.config["api_url"] = Value::String(
            preflight_stub(
                "Bearer rejected-secret",
                "project_name:router-tests",
                StatusCode::UNAUTHORIZED,
            )
            .await,
        );

        let outcome = service
            .test(draft, Some("rejected-secret".to_string()))
            .await;

        assert!(!outcome.ok);
        let error = outcome.error.unwrap();
        assert!(error.contains("401"), "{error}");
        assert!(error.contains("preflight response"), "{error}");
        assert!(outcome.latency_ms.is_some());
    }

    #[tokio::test]
    async fn public_egress_only_refuses_private_braintrust_destinations() {
        let store = Arc::new(InMemoryTelemetryExporterStore::new(Vec::new()));
        let runtime = TelemetryRuntime::new(Vec::new());
        runtime.set_public_egress_only(true);
        let service = TelemetrySettingsService::new(store.clone(), runtime.clone());

        for api_url in [
            "http://169.254.169.254",
            "http://127.0.0.1:4318",
            "http://10.0.0.5:8080",
            "http://collector.default.svc.cluster.local",
        ] {
            let mut draft = exporter("braintrust", true, "secret");
            draft.config["api_url"] = Value::String(api_url.to_string());

            let error = service
                .upsert(draft.clone(), None)
                .await
                .expect_err("a private destination must not be saved");
            assert!(
                error.to_string().contains("not allowed"),
                "{api_url}: {error}"
            );

            let outcome = service.test(draft, Some("secret".to_string())).await;
            assert!(!outcome.ok, "{api_url}");
            assert!(outcome.error.unwrap().contains("not allowed"), "{api_url}");
        }
        assert!(store.list().await.unwrap().is_empty());

        service
            .upsert(exporter("braintrust", true, "secret"), None)
            .await
            .expect("the public default destination is allowed");
        assert_eq!(runtime.braintrust_state().snapshot().1.len(), 1);
    }

    #[tokio::test]
    async fn public_egress_only_skips_a_stored_private_destination() {
        let mut stored = exporter("braintrust", true, "secret");
        stored.config["api_url"] = Value::String("http://10.0.0.5:8080".to_string());
        let runtime = TelemetryRuntime::new(Vec::new());
        runtime.set_public_egress_only(true);
        let service = TelemetrySettingsService::new(
            Arc::new(InMemoryTelemetryExporterStore::new(vec![stored.clone()])),
            runtime.clone(),
        );
        service.refresh().await.unwrap();
        assert!(runtime.braintrust_state().snapshot().1.is_empty());

        // A trusted environment keeps it.
        let trusted = TelemetryRuntime::new(Vec::new());
        TelemetrySettingsService::new(
            Arc::new(InMemoryTelemetryExporterStore::new(vec![stored])),
            trusted.clone(),
        )
        .refresh()
        .await
        .unwrap();
        assert_eq!(trusted.braintrust_state().snapshot().1.len(), 1);
    }

    #[tokio::test]
    async fn preflight_requires_a_key_for_a_new_exporter() {
        let service = TelemetrySettingsService::new(
            Arc::new(InMemoryTelemetryExporterStore::new(Vec::new())),
            TelemetryRuntime::new(Vec::new()),
        );

        let outcome = service.test(exporter("", false, ""), None).await;

        assert!(!outcome.ok);
        assert!(outcome.error.unwrap().contains("API key"));
        assert!(outcome.latency_ms.is_none());
    }

    #[tokio::test]
    async fn service_rejects_an_explicitly_blank_api_key_update() {
        let store = Arc::new(InMemoryTelemetryExporterStore::new(vec![exporter(
            "braintrust",
            true,
            "secret",
        )]));
        let service =
            TelemetrySettingsService::new(store.clone(), TelemetryRuntime::new(Vec::new()));
        let mut replacement = exporter("braintrust", false, "");
        replacement.name = "Renamed".to_string();

        assert!(matches!(
            service.upsert(replacement, Some("   ".to_string())).await,
            Err(TelemetrySettingsStoreError::InvalidArgument(_))
        ));
        let stored = store.list().await.unwrap();
        assert_eq!(stored[0].api_key, "secret");
        assert_eq!(stored[0].name, "Braintrust");
    }

    #[tokio::test]
    async fn concurrent_refreshes_cannot_apply_a_stale_snapshot_last() {
        let old = exporter("old", true, "old-secret");
        let mut new = exporter("new", true, "new-secret");
        new.config["project"] = Value::String("new-project".to_string());
        let store = Arc::new(SequencedListStore {
            calls: AtomicUsize::new(0),
            first_started: Notify::new(),
            release_first: Notify::new(),
            first: vec![old],
            second: vec![new],
        });
        let runtime = TelemetryRuntime::new(Vec::new());
        let service = Arc::new(TelemetrySettingsService::new(
            store.clone(),
            runtime.clone(),
        ));

        let first = tokio::spawn({
            let service = service.clone();
            async move { service.refresh().await.unwrap() }
        });
        store.first_started.notified().await;
        let second = tokio::spawn({
            let service = service.clone();
            async move { service.refresh().await.unwrap() }
        });
        tokio::task::yield_now().await;
        store.release_first.notify_one();
        first.await.unwrap();
        second.await.unwrap();

        let (_, configs) = runtime.braintrust_state().snapshot();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].api_key, "new-secret");
        assert_eq!(configs[0].parent, "project_name:new-project");
    }
}
