//! Provider connections: an upstream credential + endpoint config. Every kind
//! may have multiple connections; names and non-null routing prefixes remain
//! unique within an environment. Replaces the old `provider_credentials` store.
//!
//! In Postgres the API key is stored **encrypted at rest** (AES-256-GCM under
//! `ROUTER_SECRETS_KEK`; see [`crate::secrets`]) — it cannot be hashed because
//! it must be replayed verbatim to the upstream provider. The key columns are
//! `NOT NULL`, so keyless upstreams (e.g. an unauthenticated vLLM) still store
//! an encrypted placeholder ([`KEYLESS_PLACEHOLDER`]).
//!
//! A read distinguishes three states so an unreadable ciphertext is never
//! confused with an absent one: `None` (no row), [`StoredConnection::Readable`],
//! and [`StoredConnection::Unreadable`] (row present but decrypt failed —
//! wrong/rotated KEK or corruption). This mirrors the old
//! `provider_credentials::StoredCredential` three-state model.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use uuid::Uuid;

use crate::capabilities::CapabilityPatch;
use crate::model_routes::ModelRouteStore;
use crate::secrets::SecretCipher;
use crate::service::pagination::StablePageCursor;

/// The plaintext stored for a keyless upstream. Encrypted like any other key so
/// the `NOT NULL` key columns are always satisfied.
pub const KEYLESS_PLACEHOLDER: &str = "unused";

/// Whether a stored credential means "use no stored key".
pub fn is_keyless(api_key: &str) -> bool {
    let api_key = api_key.trim();
    api_key.is_empty() || api_key == KEYLESS_PLACEHOLDER
}

/// What an environment's provider connections may borrow from, or point at,
/// beyond a provider's own public API and the connection's own credential.
///
/// - **Cloud identity.** A keyless Bedrock Runtime connection signs with the
///   router process's own AWS credential chain, which is the operator's
///   identity.
/// - **Private destinations.** A `base_url` or a Vertex `token_uri`
///   makes the router connect to an address the tenant chose,
///   from inside the operator's network. See [`crate::egress`].
///
/// A cluster router serves other teams' environments, so only its legacy
/// environment is trusted with either. Caller-supplied images never inherit
/// this trust: their downloader always uses public-only egress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionPolicy {
    ambient_cloud_credentials: bool,
    public_egress_only: bool,
}

impl ConnectionPolicy {
    /// Operator seeds, tests, and the one environment an environment-mode router
    /// serves (or a cluster's legacy environment).
    pub const fn trusted() -> Self {
        Self {
            ambient_cloud_credentials: true,
            public_egress_only: false,
        }
    }

    /// Connections must carry their own credentials and may reach public
    /// destinations only.
    pub const fn isolated() -> Self {
        Self {
            ambient_cloud_credentials: false,
            public_egress_only: true,
        }
    }

    /// The policy for `environment` under `config`. A cluster router trusts only
    /// its legacy environment, which is named by deployment configuration.
    pub fn for_environment(config: &crate::config::RouterConfig, environment: &str) -> Self {
        if config.cluster.is_none() || config.legacy_environment_id.as_deref() == Some(environment)
        {
            Self::trusted()
        } else {
            Self::isolated()
        }
    }

    /// Whether requests made for this environment may reach public addresses
    /// only. When true, the environment's HTTP client is
    /// [`crate::egress::guarded_client`].
    pub const fn public_egress_only(&self) -> bool {
        self.public_egress_only
    }

    /// Reject a connection that would sign with the process's own identity, or
    /// that hands the router a URL to call during authentication.
    pub fn check_credentials(
        &self,
        provider_kind: &str,
        base_url: Option<&str>,
        api_key: &str,
    ) -> Result<(), String> {
        if !self.ambient_cloud_credentials
            && provider_kind == "bedrock"
            && is_keyless(api_key)
            && crate::backend::bedrock::is_runtime_connection(base_url)
        {
            return Err(
                "keyless Bedrock Runtime connections sign with the router's own AWS identity \
                 and are not available in this deployment; supply a Bedrock API key"
                    .to_string(),
            );
        }
        // A Vertex service-account key names the endpoint that receives its
        // signed assertion, so it is a destination the tenant chose too.
        if self.public_egress_only && provider_kind == "vertex" {
            if let Some(token_uri) = vertex_token_uri(api_key) {
                crate::egress::check_url(&token_uri)
                    .map_err(|error| format!("token_uri is not allowed: {error}"))?;
            }
        }
        Ok(())
    }

    /// Reject a `base_url` that is not a public destination. Always `Ok` for a
    /// trusted environment.
    pub fn check_endpoint(&self, base_url: Option<&str>) -> Result<(), String> {
        match base_url.map(str::trim).filter(|url| !url.is_empty()) {
            Some(url) if self.public_egress_only => crate::egress::check_url(url),
            _ => Ok(()),
        }
    }

    /// Both checks, for when a connection is turned into a backend.
    pub fn check(
        &self,
        provider_kind: &str,
        base_url: Option<&str>,
        api_key: &str,
    ) -> Result<(), String> {
        self.check_endpoint(base_url)
            .map_err(|error| format!("base_url is not allowed: {error}"))?;
        self.check_credentials(provider_kind, base_url, api_key)
    }
}

/// The `token_uri` of a Vertex credential, read the way the backend reads it
/// (raw or base64-encoded JSON) so the check cannot disagree with what is used.
/// `None` when the credential does not parse; building its backend fails then.
fn vertex_token_uri(api_key: &str) -> Option<String> {
    crate::backend::gcp_auth::GcpCredentials::parse(api_key)
        .ok()
        .map(|credentials| credentials.token_uri().to_string())
}

/// Credential health for non-secret connection metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialState {
    /// Metadata has not yet been joined with the cached or decrypted envelope.
    Unknown,
    /// A real upstream credential is stored.
    Present,
    /// The connection intentionally uses no credential.
    Keyless,
    /// The stored envelope cannot be decrypted with the active key.
    Unreadable,
}

impl CredentialState {
    pub(crate) fn from_api_key(api_key: &str) -> Self {
        if api_key.is_empty() || api_key == KEYLESS_PLACEHOLDER {
            Self::Keyless
        } else {
            Self::Present
        }
    }

    pub(crate) fn unreadable(self) -> bool {
        self == Self::Unreadable
    }
}

/// How a connection participates in prefix-based routing and `/v1/models`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExposurePolicy {
    /// Callable by prefix; discovered models listed in `/v1/models`.
    Dynamic,
    /// Callable by prefix; omitted from `/v1/models`.
    Unlisted,
    /// No prefix routing; reachable only via a model route.
    Explicit,
}

impl ExposurePolicy {
    /// The value persisted in the `exposure_policy` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            ExposurePolicy::Dynamic => "dynamic",
            ExposurePolicy::Unlisted => "unlisted",
            ExposurePolicy::Explicit => "explicit",
        }
    }

    /// Parse a persisted `exposure_policy` value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dynamic" => Some(ExposurePolicy::Dynamic),
            "unlisted" => Some(ExposurePolicy::Unlisted),
            "explicit" => Some(ExposurePolicy::Explicit),
            _ => None,
        }
    }
}

/// A stored connection with its API key decrypted into memory.
#[derive(Debug, Clone)]
pub struct ProviderConnection {
    pub id: String,
    pub name: String,
    pub provider_kind: String,
    pub base_url: Option<String>,
    pub prefix: Option<String>,
    pub exposure: ExposurePolicy,
    pub routing_enabled: bool,
    pub declared_capabilities: Option<CapabilityPatch>,
    /// Decrypted key (or the [`KEYLESS_PLACEHOLDER`] for keyless upstreams).
    pub api_key: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The non-secret portion of a provider connection.
///
/// Admin list and route-diagnostic paths only need these fields. PostgreSQL
/// selects envelopes only for cache misses, then serves unchanged connection
/// health from an `updated_at`-aware cache.
#[derive(Debug, Clone)]
pub struct ConnectionMetadata {
    pub id: String,
    pub name: String,
    pub provider_kind: String,
    pub base_url: Option<String>,
    pub prefix: Option<String>,
    pub exposure: ExposurePolicy,
    pub routing_enabled: bool,
    pub declared_capabilities: Option<CapabilityPatch>,
    pub credential_state: CredentialState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ConnectionMetadata {
    fn from_connection(connection: &ProviderConnection) -> Self {
        Self {
            id: connection.id.clone(),
            name: connection.name.clone(),
            provider_kind: connection.provider_kind.clone(),
            base_url: connection.base_url.clone(),
            prefix: connection.prefix.clone(),
            exposure: connection.exposure,
            routing_enabled: connection.routing_enabled,
            declared_capabilities: connection.declared_capabilities.clone(),
            credential_state: CredentialState::from_api_key(&connection.api_key),
            created_at: connection.created_at,
            updated_at: connection.updated_at,
        }
    }
}

/// The result of reading one connection row.
///
/// Absence is `Option::None` at the call site; a present row is either
/// [`Readable`](Self::Readable) or [`Unreadable`](Self::Unreadable). The latter
/// means the operator did configure the connection but its ciphertext could not
/// be decrypted (wrong/rotated `ROUTER_SECRETS_KEK`, or corruption); every
/// non-secret field is still surfaced.
#[derive(Debug, Clone)]
pub enum StoredConnection {
    Readable(ProviderConnection),
    Unreadable {
        id: String,
        name: String,
        provider_kind: String,
        prefix: Option<String>,
        base_url: Option<String>,
        exposure: ExposurePolicy,
        routing_enabled: bool,
        declared_capabilities: Option<CapabilityPatch>,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        error: String,
    },
}

impl StoredConnection {
    /// Drop the credential while retaining all fields needed by admin list and
    /// route diagnostics.
    pub fn metadata(&self) -> ConnectionMetadata {
        match self {
            StoredConnection::Readable(connection) => {
                ConnectionMetadata::from_connection(connection)
            }
            StoredConnection::Unreadable {
                id,
                name,
                provider_kind,
                prefix,
                base_url,
                exposure,
                routing_enabled,
                declared_capabilities,
                created_at,
                updated_at,
                ..
            } => ConnectionMetadata {
                id: id.clone(),
                name: name.clone(),
                provider_kind: provider_kind.clone(),
                base_url: base_url.clone(),
                prefix: prefix.clone(),
                exposure: *exposure,
                routing_enabled: *routing_enabled,
                declared_capabilities: declared_capabilities.clone(),
                credential_state: CredentialState::Unreadable,
                created_at: *created_at,
                updated_at: *updated_at,
            },
        }
    }
    pub fn id(&self) -> &str {
        match self {
            StoredConnection::Readable(c) => &c.id,
            StoredConnection::Unreadable { id, .. } => id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            StoredConnection::Readable(c) => &c.name,
            StoredConnection::Unreadable { name, .. } => name,
        }
    }

    pub fn provider_kind(&self) -> &str {
        match self {
            StoredConnection::Readable(c) => &c.provider_kind,
            StoredConnection::Unreadable { provider_kind, .. } => provider_kind,
        }
    }

    pub fn prefix(&self) -> Option<&str> {
        match self {
            StoredConnection::Readable(c) => c.prefix.as_deref(),
            StoredConnection::Unreadable { prefix, .. } => prefix.as_deref(),
        }
    }

    pub fn exposure(&self) -> ExposurePolicy {
        match self {
            StoredConnection::Readable(c) => c.exposure,
            StoredConnection::Unreadable { exposure, .. } => *exposure,
        }
    }

    pub fn base_url(&self) -> Option<&str> {
        match self {
            StoredConnection::Readable(c) => c.base_url.as_deref(),
            StoredConnection::Unreadable { base_url, .. } => base_url.as_deref(),
        }
    }

    pub fn routing_enabled(&self) -> bool {
        match self {
            StoredConnection::Readable(c) => c.routing_enabled,
            StoredConnection::Unreadable {
                routing_enabled, ..
            } => *routing_enabled,
        }
    }

    pub fn declared_capabilities(&self) -> Option<&CapabilityPatch> {
        match self {
            StoredConnection::Readable(c) => c.declared_capabilities.as_ref(),
            StoredConnection::Unreadable {
                declared_capabilities,
                ..
            } => declared_capabilities.as_ref(),
        }
    }
}

/// Fields required to create a connection. `id`/`created_at`/`updated_at` are
/// assigned by the store.
pub struct ConnectionInput {
    pub name: String,
    pub provider_kind: String,
    pub base_url: Option<String>,
    pub prefix: Option<String>,
    pub exposure: ExposurePolicy,
    pub routing_enabled: bool,
    pub declared_capabilities: Option<CapabilityPatch>,
    /// Plaintext key; encrypted at rest before storage. Keyless upstreams pass
    /// [`KEYLESS_PLACEHOLDER`].
    pub api_key: String,
}

/// Partial update; `None` fields are left unchanged. `provider_kind` is not
/// updatable (a connection's kind is fixed at creation). For `base_url`/`prefix`
/// the outer `Some` means "change this field" and the inner `None` means "clear
/// it to NULL".
#[derive(Default)]
pub struct ConnectionUpdate {
    pub name: Option<String>,
    pub base_url: Option<Option<String>>,
    pub prefix: Option<Option<String>>,
    pub exposure: Option<ExposurePolicy>,
    pub routing_enabled: Option<bool>,
    /// Outer `Some` updates the declaration; inner `None` clears it.
    pub declared_capabilities: Option<Option<CapabilityPatch>>,
    /// If set, rotate the stored key (re-encrypted). Omit to leave unchanged.
    pub api_key: Option<String>,
}

/// A store operation failed for a reason other than "row absent" or "row present
/// but undecryptable".
#[derive(Debug)]
pub enum StoreError {
    /// Database / connection error.
    Db(String),
    /// Encrypting the key before write failed.
    Encrypt(String),
    /// The targeted connection does not exist.
    NotFound,
    /// A uniqueness invariant was violated. Carries the offending field token
    /// (`"name"`, `"prefix"`, or `"provider_kind"`).
    Conflict(String),
    /// A persisted capability declaration does not match the strict v1 schema.
    InvalidCapabilityDeclaration {
        id: String,
        name: String,
        error: String,
    },
}

/// Result of deleting a provider connection while checking its model-route
/// references. A missing connection is an idempotent success.
pub enum DeleteConnectionOutcome {
    Deleted { prefix: Option<String> },
    NotFound,
    Referenced { public_model_ids: Vec<String> },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Db(e) => write!(f, "provider connection store database error: {e}"),
            StoreError::Encrypt(e) => write!(f, "provider connection encryption error: {e}"),
            StoreError::NotFound => write!(f, "provider connection not found"),
            StoreError::Conflict(field) => write!(f, "provider connection {field} already in use"),
            StoreError::InvalidCapabilityDeclaration { id, name, error } => write!(
                f,
                "provider connection '{name}' ({id}) has invalid declared_capabilities: {error}"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

/// Map a sqlx error, translating a unique-constraint violation into a
/// [`StoreError::Conflict`] tagged with the offending field; everything else
/// becomes [`StoreError::Db`]. The field is inferred from the index name (see
/// the `uq_router_provider_connections_*` indexes in the migration).
fn map_db_err(e: sqlx::Error) -> StoreError {
    if matches!(
        crate::db_error::classify(&e),
        Some(crate::db_error::DbConstraint::Unique)
    ) {
        let field = match e.as_database_error().and_then(|db| db.constraint()) {
            Some(c) if c.contains("prefix") => "prefix",
            Some(c) if c.contains("kind") => "provider_kind",
            _ => "name",
        };
        return StoreError::Conflict(field.to_string());
    }
    StoreError::Db(e.to_string())
}

#[async_trait]
pub trait ProviderConnectionStore: Send + Sync {
    async fn create(&self, input: ConnectionInput) -> Result<ConnectionMetadata, StoreError>;
    async fn update(
        &self,
        id: &str,
        upd: ConnectionUpdate,
    ) -> Result<ConnectionMetadata, StoreError>;
    async fn delete(&self, id: &str) -> Result<bool, StoreError>;
    /// Atomically delete a connection after checking its soft route references.
    ///
    /// This default performs *no* route check at all — it's only correct for
    /// stores/tests that don't need the reference guard. Every store used in
    /// production overrides it: PostgreSQL locks the connection row and
    /// checks `model_routes` inside the same transaction as the delete;
    /// [`InMemoryProviderConnectionStore::with_routes`] takes
    /// [`crate::service::CONNECTION_ROUTE_MUTATION_LOCK`] and checks the
    /// wired route store before deleting.
    ///
    /// [`crate::service::ProviderConnectionService::delete`] calls this and
    /// nothing else — the store is the single authority for the reference
    /// check, so it is never scanned twice.
    async fn delete_if_unreferenced(
        &self,
        id: &str,
    ) -> Result<DeleteConnectionOutcome, StoreError> {
        let Some(metadata) = self.get_metadata(id).await? else {
            return Ok(DeleteConnectionOutcome::NotFound);
        };
        if self.delete(id).await? {
            Ok(DeleteConnectionOutcome::Deleted {
                prefix: metadata.prefix,
            })
        } else {
            Ok(DeleteConnectionOutcome::NotFound)
        }
    }
    /// Whether [`Self::delete_if_unreferenced`] already checks model-route
    /// references atomically with respect to concurrent route writes (e.g.
    /// PostgreSQL's implementation locks the connection row and checks routes
    /// inside the same transaction as the delete;
    /// [`InMemoryProviderConnectionStore::with_routes`] does the equivalent
    /// under a shared process-global mutex).
    ///
    /// This only exists for callers/tests that care whether the guarantee is
    /// in place; the service layer no longer branches on it.
    fn checks_route_references_atomically(&self) -> bool {
        false
    }
    async fn get(&self, id: &str) -> Result<Option<StoredConnection>, StoreError>;
    async fn list(&self) -> Result<Vec<StoredConnection>, StoreError>;
    /// Read one connection's metadata, using cached credential health where
    /// available.
    async fn get_metadata(&self, id: &str) -> Result<Option<ConnectionMetadata>, StoreError> {
        self.get(id)
            .await
            .map(|stored| stored.map(|connection| connection.metadata()))
    }
    /// List connection metadata, using cached credential health where
    /// available.
    async fn list_metadata(&self) -> Result<Vec<ConnectionMetadata>, StoreError> {
        self.list().await.map(|connections| {
            connections
                .into_iter()
                .map(|connection| connection.metadata())
                .collect()
        })
    }
    /// Cursor page of non-secret metadata, ordered newest-first. PostgreSQL
    /// overrides this so pagination bounds work in the database rather than
    /// loading every encrypted connection into the process.
    async fn list_metadata_page(
        &self,
        cursor: Option<&StablePageCursor>,
        limit: i64,
    ) -> Result<Vec<ConnectionMetadata>, StoreError> {
        let mut connections = self.list_metadata().await?;
        connections.retain(|connection| match cursor {
            None => true,
            Some(cursor) => {
                (connection.created_at, connection.id.as_str())
                    < (cursor.created_at, cursor.id.as_str())
            }
        });
        connections.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        connections.truncate(limit.max(0) as usize);
        Ok(connections)
    }

    /// Resolve metadata for only the IDs referenced by a route page.
    async fn get_metadata_many(
        &self,
        ids: &[String],
    ) -> Result<Vec<ConnectionMetadata>, StoreError> {
        let wanted: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
        Ok(self
            .list_metadata()
            .await?
            .into_iter()
            .filter(|connection| wanted.contains(connection.id.as_str()))
            .collect())
    }
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

/// Ephemeral store (tests / no-DB deployments). Never persisted, so rows are
/// always [`StoredConnection::Readable`]. Replicates the DB's uniqueness
/// invariants (unique name and non-null prefix) so behavior matches
/// Postgres.
#[derive(Clone, Default)]
pub struct InMemoryProviderConnectionStore {
    conns: Arc<DashMap<String, ProviderConnection>>,
    /// When set, [`Self::delete_if_unreferenced`] checks model-route
    /// references itself (under [`crate::service::CONNECTION_ROUTE_MUTATION_LOCK`],
    /// the same lock `InMemoryModelRouteStore::create`/`update` take) instead
    /// of requiring the service layer to pre-scan routes on every delete.
    /// Left unset in tests that don't exercise delete-time reference
    /// checking, which then fall back to the unchecked default delete.
    routes: Option<Arc<dyn ModelRouteStore>>,
}

impl InMemoryProviderConnectionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a store whose `delete_if_unreferenced` checks `routes` itself,
    /// matching PostgreSQL's atomic (single-scan) reference check.
    pub fn with_routes(routes: Arc<dyn ModelRouteStore>) -> Self {
        Self {
            conns: Arc::new(DashMap::new()),
            routes: Some(routes),
        }
    }
}

#[async_trait]
impl ProviderConnectionStore for InMemoryProviderConnectionStore {
    fn checks_route_references_atomically(&self) -> bool {
        self.routes.is_some()
    }

    /// When `routes` is wired, this is the *only* route scan performed for an
    /// in-memory delete: the service layer no longer pre-scans (see
    /// [`crate::service::ProviderConnectionService::delete`]). Holding
    /// [`crate::service::CONNECTION_ROUTE_MUTATION_LOCK`] across the
    /// check-then-delete keeps it atomic with respect to concurrent
    /// `InMemoryModelRouteStore::create`/`update`, which take the same lock.
    async fn delete_if_unreferenced(
        &self,
        id: &str,
    ) -> Result<DeleteConnectionOutcome, StoreError> {
        let Some(routes) = &self.routes else {
            let Some(metadata) = self.get_metadata(id).await? else {
                return Ok(DeleteConnectionOutcome::NotFound);
            };
            return if self.delete(id).await? {
                Ok(DeleteConnectionOutcome::Deleted {
                    prefix: metadata.prefix,
                })
            } else {
                Ok(DeleteConnectionOutcome::NotFound)
            };
        };

        let _guard = crate::service::CONNECTION_ROUTE_MUTATION_LOCK.lock().await;
        let Some(metadata) = self.get_metadata(id).await? else {
            return Ok(DeleteConnectionOutcome::NotFound);
        };
        let referencing = routes
            .list_by_connection(id)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        if !referencing.is_empty() {
            return Ok(DeleteConnectionOutcome::Referenced {
                public_model_ids: referencing
                    .into_iter()
                    .map(|route| route.public_model_id)
                    .collect(),
            });
        }
        if self.delete(id).await? {
            Ok(DeleteConnectionOutcome::Deleted {
                prefix: metadata.prefix,
            })
        } else {
            Ok(DeleteConnectionOutcome::NotFound)
        }
    }

    async fn create(&self, input: ConnectionInput) -> Result<ConnectionMetadata, StoreError> {
        for entry in self.conns.iter() {
            if entry.name == input.name {
                return Err(StoreError::Conflict("name".to_string()));
            }
            if let (Some(existing), Some(incoming)) =
                (entry.prefix.as_deref(), input.prefix.as_deref())
            {
                if existing == incoming {
                    return Err(StoreError::Conflict("prefix".to_string()));
                }
            }
        }

        let now = Utc::now();
        let conn = ProviderConnection {
            id: Uuid::new_v4().to_string(),
            name: input.name,
            provider_kind: input.provider_kind,
            base_url: input.base_url,
            prefix: input.prefix,
            exposure: input.exposure,
            routing_enabled: input.routing_enabled,
            declared_capabilities: input.declared_capabilities,
            api_key: input.api_key,
            created_at: now,
            updated_at: now,
        };
        self.conns.insert(conn.id.clone(), conn.clone());
        Ok(ConnectionMetadata::from_connection(&conn))
    }

    async fn update(
        &self,
        id: &str,
        upd: ConnectionUpdate,
    ) -> Result<ConnectionMetadata, StoreError> {
        // Clone the current row out first so we don't hold a map guard while
        // scanning for conflicts (avoids a self-deadlock on DashMap).
        let mut current = match self.conns.get(id) {
            Some(c) => c.clone(),
            None => return Err(StoreError::NotFound),
        };

        let new_name = upd.name.clone().unwrap_or_else(|| current.name.clone());
        let new_prefix = match &upd.prefix {
            Some(p) => p.clone(),
            None => current.prefix.clone(),
        };
        for entry in self.conns.iter() {
            if entry.key().as_str() == id {
                continue;
            }
            if entry.name == new_name {
                return Err(StoreError::Conflict("name".to_string()));
            }
            if let (Some(existing), Some(incoming)) =
                (entry.prefix.as_deref(), new_prefix.as_deref())
            {
                if existing == incoming {
                    return Err(StoreError::Conflict("prefix".to_string()));
                }
            }
        }

        if let Some(name) = upd.name {
            current.name = name;
        }
        if let Some(base_url) = upd.base_url {
            current.base_url = base_url;
        }
        if let Some(prefix) = upd.prefix {
            current.prefix = prefix;
        }
        if let Some(exposure) = upd.exposure {
            current.exposure = exposure;
        }
        if let Some(routing_enabled) = upd.routing_enabled {
            current.routing_enabled = routing_enabled;
        }
        if let Some(declared_capabilities) = upd.declared_capabilities {
            current.declared_capabilities = declared_capabilities;
        }
        if let Some(api_key) = upd.api_key {
            current.api_key = api_key;
        }
        current.updated_at = Utc::now();
        self.conns.insert(id.to_string(), current.clone());
        Ok(ConnectionMetadata::from_connection(&current))
    }

    async fn delete(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.conns.remove(id).is_some())
    }

    async fn get(&self, id: &str) -> Result<Option<StoredConnection>, StoreError> {
        Ok(self
            .conns
            .get(id)
            .map(|c| StoredConnection::Readable(c.clone())))
    }

    async fn list(&self) -> Result<Vec<StoredConnection>, StoreError> {
        let mut out: Vec<StoredConnection> = self
            .conns
            .iter()
            .map(|c| StoredConnection::Readable(c.clone()))
            .collect();
        out.sort_by_key(|c| match c {
            StoredConnection::Readable(c) => c.created_at,
            StoredConnection::Unreadable { created_at, .. } => *created_at,
        });
        Ok(out)
    }

    async fn get_metadata(&self, id: &str) -> Result<Option<ConnectionMetadata>, StoreError> {
        Ok(self
            .conns
            .get(id)
            .map(|connection| ConnectionMetadata::from_connection(&connection)))
    }

    async fn list_metadata(&self) -> Result<Vec<ConnectionMetadata>, StoreError> {
        let mut connections: Vec<_> = self
            .conns
            .iter()
            .map(|connection| ConnectionMetadata::from_connection(&connection))
            .collect();
        connections.sort_by_key(|connection| connection.created_at);
        Ok(connections)
    }

    async fn get_metadata_many(
        &self,
        ids: &[String],
    ) -> Result<Vec<ConnectionMetadata>, StoreError> {
        Ok(ids
            .iter()
            .filter_map(|id| self.conns.get(id))
            .map(|connection| ConnectionMetadata::from_connection(&connection))
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Postgres implementation
// ---------------------------------------------------------------------------

const SELECT_COLUMNS: &str = "SELECT id, name, provider_kind, base_url, prefix, exposure_policy, \
     routing_enabled, declared_capabilities, api_key_envelope, created_at, updated_at \
     FROM chalk_router.provider_connections";
const SELECT_METADATA_COLUMNS: &str =
    "SELECT id, name, provider_kind, base_url, prefix, exposure_policy, \
     routing_enabled, declared_capabilities, created_at, updated_at \
     FROM chalk_router.provider_connections";

#[derive(Clone)]
pub struct PostgresProviderConnectionStore {
    pool: sqlx::PgPool,
    /// Encrypts the API key at rest / decrypts it on read.
    cipher: SecretCipher,
    /// Tenancy scope (see `PostgresApiKeyStore`).
    environment_id: String,
    /// Credential health keyed by connection id and invalidated by updated_at.
    /// Metadata-only list calls stay secret-free after the first observation
    /// while still preserving unreadable/keyless semantics.
    credential_states: Arc<DashMap<String, CachedCredentialState>>,
}

#[derive(Clone)]
struct CachedCredentialState {
    updated_at: DateTime<Utc>,
    state: CredentialState,
}

fn decode_declared_capabilities(
    row: &sqlx::postgres::PgRow,
    id: &str,
    name: &str,
) -> Result<Option<CapabilityPatch>, StoreError> {
    use sqlx::Row;
    match row.try_get::<Option<sqlx::types::Json<CapabilityPatch>>, _>("declared_capabilities") {
        Ok(value) => value
            .map(|value| {
                value.0.validate().map(|()| value.0).map_err(|error| {
                    StoreError::InvalidCapabilityDeclaration {
                        id: id.to_string(),
                        name: name.to_string(),
                        error: error.to_string(),
                    }
                })
            })
            .transpose(),
        Err(error) => Err(StoreError::InvalidCapabilityDeclaration {
            id: id.to_string(),
            name: name.to_string(),
            error: error.to_string(),
        }),
    }
}

impl PostgresProviderConnectionStore {
    /// Wrap an existing pool. Tables are provisioned by migrations before boot.
    pub fn new(pool: sqlx::PgPool, cipher: SecretCipher, environment_id: String) -> Self {
        Self {
            pool,
            cipher,
            environment_id,
            credential_states: Arc::new(DashMap::new()),
        }
    }

    fn encrypt_verified(&self, api_key: &str) -> Result<Vec<u8>, StoreError> {
        let envelope = self
            .cipher
            .encrypt(api_key.as_bytes())
            .map_err(StoreError::Encrypt)?;
        let decrypted = self
            .cipher
            .decrypt(&envelope)
            .map_err(|error| StoreError::Encrypt(format!("round-trip verification: {error}")))?;
        if decrypted != api_key.as_bytes() {
            return Err(StoreError::Encrypt(
                "round-trip verification returned different plaintext".to_string(),
            ));
        }
        Ok(envelope)
    }

    /// Decrypt one row into a [`StoredConnection`], isolating decrypt failures so
    /// a single poison row never aborts a `list()`.
    fn row_to_stored(&self, row: &sqlx::postgres::PgRow) -> Result<StoredConnection, StoreError> {
        use sqlx::Row;
        let id: String = row.get("id");
        let name: String = row.get("name");
        let provider_kind: String = row.get("provider_kind");
        let base_url: Option<String> = row.get("base_url");
        let prefix: Option<String> = row.get("prefix");
        let exposure_raw: String = row.get("exposure_policy");
        let routing_enabled: bool = row.get("routing_enabled");
        let declared_capabilities = decode_declared_capabilities(row, &id, &name)?;
        let created_at: DateTime<Utc> = row.get("created_at");
        let updated_at: DateTime<Utc> = row.get("updated_at");
        let envelope: Vec<u8> = row.get("api_key_envelope");

        // The DB CHECK constraint guarantees a valid value; fall back defensively.
        let exposure = ExposurePolicy::parse(&exposure_raw).unwrap_or_else(|| {
            tracing::warn!(%id, exposure = %exposure_raw, "unknown exposure_policy; defaulting to dynamic");
            ExposurePolicy::Dynamic
        });

        let unreadable = |error: String| StoredConnection::Unreadable {
            id: id.clone(),
            name: name.clone(),
            provider_kind: provider_kind.clone(),
            prefix: prefix.clone(),
            base_url: base_url.clone(),
            exposure,
            routing_enabled,
            declared_capabilities: declared_capabilities.clone(),
            created_at,
            updated_at,
            error,
        };

        let stored = match self.cipher.decrypt(&envelope) {
            Ok(plaintext) => match String::from_utf8(plaintext) {
                Ok(api_key) => StoredConnection::Readable(ProviderConnection {
                    id,
                    name,
                    provider_kind,
                    base_url,
                    prefix,
                    exposure,
                    routing_enabled,
                    declared_capabilities,
                    api_key,
                    created_at,
                    updated_at,
                }),
                Err(_) => {
                    tracing::warn!(%id, "decrypted provider connection key is not valid UTF-8");
                    unreadable("decrypted value is not valid UTF-8".to_string())
                }
            },
            Err(e) => {
                tracing::warn!(%id, error = %e, "failed to decrypt provider connection key (check ROUTER_SECRETS_KEK)");
                unreadable(e.to_string())
            }
        };
        let metadata = stored.metadata();
        self.credential_states.insert(
            metadata.id.clone(),
            CachedCredentialState {
                updated_at: metadata.updated_at,
                state: metadata.credential_state,
            },
        );
        Ok(stored)
    }

    fn row_to_metadata(row: &sqlx::postgres::PgRow) -> Result<ConnectionMetadata, StoreError> {
        use sqlx::Row;
        let id: String = row.get("id");
        let exposure_raw: String = row.get("exposure_policy");
        let exposure = ExposurePolicy::parse(&exposure_raw).unwrap_or_else(|| {
            tracing::warn!(%id, exposure = %exposure_raw, "unknown exposure_policy; defaulting to dynamic");
            ExposurePolicy::Dynamic
        });
        let name: String = row.get("name");
        let declared_capabilities = decode_declared_capabilities(row, &id, &name)?;
        Ok(ConnectionMetadata {
            id,
            name,
            provider_kind: row.get("provider_kind"),
            base_url: row.get("base_url"),
            prefix: row.get("prefix"),
            exposure,
            routing_enabled: row.get("routing_enabled"),
            declared_capabilities,
            credential_state: CredentialState::Unknown,
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
    }

    /// Refreshes each connection's cached decrypt-health, then reports on it:
    /// an `error`-level log if every connection just checked failed to
    /// decrypt (all four metadata read paths call this, so a KEK rotation is
    /// surfaced regardless of which one an operator happens to be using), or
    /// a `warn`-level log for any newly-observed failures otherwise. A
    /// single-connection or small-subset caller reporting "every" connection
    /// unreadable is still meaningful signal — it means every connection this
    /// call actually looked at failed the same way, which is exactly the
    /// KEK-rotation symptom.
    async fn hydrate_credential_states(
        &self,
        mut metadata: Vec<ConnectionMetadata>,
    ) -> Result<Vec<ConnectionMetadata>, StoreError> {
        use sqlx::Row;

        let missing: Vec<String> = metadata
            .iter()
            .filter(|connection| {
                self.credential_states
                    .get(&connection.id)
                    .map(|state| state.updated_at != connection.updated_at)
                    .unwrap_or(true)
            })
            .map(|connection| connection.id.clone())
            .collect();

        let mut newly_unreadable = 0usize;
        if !missing.is_empty() {
            let rows = sqlx::query(
                "SELECT id, api_key_envelope, updated_at \
                 FROM chalk_router.provider_connections \
                 WHERE environment_id = $1 AND id = ANY($2)",
            )
            .bind(&self.environment_id)
            .bind(&missing)
            .fetch_all(&self.pool)
            .await
            .map_err(map_db_err)?;

            for row in rows {
                let id: String = row.get("id");
                let envelope: Vec<u8> = row.get("api_key_envelope");
                let updated_at: DateTime<Utc> = row.get("updated_at");
                let state = match self.cipher.decrypt(&envelope) {
                    Ok(plaintext) => match String::from_utf8(plaintext) {
                        Ok(api_key) => CredentialState::from_api_key(&api_key),
                        Err(_) => CredentialState::Unreadable,
                    },
                    Err(_) => CredentialState::Unreadable,
                };
                newly_unreadable += usize::from(state.unreadable());
                self.credential_states
                    .insert(id, CachedCredentialState { updated_at, state });
            }
        }

        for connection in &mut metadata {
            if let Some(state) = self.credential_states.get(&connection.id) {
                connection.credential_state = state.state;
            }
        }
        let unreadable_count = metadata
            .iter()
            .filter(|connection| connection.credential_state.unreadable())
            .count();
        if !metadata.is_empty() && unreadable_count == metadata.len() {
            tracing::error!(
                count = unreadable_count,
                "every stored provider connection failed to decrypt; ROUTER_SECRETS_KEK is likely wrong or rotated"
            );
        } else if newly_unreadable > 0 {
            tracing::warn!(
                count = newly_unreadable,
                "provider credential envelopes could not be decrypted; check ROUTER_SECRETS_KEK"
            );
        }
        Ok(metadata)
    }
}

#[async_trait]
impl ProviderConnectionStore for PostgresProviderConnectionStore {
    fn checks_route_references_atomically(&self) -> bool {
        // delete_if_unreferenced below locks the connection row and checks
        // model_routes inside the same transaction as the delete.
        true
    }

    async fn create(&self, input: ConnectionInput) -> Result<ConnectionMetadata, StoreError> {
        let credential_state = CredentialState::from_api_key(&input.api_key);
        let envelope = self.encrypt_verified(&input.api_key)?;
        // The app mints the id (a bare uuid string); created_at / updated_at are
        // supplied by column defaults.
        let id = Uuid::new_v4().to_string();
        let declared_capabilities = input.declared_capabilities.clone().map(sqlx::types::Json);
        let row = sqlx::query(
            "INSERT INTO chalk_router.provider_connections \
             (environment_id, id, name, provider_kind, base_url, prefix, exposure_policy, \
              routing_enabled, declared_capabilities, api_key_envelope) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             RETURNING id, name, provider_kind, base_url, prefix, exposure_policy, \
                       routing_enabled, declared_capabilities, created_at, updated_at",
        )
        .bind(&self.environment_id)
        .bind(&id)
        .bind(&input.name)
        .bind(&input.provider_kind)
        .bind(input.base_url.as_deref())
        .bind(input.prefix.as_deref())
        .bind(input.exposure.as_str())
        .bind(input.routing_enabled)
        .bind(declared_capabilities)
        .bind(&envelope)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;

        let mut connection = Self::row_to_metadata(&row)?;
        connection.credential_state = credential_state;
        self.credential_states.insert(
            connection.id.clone(),
            CachedCredentialState {
                updated_at: connection.updated_at,
                state: credential_state,
            },
        );
        Ok(connection)
    }

    async fn update(
        &self,
        id: &str,
        upd: ConnectionUpdate,
    ) -> Result<ConnectionMetadata, StoreError> {
        let cached_credential_state = self.credential_states.get(id).map(|state| state.state);
        let credential_state = match upd.api_key.as_deref() {
            Some(api_key) => CredentialState::from_api_key(api_key),
            None => match cached_credential_state {
                Some(state) => state,
                None => {
                    self.get_metadata(id)
                        .await?
                        .ok_or(StoreError::NotFound)?
                        .credential_state
                }
            },
        };

        // Encrypt up-front so the ciphertext bytes are available to bind below.
        let encrypted = match &upd.api_key {
            Some(key) => Some(self.encrypt_verified(key)?),
            None => None,
        };

        // Build the SET list dynamically; bind order below must match this order.
        // $1 = id, $2 = environment_id.
        let mut sets: Vec<String> = vec!["updated_at = now()".to_string()];
        let mut idx = 3u32;
        if upd.name.is_some() {
            sets.push(format!("name = ${idx}"));
            idx += 1;
        }
        if upd.base_url.is_some() {
            sets.push(format!("base_url = ${idx}"));
            idx += 1;
        }
        if upd.prefix.is_some() {
            sets.push(format!("prefix = ${idx}"));
            idx += 1;
        }
        if upd.exposure.is_some() {
            sets.push(format!("exposure_policy = ${idx}"));
            idx += 1;
        }
        if upd.routing_enabled.is_some() {
            sets.push(format!("routing_enabled = ${idx}"));
            idx += 1;
        }
        if upd.declared_capabilities.is_some() {
            sets.push(format!("declared_capabilities = ${idx}"));
            idx += 1;
        }
        if encrypted.is_some() {
            sets.push(format!("api_key_envelope = ${idx}"));
        }

        let sql = format!(
            "UPDATE chalk_router.provider_connections SET {} \
             WHERE id = $1 AND environment_id = $2 \
             RETURNING id, name, provider_kind, base_url, prefix, exposure_policy, \
                       routing_enabled, declared_capabilities, created_at, updated_at",
            sets.join(", ")
        );

        // `sets` contains only the fixed column assignments selected above.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .bind(&self.environment_id);
        if let Some(ref name) = upd.name {
            query = query.bind(name);
        }
        if let Some(ref base_url) = upd.base_url {
            query = query.bind(base_url.as_deref());
        }
        if let Some(ref prefix) = upd.prefix {
            query = query.bind(prefix.as_deref());
        }
        if let Some(exposure) = upd.exposure {
            query = query.bind(exposure.as_str());
        }
        if let Some(routing_enabled) = upd.routing_enabled {
            query = query.bind(routing_enabled);
        }
        if let Some(ref declared_capabilities) = upd.declared_capabilities {
            query = query.bind(declared_capabilities.clone().map(sqlx::types::Json));
        }
        if let Some(envelope) = encrypted.as_ref() {
            query = query.bind(envelope);
        }

        let row = query.fetch_optional(&self.pool).await.map_err(map_db_err)?;
        let row = row.ok_or(StoreError::NotFound)?;
        let mut connection = Self::row_to_metadata(&row)?;
        connection.credential_state = credential_state;
        self.credential_states.insert(
            connection.id.clone(),
            CachedCredentialState {
                updated_at: connection.updated_at,
                state: credential_state,
            },
        );
        Ok(connection)
    }

    async fn delete(&self, id: &str) -> Result<bool, StoreError> {
        let res = sqlx::query(
            "DELETE FROM chalk_router.provider_connections WHERE id = $1 AND environment_id = $2",
        )
        .bind(id)
        .bind(&self.environment_id)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;
        self.credential_states.remove(id);
        Ok(res.rows_affected() > 0)
    }

    async fn delete_if_unreferenced(
        &self,
        id: &str,
    ) -> Result<DeleteConnectionOutcome, StoreError> {
        use sqlx::Row;

        // Route writes take a FOR KEY SHARE lock on the same connection row.
        // Holding FOR UPDATE from the reference check through DELETE ensures a
        // route cannot be committed between those two operations.
        let mut tx = self.pool.begin().await.map_err(map_db_err)?;
        let connection = sqlx::query(
            "SELECT prefix FROM chalk_router.provider_connections \
             WHERE environment_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(&self.environment_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db_err)?;
        let Some(connection) = connection else {
            return Ok(DeleteConnectionOutcome::NotFound);
        };

        let routes = sqlx::query(
            "SELECT public_model_id FROM chalk_router.model_routes \
             WHERE environment_id = $1 AND COALESCE(routing->'default_target'->>'connection_id', connection_id) = $2 ORDER BY created_at ASC",
        )
        .bind(&self.environment_id)
        .bind(id)
        .fetch_all(&mut *tx)
        .await
        .map_err(map_db_err)?;
        if !routes.is_empty() {
            return Ok(DeleteConnectionOutcome::Referenced {
                public_model_ids: routes
                    .iter()
                    .map(|route| route.get("public_model_id"))
                    .collect(),
            });
        }

        sqlx::query(
            "DELETE FROM chalk_router.provider_connections WHERE id = $1 AND environment_id = $2",
        )
        .bind(id)
        .bind(&self.environment_id)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;
        tx.commit().await.map_err(map_db_err)?;
        self.credential_states.remove(id);
        Ok(DeleteConnectionOutcome::Deleted {
            prefix: connection.get("prefix"),
        })
    }

    async fn get(&self, id: &str) -> Result<Option<StoredConnection>, StoreError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{SELECT_COLUMNS} WHERE environment_id = $1 AND id = $2"
        )))
        .bind(&self.environment_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db_err)?;
        row.map(|row| self.row_to_stored(&row)).transpose()
    }

    async fn list(&self) -> Result<Vec<StoredConnection>, StoreError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{SELECT_COLUMNS} WHERE environment_id = $1 ORDER BY created_at ASC"
        )))
        .bind(&self.environment_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_db_err)?;

        let out: Vec<StoredConnection> = rows
            .iter()
            .map(|row| self.row_to_stored(row))
            .collect::<Result<_, _>>()?;

        // Fail loud only on total failure: rows exist but none decrypted is a
        // systemic misconfiguration (wrong KEK), not one corrupt row.
        if !out.is_empty()
            && out
                .iter()
                .all(|c| matches!(c, StoredConnection::Unreadable { .. }))
        {
            tracing::error!(
                count = out.len(),
                "every stored provider connection failed to decrypt; ROUTER_SECRETS_KEK is likely wrong or rotated"
            );
        }
        Ok(out)
    }

    async fn get_metadata(&self, id: &str) -> Result<Option<ConnectionMetadata>, StoreError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{SELECT_METADATA_COLUMNS} WHERE environment_id = $1 AND id = $2"
        )))
        .bind(&self.environment_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db_err)?;
        let metadata = row
            .map(|row| Self::row_to_metadata(&row))
            .transpose()?
            .into_iter()
            .collect();
        Ok(self.hydrate_credential_states(metadata).await?.pop())
    }

    async fn list_metadata(&self) -> Result<Vec<ConnectionMetadata>, StoreError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{SELECT_METADATA_COLUMNS} WHERE environment_id = $1 ORDER BY created_at ASC"
        )))
        .bind(&self.environment_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_db_err)?;
        let metadata = rows
            .iter()
            .map(Self::row_to_metadata)
            .collect::<Result<_, _>>()?;
        self.hydrate_credential_states(metadata).await
    }

    async fn list_metadata_page(
        &self,
        cursor: Option<&StablePageCursor>,
        limit: i64,
    ) -> Result<Vec<ConnectionMetadata>, StoreError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{SELECT_METADATA_COLUMNS} \
             WHERE environment_id = $1 \
               AND ($2::timestamptz IS NULL OR (created_at, id) < ($2, $3)) \
             ORDER BY created_at DESC, id DESC LIMIT $4"
        )))
        .bind(&self.environment_id)
        .bind(cursor.map(|cursor| cursor.created_at))
        .bind(cursor.map(|cursor| cursor.id.as_str()))
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await
        .map_err(map_db_err)?;
        let metadata = rows
            .iter()
            .map(Self::row_to_metadata)
            .collect::<Result<_, _>>()?;
        self.hydrate_credential_states(metadata).await
    }

    async fn get_metadata_many(
        &self,
        ids: &[String],
    ) -> Result<Vec<ConnectionMetadata>, StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{SELECT_METADATA_COLUMNS} WHERE environment_id = $1 AND id = ANY($2)"
        )))
        .bind(&self.environment_id)
        .bind(ids)
        .fetch_all(&self.pool)
        .await
        .map_err(map_db_err)?;
        let metadata = rows
            .iter()
            .map(Self::row_to_metadata)
            .collect::<Result<_, _>>()?;
        self.hydrate_credential_states(metadata).await
    }
}

#[cfg(test)]
mod connection_policy_tests {
    use super::*;

    const RUNTIME: &str = "https://bedrock-runtime.us-west-2.amazonaws.com";

    #[test]
    fn isolated_policy_rejects_only_keyless_bedrock_runtime() {
        let isolated = ConnectionPolicy::isolated();
        for key in ["", "  ", KEYLESS_PLACEHOLDER] {
            assert!(isolated
                .check_credentials("bedrock", Some(RUNTIME), key)
                .is_err());
        }
        assert!(isolated
            .check("bedrock", Some(RUNTIME), "bearer-key")
            .is_ok());
        // Other keyless kinds do not borrow the process identity.
        assert!(isolated
            .check_credentials("vllm", Some("https://vllm.example"), "")
            .is_ok());
        assert!(isolated.check_credentials("bedrock", None, "").is_ok());
        assert!(ConnectionPolicy::trusted()
            .check("bedrock", Some(RUNTIME), "")
            .is_ok());
    }

    #[test]
    fn isolated_policy_allows_public_endpoints_only() {
        let isolated = ConnectionPolicy::isolated();
        assert!(isolated.check_endpoint(None).is_ok());
        assert!(isolated.check_endpoint(Some("  ")).is_ok());
        assert!(isolated
            .check_endpoint(Some("https://llm.example.com/v1"))
            .is_ok());
        assert!(isolated.check_endpoint(Some(RUNTIME)).is_ok());
        for url in [
            "http://169.254.169.254/v1",
            "http://127.0.0.1:8000/v1",
            "http://10.1.2.3/v1",
            "http://localhost/v1",
            "http://vllm.default.svc.cluster.local/v1",
            "file:///etc/passwd",
        ] {
            assert!(isolated.check_endpoint(Some(url)).is_err(), "{url}");
        }
        // The adapter-time check covers endpoints as well as credentials.
        assert!(isolated
            .check("openai", Some("http://169.254.169.254/v1"), "k")
            .is_err());
        // An operator-controlled environment may target private hosts.
        let trusted = ConnectionPolicy::trusted();
        assert!(trusted.check_endpoint(Some("http://10.1.2.3/v1")).is_ok());
        assert!(trusted
            .check("vllm", Some("http://10.1.2.3/v1"), "")
            .is_ok());
        assert!(!trusted.public_egress_only());
        assert!(isolated.public_egress_only());
    }

    #[test]
    fn isolated_policy_refuses_a_vertex_key_that_names_a_private_token_uri() {
        let key = |token_uri: &str| {
            format!(r#"{{"client_email":"a@b","private_key":"k","token_uri":"{token_uri}"}}"#)
        };
        let isolated = ConnectionPolicy::isolated();
        for uri in ["http://169.254.169.254/token", "http://127.0.0.1:9/token"] {
            assert!(
                isolated
                    .check_credentials("vertex", None, &key(uri))
                    .is_err(),
                "{uri}"
            );
        }
        assert!(isolated
            .check_credentials("vertex", None, &key("https://oauth2.googleapis.com/token"))
            .is_ok());
        // The backend also accepts the JSON base64-encoded, and user credentials.
        {
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD
                .encode(key("http://169.254.169.254/token"));
            assert!(isolated
                .check_credentials("vertex", None, &encoded)
                .is_err());
        }
        let user = r#"{"type":"authorized_user","client_id":"i","client_secret":"s","refresh_token":"r","token_uri":"http://10.0.0.5/token"}"#;
        assert!(isolated.check_credentials("vertex", None, user).is_err());
        // Keys that do not name one, and other kinds, are unaffected.
        assert!(isolated
            .check_credentials("vertex", None, r#"{"client_email":"a@b"}"#)
            .is_ok());
        assert!(isolated
            .check_credentials("openai", None, &key("http://127.0.0.1/"))
            .is_ok());
        assert!(ConnectionPolicy::trusted()
            .check_credentials("vertex", None, &key("http://127.0.0.1:9/token"))
            .is_ok());
    }

    #[test]
    fn cluster_trusts_only_its_legacy_environment() {
        let mut config = crate::config::RouterConfig::from_env();
        config.legacy_environment_id = Some("legacy".into());
        assert_eq!(
            ConnectionPolicy::for_environment(&config, "tenant"),
            ConnectionPolicy::trusted(),
            "an environment-mode router serves one operator-owned environment"
        );
        config.cluster = Some(crate::config::ClusterConfig {
            metadata_api_endpoint: "http://metadata".into(),
            metadata_api_token: "token".into(),
            kube_cluster_id: None,
        });
        assert_eq!(
            ConnectionPolicy::for_environment(&config, "legacy"),
            ConnectionPolicy::trusted()
        );
        assert_eq!(
            ConnectionPolicy::for_environment(&config, "tenant"),
            ConnectionPolicy::isolated()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_registry::OPENAI_COMPATIBLE_KIND;

    fn input(name: &str, kind: &str, prefix: Option<&str>) -> ConnectionInput {
        ConnectionInput {
            name: name.to_string(),
            provider_kind: kind.to_string(),
            base_url: None,
            prefix: prefix.map(|s| s.to_string()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "test-key".to_string(),
        }
    }

    #[tokio::test]
    async fn create_and_get_roundtrips() {
        let store = InMemoryProviderConnectionStore::new();
        let created = store
            .create(input("OpenAI", "openai", Some("openai/")))
            .await
            .unwrap();
        let got = store.get(&created.id).await.unwrap().unwrap();
        assert_eq!(got.id(), created.id.as_str());
        assert_eq!(got.provider_kind(), "openai");
        assert_eq!(got.prefix(), Some("openai/"));
    }

    #[tokio::test]
    async fn capability_declaration_roundtrips_and_can_be_cleared() {
        let store = InMemoryProviderConnectionStore::new();
        let mut input = input("custom", OPENAI_COMPATIBLE_KIND, Some("custom/"));
        input.declared_capabilities = Some(
            serde_json::from_value(serde_json::json!({
                "schema_version": 1,
                "model_defaults": {
                    "features": {"tool_calling": "supported"}
                }
            }))
            .unwrap(),
        );
        let created = store.create(input).await.unwrap();
        assert!(store
            .get(&created.id)
            .await
            .unwrap()
            .unwrap()
            .declared_capabilities()
            .is_some());

        store
            .update(
                &created.id,
                ConnectionUpdate {
                    declared_capabilities: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(store
            .get(&created.id)
            .await
            .unwrap()
            .unwrap()
            .declared_capabilities()
            .is_none());
    }

    #[tokio::test]
    async fn duplicate_name_and_prefix_conflict_but_same_kind_is_allowed() {
        let store = InMemoryProviderConnectionStore::new();
        store
            .create(input("OpenAI", "openai", Some("openai/")))
            .await
            .unwrap();

        // Duplicate name.
        let e = store
            .create(input("OpenAI", "anthropic", Some("anthropic/")))
            .await
            .unwrap_err();
        assert!(matches!(e, StoreError::Conflict(f) if f == "name"));

        // Duplicate prefix.
        let e = store
            .create(input("Other", "anthropic", Some("openai/")))
            .await
            .unwrap_err();
        assert!(matches!(e, StoreError::Conflict(f) if f == "prefix"));

        // A second connection for the same built-in kind is valid when its
        // name and prefix are distinct.
        store
            .create(input("OpenAI 2", "openai", Some("openai2/")))
            .await
            .unwrap();

        // The generic OpenAI-compatible pseudo-kind follows the same rule.
        store
            .create(input("compat-a", OPENAI_COMPATIBLE_KIND, Some("a/")))
            .await
            .unwrap();
        store
            .create(input("compat-b", OPENAI_COMPATIBLE_KIND, Some("b/")))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn update_clears_and_rotates() {
        let store = InMemoryProviderConnectionStore::new();
        let created = store
            .create(input("compat", OPENAI_COMPATIBLE_KIND, Some("compat/")))
            .await
            .unwrap();

        let upd = ConnectionUpdate {
            prefix: Some(None), // clear
            api_key: Some("rotated".to_string()),
            routing_enabled: Some(false),
            ..Default::default()
        };
        let updated = store.update(&created.id, upd).await.unwrap();
        assert_eq!(updated.prefix, None);
        assert_eq!(updated.credential_state, CredentialState::Present);
        assert!(!updated.routing_enabled);
        let stored = store.get(&created.id).await.unwrap().unwrap();
        assert!(
            matches!(stored, StoredConnection::Readable(connection) if connection.api_key == "rotated")
        );
    }

    #[tokio::test]
    async fn update_missing_is_not_found() {
        let store = InMemoryProviderConnectionStore::new();
        let e = store
            .update(&Uuid::new_v4().to_string(), ConnectionUpdate::default())
            .await
            .unwrap_err();
        assert!(matches!(e, StoreError::NotFound));
    }
}
