//! Provider-connection service layer: validation, persistence, and connection
//! probing. Wire-format agnostic — the gRPC layer parses proto and dispatches
//! here.

use std::sync::Arc;
use std::sync::{Arc as StdArc, LazyLock};
use std::time::{Duration, Instant};

use crate::backend::anthropic::{anthropic_probe_client, validate_anthropic_base_url};
use crate::backend::BackendProvider;
use crate::provider_connections::{
    ConnectionInput, ConnectionMetadata, ConnectionUpdate, DeleteConnectionOutcome, ExposurePolicy,
    ProviderConnectionStore, StoreError, StoredConnection,
};
use crate::provider_registry;
use crate::service::pagination::{parse_stable_cursor, resolve_limit, StablePageCursor};
use crate::service::ServiceError;

const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_PROBE_CONCURRENCY: usize = 8;
const PROBE_QUEUE_TIMEOUT: Duration = Duration::from_millis(250);

fn configured_duration(name: &str, default: Duration) -> Duration {
    match std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    {
        Some(0) | None => default,
        Some(seconds) => Duration::from_secs(seconds),
    }
}

fn configured_concurrency() -> usize {
    std::env::var("CHALK_PROVIDER_PROBE_MAX_CONCURRENCY")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .filter(|limit: &usize| *limit > 0)
        .unwrap_or(DEFAULT_PROBE_CONCURRENCY)
}

/// A short administrative probe must not occupy request tasks for the router's
/// 30-second model-catalog timeout. These values are read once on first use.
static PROVIDER_PROBE_TIMEOUT: LazyLock<Duration> = LazyLock::new(|| {
    configured_duration("CHALK_PROVIDER_PROBE_TIMEOUT_SECS", DEFAULT_PROBE_TIMEOUT)
});
static PROVIDER_PROBE_LIMIT: LazyLock<StdArc<tokio::sync::Semaphore>> =
    LazyLock::new(|| StdArc::new(tokio::sync::Semaphore::new(configured_concurrency())));

/// What [`ProviderConnectionService::test`] should probe.
pub enum TestTarget {
    /// A saved connection, by id.
    Existing(String),
    /// An unsaved draft (never persisted).
    Draft(DraftInput),
    /// A complete draft with a saved connection's credential as the fallback.
    SavedDraft {
        connection_id: String,
        draft: DraftInput,
    },
}

/// An unsaved connection config used only to build a throwaway probe adapter.
pub struct DraftInput {
    pub provider_kind: String,
    pub base_url: Option<String>,
    pub prefix: Option<String>,
    pub exposure: ExposurePolicy,
    pub api_key: String,
}

/// The outcome of a connection probe.
pub struct TestOutcome {
    pub ok: bool,
    pub error: Option<String>,
    pub discovered_models: Option<u32>,
    pub latency_ms: Option<u64>,
}

/// Result of an idempotent provider-connection delete.
#[derive(Debug)]
pub struct DeleteOutcome {
    pub deleted: bool,
    /// The deleted connection's prefix, retained for post-delete warnings.
    pub prefix: Option<String>,
}

impl TestOutcome {
    /// A failure that occurred before (or instead of) a network probe, so there
    /// is no meaningful latency to report.
    fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
            discovered_models: None,
            latency_ms: None,
        }
    }
}

pub struct ProviderConnectionService {
    store: Arc<dyn ProviderConnectionStore>,
    http_client: reqwest::Client,
    connection_policy: crate::provider_connections::ConnectionPolicy,
}

impl ProviderConnectionService {
    /// A service for operator-controlled callers (seeds, tests). Request-serving
    /// callers take their policy from `AppState::provider_connection_service`.
    pub fn new(store: Arc<dyn ProviderConnectionStore>, http_client: reqwest::Client) -> Self {
        Self {
            store,
            http_client,
            connection_policy: crate::provider_connections::ConnectionPolicy::trusted(),
        }
    }

    pub fn with_connection_policy(
        mut self,
        policy: crate::provider_connections::ConnectionPolicy,
    ) -> Self {
        self.connection_policy = policy;
        self
    }

    fn enforce_credential_policy(
        &self,
        kind: &str,
        base_url: Option<&str>,
        api_key: &str,
    ) -> Result<(), ServiceError> {
        self.connection_policy
            .check_credentials(kind, base_url, api_key)
            .map_err(|message| ServiceError::invalid_field("api_key", message))
    }

    /// An environment the operator does not control may only point a
    /// connection at a public destination (see [`crate::egress`]).
    fn enforce_endpoint_policy(&self, base_url: Option<&str>) -> Result<(), ServiceError> {
        self.connection_policy
            .check_endpoint(base_url)
            .map_err(|message| {
                ServiceError::invalid_field(
                    "base_url",
                    format!("base_url is not allowed: {message}"),
                )
            })
    }

    pub async fn list_page(
        &self,
        limit: Option<i32>,
        cursor: Option<String>,
    ) -> Result<(Vec<ConnectionMetadata>, Option<String>), ServiceError> {
        let limit = resolve_limit(limit)?;
        let cursor = parse_stable_cursor(cursor.as_deref())?;
        // The bounded metadata query omits credential envelopes on cache hits;
        // misses are hydrated in one additional batch query.
        let mut connections = self
            .store
            .list_metadata_page(cursor.as_ref(), limit + 1)
            .await
            .map_err(store_err)?;
        if connections.len() as i64 > limit {
            connections.truncate(limit as usize);
            let next_cursor = connections.last().map(|connection| {
                StablePageCursor {
                    created_at: connection.created_at,
                    id: connection.id.clone(),
                }
                .encode()
            });
            Ok((connections, next_cursor))
        } else {
            Ok((connections, None))
        }
    }

    /// Credential-reading helper for routing/bootstrap callers. Admin RPCs use
    /// the metadata cache via [`Self::list_page`].
    pub async fn list(&self) -> Result<Vec<StoredConnection>, ServiceError> {
        self.store.list().await.map_err(store_err)
    }

    pub async fn create(
        &self,
        mut input: ConnectionInput,
    ) -> Result<ConnectionMetadata, ServiceError> {
        if let Some(declared) = &input.declared_capabilities {
            declared.validate().map_err(|error| {
                ServiceError::invalid_field("declared_capabilities", error.to_string())
            })?;
        }
        // A blank/whitespace base_url is not a real override: normalize it to
        // None so a built-in falls back to its registry default rather than being
        // stored as a literal empty endpoint (which routing would treat as a real,
        // unusable URL). For openai-compatible this then fails validation below,
        // exactly as a missing base_url should.
        input.base_url = normalize_base_url(input.base_url.take());
        self.enforce_endpoint_policy(input.base_url.as_deref())?;

        validate(
            &input.name,
            &input.provider_kind,
            input.base_url.as_deref(),
            input.prefix.as_deref(),
            input.exposure,
        )?;

        // Runtime Bedrock authenticates through the process AWS credential
        // chain; vLLM and arbitrary OpenAI-compatible endpoints may be
        // keyless. Store the placeholder so the encrypted credential column
        // remains non-null without inventing a public auth configuration
        // surface.
        if input.api_key.trim().is_empty() {
            if input.provider_kind == "vllm"
                || input.provider_kind == crate::provider_registry::OPENAI_COMPATIBLE_KIND
                || (input.provider_kind == "bedrock"
                    && crate::backend::bedrock::is_runtime_connection(input.base_url.as_deref()))
            {
                input.api_key = crate::provider_connections::KEYLESS_PLACEHOLDER.to_string();
            } else {
                return Err(ServiceError::invalid_field("api_key", "must not be empty"));
            }
        }
        self.enforce_credential_policy(
            &input.provider_kind,
            input.base_url.as_deref(),
            &input.api_key,
        )?;

        let created = self.store.create(input).await.map_err(store_err)?;
        tracing::info!(
            id = %created.id,
            name = %created.name,
            kind = %created.provider_kind,
            "created provider connection"
        );
        Ok(created)
    }

    pub async fn update(
        &self,
        id: &str,
        mut upd: ConnectionUpdate,
    ) -> Result<ConnectionMetadata, ServiceError> {
        if let Some(Some(declared)) = &upd.declared_capabilities {
            declared.validate().map_err(|error| {
                ServiceError::invalid_field("declared_capabilities", error.to_string())
            })?;
        }
        // Normalize a supplied blank/whitespace base_url override to None (use the
        // registry default) so it can't be stored as a literal empty endpoint.
        // A *missing* base_url (outer None) is left untouched — it means "leave
        // the stored value unchanged".
        if let Some(base_url) = upd.base_url.take() {
            upd.base_url = Some(normalize_base_url(base_url));
        }

        // Validate against the *effective* connection (existing merged with the
        // update); provider_kind is immutable so it comes from the stored row.
        let existing = self
            .store
            .get_metadata(id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| ServiceError::not_found(format!("provider connection '{id}'")))?;

        let kind = existing.provider_kind.clone();
        let cur_prefix = existing.prefix;
        let cur_base_url = existing.base_url;
        let cur_exposure = existing.exposure;
        let cur_name = existing.name;

        let eff_name = upd.name.clone().unwrap_or(cur_name);
        let eff_base_url = match &upd.base_url {
            Some(v) => v.clone(),
            None => cur_base_url,
        };
        let eff_prefix = match &upd.prefix {
            Some(v) => v.clone(),
            None => cur_prefix,
        };
        let eff_exposure = upd.exposure.unwrap_or(cur_exposure);
        // Only a change of endpoint is policed here; a row that already holds a
        // private endpoint is refused when its backend is built.
        if upd.base_url.is_some() {
            self.enforce_endpoint_policy(eff_base_url.as_deref())?;
        }

        validate(
            &eff_name,
            &kind,
            eff_base_url.as_deref(),
            eff_prefix.as_deref(),
            eff_exposure,
        )?;

        // A key on update is optional (absent = leave unchanged), but an
        // explicitly-supplied empty key is never valid — including for vLLM,
        // whose keyless placeholder is set at create time.
        if let Some(api_key) = &upd.api_key {
            if api_key.trim().is_empty() {
                return Err(ServiceError::invalid_field("api_key", "must not be empty"));
            }
        }

        // The effective credential is the supplied key, or else the stored one
        // (an update may retarget a keyless connection at Bedrock Runtime).
        let effective_key = match &upd.api_key {
            Some(api_key) => Some(api_key.clone()),
            None => match self.store.get(id).await.map_err(store_err)? {
                Some(StoredConnection::Readable(connection)) => Some(connection.api_key),
                _ => None,
            },
        };
        if let Some(api_key) = effective_key {
            self.enforce_credential_policy(&kind, eff_base_url.as_deref(), &api_key)?;
        }

        let updated = self.store.update(id, upd).await.map_err(store_err)?;
        tracing::info!(id = %updated.id, "updated provider connection");
        Ok(updated)
    }

    /// Idempotent delete. Already-gone connections succeed. A connection that
    /// still has model routes pointing at it is rejected with
    /// [`ServiceError::FailedPrecondition`] listing those routes'
    /// `public_model_id`s — operators must delete or repoint routes first.
    ///
    /// `store.delete_if_unreferenced` is the single authority for the
    /// reference check — this method performs no pre-scan and takes no lock
    /// itself. PostgreSQL's implementation locks the connection row and
    /// checks `model_routes` inside the same transaction as the delete;
    /// `InMemoryProviderConnectionStore::with_routes` takes
    /// `CONNECTION_ROUTE_MUTATION_LOCK` itself and checks its wired route
    /// store before deleting. Either way the route scan happens exactly once.
    pub async fn delete(&self, id: &str) -> Result<DeleteOutcome, ServiceError> {
        finish_delete(
            self.store
                .delete_if_unreferenced(id)
                .await
                .map_err(store_err)?,
        )
    }

    async fn resolve_test_draft(&self, target: TestTarget) -> Result<DraftInput, String> {
        let (id, draft) = match target {
            TestTarget::Draft(draft) => return Ok(draft),
            TestTarget::Existing(id) => (id, None),
            TestTarget::SavedDraft {
                connection_id,
                draft,
            } => (connection_id, Some(draft)),
        };
        let stored = self
            .store
            .get(&id)
            .await
            .map_err(|error| format!("store error: {error}"))?
            .ok_or_else(|| "connection not found".to_string())?;

        if let Some(draft) = &draft {
            if draft.provider_kind != stored.provider_kind() {
                return Err("draft provider kind must match the saved connection".to_string());
            }
        }
        // use a supplied draft key without requiring the saved credential to be readable
        if let Some(draft) = draft
            .as_ref()
            .filter(|draft| !draft.api_key.trim().is_empty())
        {
            return Ok(DraftInput {
                provider_kind: draft.provider_kind.clone(),
                base_url: draft.base_url.clone(),
                prefix: draft.prefix.clone(),
                exposure: draft.exposure,
                api_key: draft.api_key.trim().to_string(),
            });
        }
        let saved = match stored {
            StoredConnection::Readable(connection) => connection,
            StoredConnection::Unreadable { error, .. } => {
                return Err(format!("stored credential could not be decrypted: {error}"));
            }
        };
        // use saved key when draft key is blank. if no draft supplied, use all saved settings
        Ok(match draft {
            Some(mut draft) => {
                draft.api_key = saved.api_key;
                draft
            }
            None => DraftInput {
                provider_kind: saved.provider_kind,
                base_url: saved.base_url,
                prefix: saved.prefix,
                exposure: saved.exposure,
                api_key: saved.api_key,
            },
        })
    }

    /// Probe a saved connection or an unsaved draft by listing its models. Never
    /// persists anything. Errors are reported in [`TestOutcome`], not as
    /// [`ServiceError`], so a failing upstream is a normal (ok:false) result.
    pub async fn test(&self, target: TestTarget) -> TestOutcome {
        let DraftInput {
            provider_kind: kind,
            api_key,
            base_url,
            prefix,
            exposure,
        } = match self.resolve_test_draft(target).await {
            Ok(draft) => draft,
            Err(error) => return TestOutcome::failed(error),
        };

        // A probe connects to `base_url` with the supplied key, so it is held to
        // the same endpoint policy as saving the connection.
        if let Err(error) = self.enforce_endpoint_policy(base_url.as_deref()) {
            return TestOutcome::failed(error.message());
        }
        if let Err(error) =
            validate_connection_config(&kind, base_url.as_deref(), prefix.as_deref(), exposure)
        {
            return TestOutcome::failed(error.message());
        }
        if let Err(error) = self.enforce_credential_policy(&kind, base_url.as_deref(), &api_key) {
            return TestOutcome::failed(error.message());
        }

        let permit = match tokio::time::timeout(
            PROBE_QUEUE_TIMEOUT,
            PROVIDER_PROBE_LIMIT.clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) | Err(_) => {
                return TestOutcome::failed(
                    "provider connection test capacity is busy; retry shortly",
                );
            }
        };
        let probe_client = if kind == "anthropic" {
            match anthropic_probe_client(base_url.as_deref()).await {
                Ok(client) => client,
                Err(error) => return TestOutcome::failed(error.to_string()),
            }
        } else {
            self.http_client.clone()
        };
        let adapter = match build_probe_adapter(probe_client, &kind, &api_key, base_url.as_deref())
        {
            Ok(a) => a,
            Err(e) => return TestOutcome::failed(e),
        };

        let start = Instant::now();
        let outcome =
            match tokio::time::timeout(*PROVIDER_PROBE_TIMEOUT, adapter.probe_models()).await {
                Ok(Ok(list)) => TestOutcome {
                    ok: true,
                    error: None,
                    discovered_models: Some(list.data.len() as u32),
                    latency_ms: Some(start.elapsed().as_millis() as u64),
                },
                Ok(Err(e)) => TestOutcome {
                    ok: false,
                    error: Some(e.to_string()),
                    discovered_models: None,
                    latency_ms: Some(start.elapsed().as_millis() as u64),
                },
                Err(_) => TestOutcome {
                    ok: false,
                    error: Some(format!(
                        "connection test timed out after {}s",
                        PROVIDER_PROBE_TIMEOUT.as_secs()
                    )),
                    discovered_models: None,
                    latency_ms: Some(start.elapsed().as_millis() as u64),
                },
            };
        drop(permit);
        outcome
    }
}

/// Map a store's [`DeleteConnectionOutcome`] into the service's [`DeleteOutcome`]
/// (or a [`ServiceError`] when routes still reference the connection).
fn finish_delete(outcome: DeleteConnectionOutcome) -> Result<DeleteOutcome, ServiceError> {
    match outcome {
        DeleteConnectionOutcome::Deleted { prefix } => Ok(DeleteOutcome {
            deleted: true,
            prefix,
        }),
        DeleteConnectionOutcome::NotFound => Ok(DeleteOutcome {
            deleted: false,
            prefix: None,
        }),
        DeleteConnectionOutcome::Referenced { public_model_ids } => {
            Err(ServiceError::failed_precondition(format!(
                "cannot delete connection: still referenced by model routes: {}",
                public_model_ids.join(", ")
            )))
        }
    }
}

/// Trim a `base_url` override and treat blank/whitespace as "no override"
/// (`None`), so a built-in falls back to its registry default rather than a
/// literal empty endpoint. Applied at the service boundary so no write path can
/// persist `Some("")` (which routing would treat as a real, unusable URL).
fn normalize_base_url(base_url: Option<String>) -> Option<String> {
    base_url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
}

/// Validate a connection's effective fields against the registry.
fn validate(
    name: &str,
    kind: &str,
    base_url: Option<&str>,
    prefix: Option<&str>,
    exposure: ExposurePolicy,
) -> Result<(), ServiceError> {
    if name.trim().is_empty() {
        return Err(ServiceError::invalid_field("name", "must not be empty"));
    }

    validate_connection_config(kind, base_url, prefix, exposure)
}

/// Validate the connection fields shared by create, update, and unsaved draft
/// probes. Keeping this separate from `name` lets TestProviderConnection reject
/// unsafe endpoints before a throwaway adapter can attach the supplied key.
fn validate_connection_config(
    kind: &str,
    base_url: Option<&str>,
    prefix: Option<&str>,
    exposure: ExposurePolicy,
) -> Result<(), ServiceError> {
    let is_builtin = provider_registry::is_builtin(kind);
    let is_compatible = kind == provider_registry::OPENAI_COMPATIBLE_KIND;
    if !is_builtin && !is_compatible {
        return Err(ServiceError::invalid_field(
            "provider_kind",
            format!("unknown provider kind '{kind}'"),
        ));
    }

    // vLLM and arbitrary OpenAI-compatible endpoints have no code-owned default
    // endpoint, so they must supply their own base URL. Without one the
    // connection is accepted but every dispatch fails as unconfigured, so reject
    // it at create/update time instead. `requires_explicit_base_url` is exactly
    // that set (`vllm` + `openai-compatible`).
    if provider_registry::requires_explicit_base_url(kind)
        && !base_url.map(|b| !b.trim().is_empty()).unwrap_or(false)
    {
        return Err(ServiceError::invalid_field(
            "base_url",
            "base_url is required for vllm and openai-compatible connections",
        ));
    }

    if kind == "bedrock" {
        crate::backend::bedrock::runtime_endpoint(base_url)
            .map_err(|error| ServiceError::invalid_field("base_url", error.to_string()))?;
    }

    if kind == "anthropic" {
        validate_anthropic_base_url(base_url)
            .map_err(|error| ServiceError::invalid_field("base_url", error.to_string()))?;
    }

    match prefix {
        None => {
            if exposure != ExposurePolicy::Explicit {
                return Err(ServiceError::invalid_field(
                    "prefix",
                    "prefix is required unless exposure is EXPLICIT",
                ));
            }
        }
        Some(prefix) => {
            if !prefix_is_valid(prefix) {
                return Err(ServiceError::invalid_field(
                    "prefix",
                    "prefix must be a single segment matching ^[a-z0-9][a-z0-9_-]*/$",
                ));
            }
            if prefix == provider_registry::ROUTE_RESERVED_PREFIX {
                return Err(ServiceError::invalid_field(
                    "prefix",
                    format!(
                        "'{}' is reserved for model routes",
                        provider_registry::ROUTE_RESERVED_PREFIX
                    ),
                ));
            }
            if provider_registry::prefix_reserved_by_other_kind(prefix, kind) {
                return Err(ServiceError::invalid_field(
                    "prefix",
                    format!("prefix '{prefix}' is reserved by another built-in provider"),
                ));
            }
        }
    }

    Ok(())
}

/// Whether `prefix` matches `^[a-z0-9][a-z0-9_-]*/$`: a single lowercase
/// alphanumeric segment (with `_`/`-` allowed after the first char) ending in a
/// trailing slash. Hand-rolled to avoid a regex dependency.
fn prefix_is_valid(prefix: &str) -> bool {
    let Some(segment) = prefix.strip_suffix('/') else {
        return false;
    };
    let mut chars = segment.chars();
    let Some(first) = chars.next() else {
        return false; // empty segment ("/")
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Build a throwaway backend adapter for a probe by dispatching on the
/// provider kind through
/// [`build_provider_adapter`](crate::backend::router::build_provider_adapter)
/// — the exact per-kind construction logic routing uses for stored
/// connections — so anything routable is also testable. The base URL comes
/// from the same place routing gets it: the operator `base_url` override when
/// present, otherwise the constructor's own default (region-derived for
/// `bedrock`, the registry default endpoint for the other OpenAI-compatible
/// built-ins). Not cached, not persisted.
///
/// A probe has no stored connection name to identify a custom
/// `openai-compatible` backend by, so the
/// [`OPENAI_COMPATIBLE_KIND`](provider_registry::OPENAI_COMPATIBLE_KIND)
/// literal is passed through as the backend identity for that arm, matching
/// what `build_probe_adapter` has always used here.
///
/// Only genuinely-unconstructible configs are rejected: `vllm` and the
/// `openai-compatible` pseudo-kind both require a base URL.
fn build_probe_adapter(
    http_client: reqwest::Client,
    kind: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<Arc<dyn BackendProvider>, String> {
    crate::backend::router::build_provider_adapter(
        http_client,
        kind,
        provider_registry::OPENAI_COMPATIBLE_KIND,
        api_key,
        base_url,
    )
    .map_err(|error| error.to_string())
}

/// Map a store error into the wire-agnostic [`ServiceError`].
fn store_err(e: StoreError) -> ServiceError {
    match e {
        StoreError::NotFound => ServiceError::not_found("provider connection"),
        StoreError::Conflict(field) => {
            let message = match field.as_str() {
                "prefix" => "a connection with this prefix already exists",
                _ => "a connection with this name already exists",
            };
            ServiceError::invalid_field(field, message)
        }
        StoreError::Db(msg) => {
            tracing::error!(error = %msg, "provider connection store error");
            ServiceError::internal("database error")
        }
        StoreError::Encrypt(msg) => {
            tracing::error!(error = %msg, "provider connection encryption error");
            ServiceError::internal("encryption error")
        }
        StoreError::InvalidCapabilityDeclaration { id, name, error } => {
            tracing::error!(connection_id = %id, connection_name = %name, error = %error, "invalid stored capability declaration");
            ServiceError::internal(format!(
                "provider connection '{name}' has invalid declared_capabilities"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probe adapter under the trusted policy.
    fn probe_adapter(
        http_client: reqwest::Client,
        kind: &str,
        api_key: &str,
        base_url: Option<&str>,
    ) -> Result<Arc<dyn BackendProvider>, String> {
        build_probe_adapter(http_client, kind, api_key, base_url)
    }
    use crate::provider_connections::InMemoryProviderConnectionStore;

    fn service() -> ProviderConnectionService {
        ProviderConnectionService::new(
            Arc::new(InMemoryProviderConnectionStore::new()),
            reqwest::Client::new(),
        )
    }

    /// Wires the in-memory connection store to `routes` so it owns its own
    /// delete-time reference check (mirroring production; see
    /// `InMemoryProviderConnectionStore::with_routes`).
    fn service_with_routes(
        routes: Arc<dyn crate::model_routes::ModelRouteStore>,
    ) -> (ProviderConnectionService, Arc<dyn ProviderConnectionStore>) {
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::with_routes(routes));
        (
            ProviderConnectionService::new(store.clone(), reqwest::Client::new()),
            store,
        )
    }

    fn input(
        name: &str,
        kind: &str,
        base_url: Option<&str>,
        prefix: Option<&str>,
        exposure: ExposurePolicy,
    ) -> ConnectionInput {
        ConnectionInput {
            name: name.to_string(),
            provider_kind: kind.to_string(),
            base_url: base_url.map(str::to_string),
            prefix: prefix.map(str::to_string),
            exposure,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "sk-test".to_string(),
        }
    }

    #[tokio::test]
    async fn saved_draft_uses_credentials_and_edits_without_persisting() {
        for kind in ["openai", "anthropic"] {
            let credentials = Arc::new(std::sync::Mutex::new(Vec::new()));
            let observed = credentials.clone();
            let app = axum::Router::new().route(
                "/draft/models",
                axum::routing::get(move |headers: axum::http::HeaderMap| {
                    let header = if kind == "anthropic" {
                        "x-api-key"
                    } else {
                        "authorization"
                    };
                    observed
                        .lock()
                        .unwrap()
                        .push(headers[header].to_str().unwrap().to_string());
                    async {
                        axum::Json(
                            serde_json::json!({ "object": "list", "data": [], "has_more": false }),
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let svc = service();
            let saved = svc
                .create(input(
                    "saved",
                    kind,
                    Some(&format!("{endpoint}/saved")),
                    Some("saved/"),
                    ExposurePolicy::Dynamic,
                ))
                .await
                .unwrap();

            for api_key in ["  ", "replacement"] {
                let outcome = svc
                    .test(TestTarget::SavedDraft {
                        connection_id: saved.id.clone(),
                        draft: DraftInput {
                            provider_kind: kind.to_string(),
                            base_url: Some(format!("{endpoint}/draft")),
                            prefix: None,
                            exposure: ExposurePolicy::Explicit,
                            api_key: api_key.to_string(),
                        },
                    })
                    .await;
                assert!(outcome.ok, "{:?}", outcome.error);
            }
            let expected = if kind == "anthropic" {
                vec!["sk-test", "replacement"]
            } else {
                vec!["Bearer sk-test", "Bearer replacement"]
            };
            assert_eq!(*credentials.lock().unwrap(), expected);
            let StoredConnection::Readable(after) =
                svc.store.get(&saved.id).await.unwrap().unwrap()
            else {
                unreachable!("in-memory credentials are readable")
            };
            assert_eq!(after.api_key, "sk-test");
            assert_eq!(after.base_url, saved.base_url);
            assert_eq!(after.prefix, saved.prefix);
            assert_eq!(after.exposure, saved.exposure);
            assert_eq!(after.updated_at, saved.updated_at);
            server.abort();
        }
    }

    #[tokio::test]
    async fn saved_draft_rejects_missing_connections_and_changed_provider_kinds() {
        let svc = service();
        let saved = svc
            .create(input(
                "saved",
                "openai",
                None,
                Some("openai/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap();
        for (id, kind, error) in [
            ("missing", "openai", "connection not found"),
            (
                saved.id.as_str(),
                "anthropic",
                "draft provider kind must match the saved connection",
            ),
        ] {
            for api_key in ["", "replacement"] {
                let outcome = svc
                    .test(TestTarget::SavedDraft {
                        connection_id: id.to_string(),
                        draft: DraftInput {
                            provider_kind: kind.to_string(),
                            base_url: None,
                            prefix: Some("test/".to_string()),
                            exposure: ExposurePolicy::Dynamic,
                            api_key: api_key.to_string(),
                        },
                    })
                    .await;
                assert!(!outcome.ok);
                assert_eq!(outcome.error.as_deref(), Some(error));
                assert_eq!(outcome.latency_ms, None);
            }
        }
    }

    #[tokio::test]
    async fn saved_draft_validates_overridden_endpoint_before_using_saved_key() {
        let svc = service();
        let saved = svc
            .create(input(
                "saved",
                "anthropic",
                None,
                Some("anthropic/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap();
        let outcome = svc
            .test(TestTarget::SavedDraft {
                connection_id: saved.id,
                draft: DraftInput {
                    provider_kind: "anthropic".to_string(),
                    base_url: Some("http://169.254.169.254/latest/meta-data".to_string()),
                    prefix: Some("anthropic/".to_string()),
                    exposure: ExposurePolicy::Dynamic,
                    api_key: String::new(),
                },
            })
            .await;
        assert!(!outcome.ok);
        assert!(outcome
            .error
            .unwrap()
            .contains("must use a public host or a loopback address"));
        assert_eq!(outcome.latency_ms, None);
    }

    #[tokio::test]
    async fn saved_draft_can_clear_base_url_and_preserve_keyless_credentials() {
        let svc = service();
        let mut fields = input(
            "saved",
            "vllm",
            Some("http://localhost:8000/v1"),
            Some("local/"),
            ExposurePolicy::Dynamic,
        );
        fields.api_key.clear();
        let saved = svc.create(fields).await.unwrap();
        let resolved = svc
            .resolve_test_draft(TestTarget::SavedDraft {
                connection_id: saved.id,
                draft: DraftInput {
                    provider_kind: "vllm".to_string(),
                    base_url: None,
                    prefix: None,
                    exposure: ExposurePolicy::Explicit,
                    api_key: String::new(),
                },
            })
            .await
            .unwrap_or_else(|error| unreachable!("{error}"));
        assert_eq!(
            resolved.api_key,
            crate::provider_connections::KEYLESS_PLACEHOLDER
        );
        assert_eq!(resolved.base_url, None);
        assert_eq!(resolved.prefix, None);
        assert_eq!(resolved.exposure, ExposurePolicy::Explicit);
    }

    #[tokio::test]
    async fn rejects_unknown_kind() {
        let svc = service();
        let err = svc
            .create(input(
                "x",
                "nope",
                None,
                Some("nope/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::InvalidArgument { .. }));
    }

    #[tokio::test]
    async fn rejects_new_connections_using_the_legacy_grok_kind() {
        let svc = service();
        let err = svc
            .create(input(
                "legacy",
                "grok",
                None,
                Some("grok/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::InvalidArgument { .. }));
    }

    #[tokio::test]
    async fn openai_compatible_requires_base_url() {
        let svc = service();
        let err = svc
            .create(input(
                "x",
                "openai-compatible",
                None,
                Some("x/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ServiceError::InvalidArgument { field: Some(f), .. } if f == "base_url")
        );
    }

    #[tokio::test]
    async fn vllm_requires_base_url() {
        // vLLM has no code-owned default endpoint, so a base_url is required at
        // create time — otherwise the connection is accepted but never routes.
        let svc = service();
        let err = svc
            .create(input(
                "vLLM",
                "vllm",
                None,
                Some("vllm/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ServiceError::InvalidArgument { field: Some(f), .. } if f == "base_url")
        );
        // With a base_url it creates fine.
        svc.create(input(
            "vLLM",
            "vllm",
            Some("http://localhost:8000/v1"),
            Some("vllm/"),
            ExposurePolicy::Dynamic,
        ))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn blank_base_url_normalized_to_none_for_builtin() {
        // A whitespace-only override on a built-in is stored as None (the
        // registry default), never a literal empty endpoint.
        let svc = service();
        let created = svc
            .create(input(
                "OpenAI",
                "openai",
                Some("   "),
                Some("openai/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap();
        assert_eq!(created.base_url, None);
    }

    #[tokio::test]
    async fn blank_base_url_rejected_for_openai_compatible() {
        // Whitespace normalizes to None, which openai-compatible then rejects,
        // exactly as a missing base_url does.
        let svc = service();
        let err = svc
            .create(input(
                "x",
                "openai-compatible",
                Some("   "),
                Some("x/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ServiceError::InvalidArgument { field: Some(f), .. } if f == "base_url")
        );
    }

    #[tokio::test]
    async fn anthropic_base_url_requires_a_safe_absolute_endpoint() {
        let svc = service();
        for invalid in [
            "relative/path",
            "ftp://gateway.example/v1",
            "https://user:pass@gateway.example/v1",
            "https://gateway.example/v1?redirect=elsewhere",
            "https://gateway.example/v1#fragment",
            "http://10.0.0.1/v1",
            "http://169.254.169.254/latest/meta-data",
            "http://[fd00::1]/v1",
            "http://[fe80::1]/v1",
        ] {
            let error = svc
                .create(input(
                    "Anthropic",
                    "anthropic",
                    Some(invalid),
                    Some("anthropic/"),
                    ExposurePolicy::Dynamic,
                ))
                .await
                .expect_err("invalid Anthropic base URL");
            assert!(
                matches!(error, ServiceError::InvalidArgument { field: Some(field), .. } if field == "base_url")
            );
        }

        svc.create(input(
            "Anthropic",
            "anthropic",
            Some("https://gateway.example/proxy/v1"),
            Some("anthropic/"),
            ExposurePolicy::Dynamic,
        ))
        .await
        .expect("public absolute HTTP endpoint");
    }

    #[tokio::test]
    async fn builtin_prefix_may_be_custom() {
        let svc = service();
        let created = svc
            .create(input(
                "OpenAI",
                "openai",
                None,
                Some("gpt/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap();
        assert_eq!(created.prefix.as_deref(), Some("gpt/"));

        svc.create(input(
            "OpenAI canonical",
            "openai",
            None,
            Some("openai/"),
            ExposurePolicy::Dynamic,
        ))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn prefix_required_unless_explicit() {
        let svc = service();
        let err = svc
            .create(input(
                "x",
                "openai-compatible",
                Some("https://x/v1"),
                None,
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ServiceError::InvalidArgument { field: Some(f), .. } if f == "prefix")
        );

        // Explicit exposure allows a null prefix.
        svc.create(input(
            "x",
            "openai-compatible",
            Some("https://x/v1"),
            None,
            ExposurePolicy::Explicit,
        ))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn openai_compatible_cannot_steal_a_builtin_prefix() {
        let svc = service();
        let err = svc
            .create(input(
                "x",
                "openai-compatible",
                Some("https://x/v1"),
                Some("openai/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ServiceError::InvalidArgument { field: Some(f), .. } if f == "prefix")
        );
    }

    #[tokio::test]
    async fn chalk_prefix_is_reserved() {
        let svc = service();
        let err = svc
            .create(input(
                "x",
                "openai-compatible",
                Some("https://x/v1"),
                Some("chalk/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ServiceError::InvalidArgument { field: Some(f), .. } if f == "prefix")
        );
    }

    #[tokio::test]
    async fn create_without_api_key_is_rejected_for_builtin() {
        let svc = service();
        let mut inp = input(
            "OpenAI",
            "openai",
            None,
            Some("openai/"),
            ExposurePolicy::Dynamic,
        );
        inp.api_key = "   ".to_string(); // whitespace-only counts as empty
        let err = svc.create(inp).await.unwrap_err();
        assert!(
            matches!(err, ServiceError::InvalidArgument { field: Some(f), .. } if f == "api_key")
        );
    }

    #[tokio::test]
    async fn vllm_create_without_key_stores_placeholder() {
        let svc = service();
        let mut inp = input(
            "vLLM",
            "vllm",
            Some("https://vllm/v1"),
            Some("vllm/"),
            ExposurePolicy::Dynamic,
        );
        inp.api_key = String::new();
        let created = svc.create(inp).await.unwrap();
        assert_eq!(
            created.credential_state,
            crate::provider_connections::CredentialState::Keyless
        );
    }

    #[tokio::test]
    async fn openai_compatible_create_without_key_stores_placeholder() {
        let svc = service();
        let mut inp = input(
            "Keyless compatible",
            "openai-compatible",
            Some("https://compatible.example/v1"),
            Some("compatible/"),
            ExposurePolicy::Dynamic,
        );
        inp.api_key = String::new();
        let created = svc.create(inp).await.unwrap();
        assert_eq!(
            created.credential_state,
            crate::provider_connections::CredentialState::Keyless
        );
    }

    #[tokio::test]
    async fn bedrock_runtime_create_without_key_stores_placeholder() {
        let svc = service();
        let mut inp = input(
            "Bedrock Runtime",
            "bedrock",
            Some("https://bedrock-runtime.us-west-2.amazonaws.com"),
            Some("bedrock/"),
            ExposurePolicy::Dynamic,
        );
        inp.api_key = String::new();
        let created = svc.create(inp).await.unwrap();
        assert_eq!(
            created.credential_state,
            crate::provider_connections::CredentialState::Keyless
        );
    }

    fn isolated_service() -> ProviderConnectionService {
        service().with_connection_policy(crate::provider_connections::ConnectionPolicy::isolated())
    }

    fn bedrock_runtime_input(api_key: &str) -> ConnectionInput {
        let mut inp = input(
            "Bedrock Runtime",
            "bedrock",
            Some("https://bedrock-runtime.us-west-2.amazonaws.com"),
            Some("bedrock/"),
            ExposurePolicy::Dynamic,
        );
        inp.api_key = api_key.to_string();
        inp
    }

    #[tokio::test]
    async fn isolated_policy_rejects_keyless_bedrock_runtime_on_create() {
        let svc = isolated_service();
        let error = svc.create(bedrock_runtime_input("")).await.unwrap_err();
        assert!(
            error.message().contains("Bedrock API key"),
            "{}",
            error.message()
        );
        assert!(svc.list().await.unwrap().is_empty());

        let created = svc
            .create(bedrock_runtime_input("bedrock-key"))
            .await
            .unwrap();
        assert_eq!(
            created.credential_state,
            crate::provider_connections::CredentialState::Present
        );
    }

    #[tokio::test]
    async fn isolated_policy_rejects_retargeting_a_keyless_connection_at_bedrock_runtime() {
        // A keyless connection written before the policy applied (or by a trusted
        // seed) must not be able to start using the process identity by update.
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        let trusted = ProviderConnectionService::new(store.clone(), reqwest::Client::new());
        let seeded = trusted.create(bedrock_runtime_input("")).await.unwrap();
        let isolated = ProviderConnectionService::new(store, reqwest::Client::new())
            .with_connection_policy(crate::provider_connections::ConnectionPolicy::isolated());

        let error = isolated
            .update(
                &seeded.id,
                ConnectionUpdate {
                    name: Some("renamed".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            error.message().contains("Bedrock API key"),
            "{}",
            error.message()
        );

        // Supplying a key is the way out.
        isolated
            .update(
                &seeded.id,
                ConnectionUpdate {
                    api_key: Some("bedrock-key".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn isolated_policy_rejects_private_endpoints_on_create() {
        let svc = isolated_service();
        for (kind, base_url, prefix) in [
            ("vllm", "http://10.0.0.5:8000/v1", "vllm/"),
            ("openai-compatible", "http://127.0.0.1:8000/v1", "x/"),
            ("openai", "http://169.254.169.254/v1", "openai/"),
            (
                "openai-compatible",
                "http://vllm.default.svc.cluster.local/v1",
                "y/",
            ),
        ] {
            let error = svc
                .create(input(
                    "c",
                    kind,
                    Some(base_url),
                    Some(prefix),
                    ExposurePolicy::Dynamic,
                ))
                .await
                .unwrap_err();
            assert!(
                error.message().contains("not allowed"),
                "{base_url}: {}",
                error.message()
            );
        }
        assert!(svc.list().await.unwrap().is_empty());

        // Public endpoints, including a customer's own vLLM, are fine.
        svc.create(input(
            "mine",
            "vllm",
            Some("https://vllm.customer.example/v1"),
            Some("vllm/"),
            ExposurePolicy::Dynamic,
        ))
        .await
        .unwrap();
        svc.create(input(
            "openai",
            "openai",
            None,
            Some("openai/"),
            ExposurePolicy::Dynamic,
        ))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn isolated_policy_rejects_pointing_an_existing_connection_at_a_private_endpoint() {
        let svc = isolated_service();
        let created = svc
            .create(input(
                "openai",
                "openai",
                None,
                Some("openai/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap();
        let error = svc
            .update(
                &created.id,
                ConnectionUpdate {
                    base_url: Some(Some("http://10.0.0.5/v1".into())),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            error.message().contains("not allowed"),
            "{}",
            error.message()
        );
        // Clearing an override is always fine.
        svc.update(
            &created.id,
            ConnectionUpdate {
                base_url: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn trusted_service_keeps_private_endpoints() {
        service()
            .create(input(
                "vllm",
                "vllm",
                Some("http://10.0.0.5:8000/v1"),
                Some("vllm/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn isolated_policy_refuses_to_probe_a_private_endpoint() {
        let outcome = isolated_service()
            .test(TestTarget::Draft(DraftInput {
                provider_kind: "openai".into(),
                base_url: Some("http://169.254.169.254/latest".into()),
                prefix: Some("openai/".into()),
                exposure: ExposurePolicy::Dynamic,
                api_key: "sk-test".into(),
            }))
            .await;
        assert!(!outcome.ok);
        assert!(outcome.error.unwrap().contains("not allowed"));
    }

    #[tokio::test]
    async fn isolated_policy_refuses_to_probe_keyless_bedrock_runtime() {
        let outcome = isolated_service()
            .test(TestTarget::Draft(DraftInput {
                provider_kind: "bedrock".into(),
                base_url: Some("https://bedrock-runtime.us-west-2.amazonaws.com".into()),
                prefix: Some("bedrock/".into()),
                exposure: ExposurePolicy::Dynamic,
                api_key: String::new(),
            }))
            .await;
        assert!(!outcome.ok);
        assert!(outcome.error.unwrap().contains("Bedrock API key"));
    }

    #[tokio::test]
    async fn update_with_explicitly_empty_api_key_is_rejected() {
        let svc = service();
        let created = svc
            .create(input(
                "OpenAI",
                "openai",
                None,
                Some("openai/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap();
        let upd = ConnectionUpdate {
            api_key: Some("  ".to_string()),
            ..Default::default()
        };
        let err = svc.update(&created.id, upd).await.unwrap_err();
        assert!(
            matches!(err, ServiceError::InvalidArgument { field: Some(f), .. } if f == "api_key")
        );
    }

    #[tokio::test]
    async fn create_rejects_unknown_capability_schema_versions() {
        let svc = service();
        let mut input = input(
            "OpenAI",
            "openai",
            None,
            Some("openai/"),
            ExposurePolicy::Dynamic,
        );
        input.declared_capabilities = Some(crate::capabilities::CapabilityPatch {
            schema_version: 999,
            ..Default::default()
        });

        let error = svc.create(input).await.unwrap_err();
        assert!(
            matches!(error, ServiceError::InvalidArgument { field: Some(field), .. } if field == "declared_capabilities")
        );
    }

    #[test]
    fn prefix_validation() {
        assert!(prefix_is_valid("openai/"));
        assert!(prefix_is_valid("a/"));
        assert!(prefix_is_valid("my-endpoint_2/"));
        assert!(!prefix_is_valid("/"));
        assert!(!prefix_is_valid("openai"));
        assert!(!prefix_is_valid("-bad/"));
        assert!(!prefix_is_valid("Bad/"));
        assert!(!prefix_is_valid("a/b/"));
    }

    #[test]
    fn default_bedrock_probe_adapter_builds_without_base_url() {
        // F3: bedrock has no registry default_base_url — its endpoint is
        // region-derived at construction. A default connection (no explicit
        // base_url) must still build a probe adapter, just as routing does,
        // rather than being rejected as untestable.
        assert!(
            probe_adapter(reqwest::Client::new(), "bedrock", "sk-test", None).is_ok(),
            "default bedrock connection should be testable without a base_url"
        );
    }

    #[test]
    fn probe_adapter_builds_for_every_routable_kind() {
        // Every built-in kind that derives an endpoint internally (registry
        // default or, for bedrock, a region-derived one) is constructible with
        // no operator base_url override, mirroring `build_provider_adapter`.
        // `vertex` is absent because its credential is a service account key
        // rather than a bearer token, so `"sk-test"` cannot build one — see
        // `probe_adapter_rejects_a_vertex_credential_that_is_not_a_key`.
        for kind in [
            "openai",
            "anthropic",
            "gemini",
            "voyage",
            "typesafe",
            "bedrock",
            "openrouter",
            "xai",
            "cohere",
            "qwen",
            "fireworks",
            "together",
            "groq",
            "mistral",
            "perplexity",
        ] {
            assert!(
                probe_adapter(reqwest::Client::new(), kind, "sk-test", None).is_ok(),
                "kind '{kind}' should build a probe adapter without a base_url"
            );
        }
    }

    #[tokio::test]
    async fn anthropic_probe_uses_its_base_url_override() {
        let adapter = probe_adapter(
            reqwest::Client::new(),
            "anthropic",
            "test-key",
            Some("http://127.0.0.1:9/custom"),
        )
        .expect("build Anthropic probe adapter");

        let error = adapter
            .probe_models()
            .await
            .expect_err("loopback connection refused");

        match error {
            crate::backend::BackendError::RequestError(error) => {
                let url = error.url().expect("request URL");
                assert_eq!(url.host_str(), Some("127.0.0.1"));
                assert_eq!(url.port(), Some(9));
                assert_eq!(url.path(), "/custom/models");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn anthropic_connection_probe_stops_after_one_page() {
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicUsize, Ordering};

        async fn models(
            axum::extract::State(calls): axum::extract::State<Arc<AtomicUsize>>,
            axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
        ) -> axum::Json<serde_json::Value> {
            calls.fetch_add(1, Ordering::SeqCst);
            assert!(
                !query.contains_key("after_id"),
                "probe fetched a second page"
            );
            axum::Json(serde_json::json!({
                "data": [{
                    "type": "model",
                    "id": "claude-probe",
                    "created_at": "2026-07-24T00:00:00Z"
                }],
                "has_more": true,
                "last_id": "next-page"
            }))
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route("/models", axum::routing::get(models))
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let adapter = probe_adapter(
            reqwest::Client::new(),
            "anthropic",
            "test-key",
            Some(&format!("http://{addr}")),
        )
        .expect("build Anthropic probe adapter");
        let models = adapter.probe_models().await;

        assert_eq!(models.expect("probe models").data.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn private_anthropic_draft_endpoint_sends_no_authenticated_request() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        async fn models(
            axum::extract::State(calls): axum::extract::State<Arc<AtomicUsize>>,
        ) -> axum::Json<serde_json::Value> {
            calls.fetch_add(1, Ordering::SeqCst);
            axum::Json(serde_json::json!({
                "data": [],
                "has_more": false,
                "last_id": null
            }))
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route("/models", axum::routing::get(models))
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let outcome = service()
            .test(TestTarget::Draft(DraftInput {
                provider_kind: "anthropic".to_string(),
                base_url: Some("http://10.0.0.1/v1".to_string()),
                prefix: Some("anthropic/".to_string()),
                exposure: ExposurePolicy::Dynamic,
                api_key: "test-key".to_string(),
            }))
            .await;

        assert!(!outcome.ok);
        assert!(outcome
            .error
            .unwrap_or_default()
            .contains("must use a public host or a loopback address"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn draft_probe_runs_shared_connection_validation() {
        let outcome = service()
            .test(TestTarget::Draft(DraftInput {
                provider_kind: "anthropic".to_string(),
                base_url: None,
                prefix: Some("Bad/".to_string()),
                exposure: ExposurePolicy::Dynamic,
                api_key: "test-key".to_string(),
            }))
            .await;

        assert!(!outcome.ok);
        assert!(outcome.error.unwrap_or_default().contains("prefix must"));
        assert_eq!(outcome.latency_ms, None);
    }

    #[tokio::test]
    async fn private_existing_anthropic_endpoint_is_not_probed() {
        let store = Arc::new(InMemoryProviderConnectionStore::new());
        let connection = store
            .create(input(
                "Legacy Anthropic",
                "anthropic",
                Some("http://169.254.169.254/latest/meta-data"),
                Some("anthropic/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .expect("insert legacy connection directly");
        let svc = ProviderConnectionService::new(store, reqwest::Client::new());

        let outcome = svc.test(TestTarget::Existing(connection.id)).await;

        assert!(!outcome.ok);
        assert!(outcome
            .error
            .unwrap_or_default()
            .contains("must use a public host or a loopback address"));
        assert_eq!(outcome.latency_ms, None);
    }

    #[tokio::test]
    async fn local_anthropic_mock_supports_create_update_and_both_probe_targets() {
        check_local_anthropic_mock_supports_create_update_and_both_probe_targets("127.0.0.1").await;
    }

    #[tokio::test]
    async fn localhost_anthropic_mock_supports_create_update_and_both_probe_targets() {
        check_local_anthropic_mock_supports_create_update_and_both_probe_targets("localhost").await;
    }

    async fn check_local_anthropic_mock_supports_create_update_and_both_probe_targets(host: &str) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get({
                let calls = calls.clone();
                move |headers: axum::http::HeaderMap| async move {
                    assert_eq!(headers["x-api-key"], "unused");
                    calls.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({
                        "data": [{"id": "mock/fast", "created_at": "2026-09-09T00:00:00Z"}],
                        "has_more": false
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{host}:{}/v1", listener.local_addr().unwrap().port());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let svc = service();
        let mut connection = input(
            "Local Anthropic",
            "anthropic",
            Some(&base_url),
            Some("mock-anthropic/"),
            ExposurePolicy::Unlisted,
        );
        connection.api_key = "unused".to_string();
        let created = svc.create(connection).await.unwrap();
        svc.update(
            &created.id,
            ConnectionUpdate {
                name: Some("Renamed local mock".to_string()),
                base_url: Some(Some(format!("{base_url}/"))),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        for target in [
            TestTarget::Existing(created.id.clone()),
            TestTarget::Draft(DraftInput {
                provider_kind: "anthropic".to_string(),
                base_url: Some(base_url.clone()),
                prefix: Some("mock-anthropic/".to_string()),
                exposure: ExposurePolicy::Unlisted,
                api_key: "unused".to_string(),
            }),
        ] {
            let outcome = svc.test(target).await;
            assert!(outcome.ok, "{:?}", outcome.error);
            assert_eq!(outcome.discovered_models, Some(1));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // Custom loopback API roots are accepted, even if the mock doesn't serve them.
        assert!(svc
            .update(
                &created.id,
                ConnectionUpdate {
                    base_url: Some(Some(base_url.replace("/v1", "/other"))),
                    ..Default::default()
                }
            )
            .await
            .is_ok());

        // The saved URL now points to /other/models, which returns 404.
        assert!(!svc.test(TestTarget::Existing(created.id)).await.ok);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn local_anthropic_probe_does_not_follow_redirects() {
        check_local_anthropic_probe_does_not_follow_redirects("127.0.0.1").await;
    }

    #[tokio::test]
    async fn localhost_anthropic_probe_does_not_follow_redirects() {
        check_local_anthropic_probe_does_not_follow_redirects("localhost").await;
    }

    async fn check_local_anthropic_probe_does_not_follow_redirects(host: &str) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route(
                "/v1/models",
                axum::routing::get(|| async {
                    axum::response::Redirect::temporary("/secret-target")
                }),
            )
            .route(
                "/secret-target",
                axum::routing::get({
                    let calls = calls.clone();
                    move || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({"data": [], "has_more": false}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{host}:{}/v1", listener.local_addr().unwrap().port());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let svc = service();
        let outcome = svc
            .test(TestTarget::Draft(DraftInput {
                provider_kind: "anthropic".to_string(),
                base_url: Some(base_url),
                prefix: Some("mock-anthropic/".to_string()),
                exposure: ExposurePolicy::Unlisted,
                api_key: "unused".to_string(),
            }))
            .await;
        assert!(!outcome.ok);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[test]
    fn probe_adapter_requires_base_url_for_unconstructible_kinds() {
        // The only genuinely-unconstructible configs: vllm and the
        // openai-compatible pseudo-kind both require a base URL.
        assert!(probe_adapter(reqwest::Client::new(), "vllm", "sk-test", None).is_err());
        assert!(probe_adapter(
            reqwest::Client::new(),
            provider_registry::OPENAI_COMPATIBLE_KIND,
            "sk-test",
            None,
        )
        .is_err());
        // With a base_url they build fine.
        assert!(probe_adapter(
            reqwest::Client::new(),
            "vllm",
            "sk-test",
            Some("http://localhost:8000"),
        )
        .is_ok());
        assert!(probe_adapter(
            reqwest::Client::new(),
            provider_registry::OPENAI_COMPATIBLE_KIND,
            "sk-test",
            Some("https://x/v1")
        )
        .is_ok());
    }

    #[test]
    fn probe_adapter_builds_for_a_vertex_service_account_key() {
        let key = r#"{
            "type": "service_account",
            "project_id": "my-proj",
            "private_key": "-----BEGIN PRIVATE KEY-----\nMIIB\n-----END PRIVATE KEY-----\n",
            "client_email": "router@my-proj.iam.gserviceaccount.com"
        }"#;
        assert!(
            probe_adapter(reqwest::Client::new(), "vertex", key, None).is_ok(),
            "a service account key should build a vertex probe adapter without a base_url"
        );
    }

    #[test]
    fn probe_adapter_rejects_a_vertex_credential_that_is_not_a_key() {
        // Pasting a Gemini AI Studio API key into a Vertex connection is the
        // likely mixup; Test should say so rather than report a 401 from Google.
        let err = match probe_adapter(reqwest::Client::new(), "vertex", "AIzaSyNotAKey", None) {
            Err(err) => err,
            Ok(_) => panic!("an API key must not build a vertex probe adapter"),
        };
        assert!(
            err.contains("service account key"),
            "unhelpful error: {err}"
        );
    }

    #[test]
    fn probe_adapter_rejects_unknown_kind() {
        assert!(probe_adapter(reqwest::Client::new(), "nope", "sk-test", None).is_err());
    }

    #[tokio::test]
    async fn delete_blocked_when_routes_still_reference_connection() {
        use crate::model_routes::{InMemoryModelRouteStore, ModelRouteStore, RouteInput};

        let routes: Arc<dyn ModelRouteStore> = Arc::new(InMemoryModelRouteStore::new());
        let (svc, _) = service_with_routes(routes.clone());
        let conn = svc
            .create(input(
                "OpenAI",
                "openai",
                None,
                Some("openai/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap();
        routes
            .create(RouteInput {
                routing: Default::default(),
                public_model_id: "chalk/fast".to_string(),
                connection_id: conn.id.clone(),
                upstream_model_id: "gpt-4o".to_string(),
                published: true,
            })
            .await
            .unwrap();

        let err = svc.delete(&conn.id).await.unwrap_err();
        assert!(
            matches!(err, ServiceError::FailedPrecondition { .. }),
            "got {err:?}"
        );
        assert!(
            err.message().contains("chalk/fast"),
            "message should list the blocking route: {}",
            err.message()
        );
        // Connection must still exist.
        assert!(svc.store.get_metadata(&conn.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_succeeds_after_routes_are_removed() {
        use crate::model_routes::{InMemoryModelRouteStore, ModelRouteStore, RouteInput};

        let routes: Arc<dyn ModelRouteStore> = Arc::new(InMemoryModelRouteStore::new());
        let (svc, _) = service_with_routes(routes.clone());
        let conn = svc
            .create(input(
                "OpenAI",
                "openai",
                None,
                Some("openai/"),
                ExposurePolicy::Dynamic,
            ))
            .await
            .unwrap();
        let route = routes
            .create(RouteInput {
                routing: Default::default(),
                public_model_id: "chalk/fast".to_string(),
                connection_id: conn.id.clone(),
                upstream_model_id: "gpt-4o".to_string(),
                published: true,
            })
            .await
            .unwrap();

        assert!(svc.delete(&conn.id).await.is_err());
        routes.delete(&route.id).await.unwrap();
        assert!(svc.delete(&conn.id).await.unwrap().deleted);
        // Idempotent re-delete.
        assert!(!svc.delete(&conn.id).await.unwrap().deleted);
    }
}
