use async_trait::async_trait;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use futures::future::{join_all, BoxFuture, Shared};
use futures::FutureExt;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::provider::{
    BackendError, BackendProvider, ConnectionModel, ResolvedRoute, RouteResolution, SseByteStream,
};
use super::{
    anthropic_backend, bedrock_backend, cohere_backend, fireworks_backend, gemini_backend,
    groq_backend, mistral_backend, openai_backend, openrouter_backend, perplexity_backend,
    qwen_backend, together_backend, vertex_backend, vllm_backend, xai_backend,
    BedrockRuntimeBackend, OpenAICompatibleBackend, VoyageBackend,
};
use crate::model_routes::{ModelRoute, ModelRouteStore};
use crate::provider_connections::{
    ExposurePolicy, ProviderConnection, ProviderConnectionStore, StoredConnection,
};
use crate::provider_registry::{
    canonical_prefix, is_builtin, strip_route_prefix, OPENAI_COMPATIBLE_KIND, ROUTE_RESERVED_PREFIX,
};
use crate::types::{
    ChatCompletionRequest, ChatCompletionResponse, CreateEmbeddingRequest, CreateEmbeddingResponse,
    CreateImageRequest, CreateModerationRequest, CreateModerationResponse, CreateSpeechRequest,
    ImageResponse, ListResponse, Model, TranscriptionResponse,
};

/// Per-upstream timeout for `/models` so one slow provider can't stall it.
pub const LIST_MODELS_TIMEOUT: Duration = Duration::from_secs(30);

/// Fetch a backend's models and apply its provider prefix, bounded by [`LIST_MODELS_TIMEOUT`].
async fn list_prefixed_models(
    backend: &dyn BackendProvider,
    prefix: &str,
) -> Result<PrefixedModelList, BackendError> {
    let resp = match tokio::time::timeout(LIST_MODELS_TIMEOUT, backend.list_models()).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(BackendError::api_status(
                504,
                format!("Timed out listing models for provider '{prefix}'"),
            ))
        }
    };
    Ok(PrefixedModelList {
        models: resp
            .data
            .into_iter()
            .map(|model| Model {
                id: format!("{prefix}/{}", model.id),
                ..model
            })
            .collect(),
        has_more: resp.has_more,
    })
}

/// How long a provider's listed models stay fresh before we refetch.
const MODELS_CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Default)]
struct PrefixedModelList {
    models: Vec<Model>,
    /// True when the provider stopped at a local or upstream pagination limit.
    has_more: bool,
}

impl Deref for PrefixedModelList {
    type Target = [Model];

    fn deref(&self) -> &Self::Target {
        &self.models
    }
}

// Provider shutdown dates have no time component; interpret them as midnight UTC.
fn model_is_shutdown(model: &Model, now: chrono::DateTime<chrono::Utc>) -> bool {
    model
        .provider_metadata
        .get("shutdown_date")
        .and_then(serde_json::Value::as_str)
        .and_then(|date| chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .is_some_and(|shutdown| shutdown.and_utc() < now)
}

/// A provider's last successfully-listed models and when they were fetched.
#[derive(Clone)]
struct CachedModelList {
    fetched_at: Instant,
    /// Identity of the connection that produced this list (connection id +
    /// backend fingerprint). A mismatch — the connection's endpoint/key rotated,
    /// or it was deleted and replaced at the same prefix — is treated as cold, so
    /// a since-changed connection's catalog is never served (even stale).
    identity: String,
    catalog: PrefixedModelList,
}

/// Per-provider discovered-model cache with **stale-while-refresh + single-flight**.
///
/// - **Fresh** (age < [`MODELS_CACHE_TTL`]): served lock-free, zero network.
/// - **Stale**: the cached list is returned immediately and, unless a refresh is
///   already running for that prefix, one background refetch is spawned
///   (single-flight per prefix; serve-stale-on-error keeps the old list).
/// - **Cold** (no entry yet): the caller must fetch, but concurrent first fetches
///   for the same prefix **and connection identity** coalesce behind one
///   **shared in-flight future**, so a provider is hit at most once even under a
///   burst of `/v1/models` requests — and even when that one fetch FAILS, current
///   waiters share its (empty) result rather than each re-fetching. The failure
///   is not cached, so a later, non-concurrent call is free to try again. Keying
///   the in-flight gate by identity (not prefix alone) means a connection rotated
///   mid-fetch starts its own fetch instead of joining — and being served — the
///   old connection's in-flight result.
///
/// Mirrors [`ResolutionCache`], but keyed per provider prefix rather than being a
/// single global snapshot.
struct ModelsCache {
    /// prefix (bare segment, e.g. `"openai"`) -> last good listing + fetch time.
    entries: DashMap<String, CachedModelList>,
    /// prefix -> single-flight gate for the background *stale* refresh, so at
    /// most one refresh per prefix runs at a time. Per-prefix (not per-identity)
    /// is sufficient here: the stale path only runs for the one identity whose
    /// entry currently occupies the prefix, since there is at most one entry per
    /// prefix.
    gates: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// (prefix, connection identity) -> the shared in-flight *cold* fetch, if one
    /// is running. Concurrent cold callers for the SAME identity clone and await
    /// this same future (success or empty), so one failing fetch is shared rather
    /// than re-issued N times. Keying by identity (not prefix alone) ensures a
    /// connection rotated while a fetch is in flight does not join — and get
    /// served — the previous connection's result. The leader clears the entry
    /// when the fetch completes, so it never caches a failure.
    inflight: DashMap<(String, String), SharedModelFetch>,
    /// Freshness window; production uses [`MODELS_CACHE_TTL`] (tests shrink it).
    ttl: Duration,
}

/// A cold fetch shared by all concurrent waiters for one prefix.
/// `PrefixedModelList` is `Clone`, so every awaiter receives its own copy of the
/// one attempt's result.
type SharedModelFetch = Shared<BoxFuture<'static, PrefixedModelList>>;

impl ModelsCache {
    fn new() -> Self {
        Self {
            entries: DashMap::new(),
            gates: DashMap::new(),
            inflight: DashMap::new(),
            ttl: MODELS_CACHE_TTL,
        }
    }

    /// The per-prefix single-flight gate, created on first use. The returned
    /// `Arc` is cloned out so the `DashMap` guard is never held across an await.
    fn gate(&self, prefix: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.gates
            .entry(prefix.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Cached models if present, produced by the same connection `identity`, and
    /// younger than [`ttl`](Self::ttl).
    fn fresh(&self, prefix: &str, identity: &str) -> Option<PrefixedModelList> {
        let entry = self.entries.get(prefix)?;
        (entry.identity == identity && entry.fetched_at.elapsed() < self.ttl)
            .then(|| entry.catalog.clone())
    }

    /// Any cached models regardless of age, but only if produced by the same
    /// connection `identity` (serve-stale / cold re-check). An identity mismatch
    /// returns `None` so the entry is treated as cold, never served stale.
    fn cached(&self, prefix: &str, identity: &str) -> Option<PrefixedModelList> {
        let entry = self.entries.get(prefix)?;
        (entry.identity == identity).then(|| entry.catalog.clone())
    }

    fn store(&self, prefix: &str, identity: &str, catalog: PrefixedModelList) {
        self.entries.insert(
            prefix.to_string(),
            CachedModelList {
                fetched_at: Instant::now(),
                identity: identity.to_string(),
                catalog,
            },
        );
    }

    /// Serve `prefix`'s discovered models under the SWR policy above. `backend`
    /// is used only when a fetch is actually needed (cold, or a spawned refresh).
    async fn get(
        self: &Arc<Self>,
        prefix: &str,
        identity: &str,
        backend: Arc<dyn BackendProvider>,
    ) -> PrefixedModelList {
        if let Some(catalog) = self.fresh(prefix, identity) {
            return catalog;
        }

        // Stale: serve the old list now; single-flight a background refresh. If a
        // refresh is already in flight (gate held) we skip spawning another.
        if let Some(stale) = self.cached(prefix, identity) {
            if let Ok(guard) = self.gate(prefix).try_lock_owned() {
                let this = self.clone();
                let prefix = prefix.to_string();
                let identity = identity.to_string();
                tokio::spawn(async move {
                    let _guard = guard; // held for the refetch -> single-flight
                    match list_prefixed_models(backend.as_ref(), &prefix).await {
                        Ok(catalog) => this.store(&prefix, &identity, catalog),
                        Err(e) => tracing::warn!(
                            provider = %prefix, error = %e,
                            "Background models refresh failed; keeping stale entry"
                        ),
                    }
                });
            }
            return stale;
        }

        // Cold: nothing to serve, so we must fetch. Coalesce concurrent first
        // fetches for the same (prefix, identity) behind a single shared
        // in-flight future: the leader publishes it, every current waiter
        // clones+awaits the same result, and the leader clears it on completion.
        // So the provider is hit at most once per burst even when the fetch FAILS
        // (waiters share the empty result), yet the failure isn't cached — a
        // later, non-concurrent call finds no in-flight entry and starts a fresh
        // attempt. The key includes the identity so a connection rotated mid-fetch
        // gets its own fetch instead of joining the previous connection's future.
        let key = (prefix.to_string(), identity.to_string());
        let fut = match self.inflight.entry(key.clone()) {
            // A fetch is already in flight for this prefix+identity: share it.
            Entry::Occupied(e) => e.get().clone(),
            // Become the leader: build the shared fetch, publish it, then await.
            Entry::Vacant(e) => {
                let this = self.clone();
                let prefix = prefix.to_string();
                let identity = identity.to_string();
                let fut = async move {
                    let result = match list_prefixed_models(backend.as_ref(), &prefix).await {
                        Ok(catalog) => {
                            this.store(&prefix, &identity, catalog.clone());
                            catalog
                        }
                        Err(e) => {
                            tracing::warn!(
                                provider = %prefix, error = %e,
                                "Failed to list models; omitting from /models"
                            );
                            PrefixedModelList::default()
                        }
                    };
                    // Clear the in-flight marker so the failure is not cached and
                    // a later (non-concurrent) call can retry.
                    this.inflight.remove(&key);
                    result
                }
                .boxed()
                .shared();
                e.insert(fut.clone());
                fut
            }
        };
        fut.await
    }
}

const MODEL_DETAILS_CACHE_CAPACITY: usize = 10_000;
const MODEL_NOT_FOUND_CACHE_CAPACITY: usize = 1_000;
const MODEL_DETAILS_INFLIGHT_CAPACITY: usize = 1_024;
const MODEL_DETAILS_CACHE_TTL: Duration = Duration::from_secs(300);
const MODEL_NOT_FOUND_CACHE_TTL: Duration = Duration::from_secs(30);
const MODEL_DETAILS_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ModelDetailsKey {
    connection_identity: String,
    model_id: String,
}

struct CachedModelDetails {
    model: Model,
    inserted: Instant,
    _permit: OwnedSemaphorePermit,
}

struct CachedNotFound {
    message: String,
    inserted: Instant,
    _permit: OwnedSemaphorePermit,
}

type SharedModelLookup = Shared<BoxFuture<'static, Result<Model, BackendError>>>;

/// Shared bounded cache for `/models/{id}` lookups.
///
/// Positive results and short-lived 404s have separate admission limits, so a
/// stream of caller-controlled unknown ids cannot evict known models. Cold
/// lookups use a shared in-flight future: every concurrent caller observes the
/// same success or failure without serial retries. A background driver keeps
/// each admitted future running after caller cancellation, and a generic
/// deadline prevents a provider from holding an in-flight slot forever.
struct ModelDetailsCache {
    found: DashMap<ModelDetailsKey, CachedModelDetails>,
    not_found: DashMap<ModelDetailsKey, CachedNotFound>,
    inflight: DashMap<ModelDetailsKey, SharedModelLookup>,
    found_slots: Arc<Semaphore>,
    not_found_slots: Arc<Semaphore>,
    inflight_slots: Arc<Semaphore>,
    found_ttl: Duration,
    not_found_ttl: Duration,
    fetch_timeout: Duration,
}

impl ModelDetailsCache {
    fn new() -> Self {
        Self::with_limits(
            MODEL_DETAILS_CACHE_CAPACITY,
            MODEL_NOT_FOUND_CACHE_CAPACITY,
            MODEL_DETAILS_INFLIGHT_CAPACITY,
            MODEL_DETAILS_CACHE_TTL,
            MODEL_NOT_FOUND_CACHE_TTL,
            MODEL_DETAILS_FETCH_TIMEOUT,
        )
    }

    #[cfg(test)]
    fn with_config(capacity: usize, found_ttl: Duration, not_found_ttl: Duration) -> Self {
        Self::with_limits(
            capacity,
            capacity,
            capacity,
            found_ttl,
            not_found_ttl,
            MODEL_DETAILS_FETCH_TIMEOUT,
        )
    }

    fn with_limits(
        found_capacity: usize,
        not_found_capacity: usize,
        inflight_capacity: usize,
        found_ttl: Duration,
        not_found_ttl: Duration,
        fetch_timeout: Duration,
    ) -> Self {
        let found_capacity = found_capacity.max(1);
        let not_found_capacity = not_found_capacity.max(1);
        let inflight_capacity = inflight_capacity.max(1);
        Self {
            found: DashMap::new(),
            not_found: DashMap::new(),
            inflight: DashMap::new(),
            found_slots: Arc::new(Semaphore::new(found_capacity)),
            not_found_slots: Arc::new(Semaphore::new(not_found_capacity)),
            inflight_slots: Arc::new(Semaphore::new(inflight_capacity)),
            found_ttl,
            not_found_ttl,
            fetch_timeout,
        }
    }

    fn cached(&self, key: &ModelDetailsKey) -> Option<Result<Model, BackendError>> {
        if let Some(entry) = self.found.get(key) {
            if entry.inserted.elapsed() < self.found_ttl {
                return Some(Ok(entry.model.clone()));
            }
        }
        self.found
            .remove_if(key, |_, entry| entry.inserted.elapsed() >= self.found_ttl);

        if let Some(entry) = self.not_found.get(key) {
            if entry.inserted.elapsed() < self.not_found_ttl {
                // Upstream headers describe the original request, not this cache hit.
                return Some(Err(BackendError::api_status(404, entry.message.clone())));
            }
        }
        self.not_found.remove_if(key, |_, entry| {
            entry.inserted.elapsed() >= self.not_found_ttl
        });
        None
    }

    fn store_found(&self, key: ModelDetailsKey, model: Model) {
        let mut permit = self.found_slots.clone().try_acquire_owned().ok();
        if permit.is_none() {
            let ttl = self.found_ttl;
            self.found.retain(|_, entry| entry.inserted.elapsed() < ttl);
            permit = self.found_slots.clone().try_acquire_owned().ok();
        }
        if let Some(permit) = permit {
            self.found.insert(
                key,
                CachedModelDetails {
                    model,
                    inserted: Instant::now(),
                    _permit: permit,
                },
            );
        }
    }

    fn store_not_found(&self, key: ModelDetailsKey, message: String) {
        let mut permit = self.not_found_slots.clone().try_acquire_owned().ok();
        if permit.is_none() {
            let ttl = self.not_found_ttl;
            self.not_found
                .retain(|_, entry| entry.inserted.elapsed() < ttl);
            permit = self.not_found_slots.clone().try_acquire_owned().ok();
        }
        if let Some(permit) = permit {
            self.not_found.insert(
                key,
                CachedNotFound {
                    message,
                    inserted: Instant::now(),
                    _permit: permit,
                },
            );
        }
    }

    async fn fetch(
        &self,
        backend: Arc<dyn BackendProvider>,
        model_id: &str,
    ) -> Result<Model, BackendError> {
        match tokio::time::timeout(self.fetch_timeout, backend.get_model(model_id)).await {
            Ok(result) => result,
            Err(_) => Err(BackendError::api_status(
                504,
                format!("Timed out retrieving model '{model_id}'"),
            )),
        }
    }

    fn record(&self, key: ModelDetailsKey, result: &Result<Model, BackendError>) {
        match result {
            Ok(model) => self.store_found(key, model.clone()),
            Err(BackendError::ApiError {
                message,
                status: 404,
                ..
            }) => self.store_not_found(key, message.clone()),
            Err(_) => {}
        }
    }

    async fn get(
        self: &Arc<Self>,
        connection_identity: &str,
        model_id: &str,
        backend: Arc<dyn BackendProvider>,
    ) -> Result<Model, BackendError> {
        let key = ModelDetailsKey {
            connection_identity: connection_identity.to_string(),
            model_id: model_id.to_string(),
        };
        if let Some(result) = self.cached(&key) {
            return result;
        }

        let shared = match self.inflight.entry(key.clone()) {
            Entry::Occupied(entry) => Some(entry.get().clone()),
            Entry::Vacant(entry) => {
                let permit = self.inflight_slots.clone().try_acquire_owned().ok();
                let shared_backend = backend.clone();
                permit.map(|permit| {
                    let this = self.clone();
                    let fetch_key = key.clone();
                    let fetch_model_id = model_id.to_string();
                    let future = async move {
                        let result = this.fetch(shared_backend, &fetch_model_id).await;
                        this.record(fetch_key.clone(), &result);
                        this.inflight.remove(&fetch_key);
                        drop(permit);
                        result
                    }
                    .boxed()
                    .shared();
                    entry.insert(future.clone());
                    let driver = future.clone();
                    tokio::spawn(async move {
                        let _ = driver.await;
                    });
                    future
                })
            }
        };

        if let Some(shared) = shared {
            return shared.await;
        }

        // Coordination is deliberately bounded. When every in-flight slot is
        // occupied, bypass coalescing rather than queueing unrelated model ids
        // behind caller-controlled work; the generic deadline still applies.
        let result = self.fetch(backend, model_id).await;
        self.record(key, &result);
        result
    }
}

/// A backend built from a stored connection, cached so we don't rebuild the
/// HTTP client on every request. Rebuilt when the connection's fingerprint
/// (api_key + base_url) changes.
#[derive(Clone)]
struct CachedBackend {
    fingerprint: String,
    backend: Arc<dyn BackendProvider>,
}

/// Fixed-size, non-secret fingerprint over `(api_key, base_url)`. Cache keys need
/// to change when credentials or endpoints rotate, but must never retain another
/// plaintext copy of the credential for every cached model id.
fn connection_fingerprint(api_key: &str, base_url: Option<&str>) -> String {
    let mut digest = Sha256::new();
    digest.update(api_key.len().to_be_bytes());
    digest.update(api_key.as_bytes());
    match base_url {
        Some(url) => {
            digest.update([1]);
            digest.update(url.len().to_be_bytes());
            digest.update(url.as_bytes());
        }
        None => digest.update([0]),
    }
    let digest = digest.finalize();
    format!("{digest:x}")
}

fn connection_identity(conn: &ProviderConnection) -> String {
    format!(
        "{}|{}",
        conn.id,
        connection_fingerprint(&conn.api_key, conn.base_url.as_deref())
    )
}

/// How long a resolution snapshot stays fresh before a request triggers a
/// background refresh. Steady-state routing reads only the in-memory snapshot,
/// so the stores are hit at most once per this interval and a DB blip is
/// invisible while the snapshot is served stale.
const RESOLUTION_TTL: Duration = Duration::from_secs(5);

/// Connections indexed for O(1) resolution, built from one `connections.list()`.
struct ConnIndex {
    /// key = connection prefix (e.g. `"openai/"`); only connections that own one.
    by_prefix: HashMap<String, StoredConnection>,
    /// key = provider_kind, value = every connection of that built-in kind.
    /// Used only by provider entrypoints that lack an explicit prefix/route.
    by_kind: HashMap<String, Vec<StoredConnection>>,
    /// key = connection id. Backs `chalk/*` route resolution.
    by_id: HashMap<String, StoredConnection>,
    /// Every connection, for `/models` enumeration.
    all: Vec<StoredConnection>,
}

/// Model routes indexed for O(1) resolution, built from one `routes.list()`.
struct RouteIndex {
    /// key = public model id (e.g. `"chalk/gemma"`).
    by_public_id: HashMap<String, IndexedRoute>,
}

/// A model route prepared once per snapshot: the grid is validated and shared
/// behind an `Arc`, so resolution never re-validates or deep-clones it.
struct IndexedRoute {
    id: String,
    published: bool,
    /// `Err` when the stored routing is unreadable or fails validation.
    routing: Result<Option<Arc<crate::route_grid::RouteGrid>>, String>,
    default_connection_id: String,
    default_upstream_model_id: String,
}

impl IndexedRoute {
    fn new(route: ModelRoute) -> Self {
        let default_connection_id = route.default_connection_id().to_owned();
        let default_upstream_model_id = route.default_upstream_model_id().to_owned();
        let routing = match (route.routing_error, route.routing) {
            (Some(_), _) => Err(format!(
                "model route '{}' has stored routing this server cannot read",
                route.public_model_id
            )),
            (None, None) => Ok(None),
            (None, Some(grid)) => grid.validate().map(|()| Some(Arc::new(grid))),
        };
        Self {
            id: route.id,
            published: route.published,
            routing,
            default_connection_id,
            default_upstream_model_id,
        }
    }
}

/// A point-in-time view of the connection + route stores and when it was built.
struct ResolutionSnapshot {
    conns: Arc<ConnIndex>,
    routes: Arc<RouteIndex>,
    fetched_at: Instant,
}

/// Build a [`ResolutionSnapshot`] from raw store listings, stamped `now`.
fn build_snapshot(
    connections: Vec<StoredConnection>,
    routes: Vec<ModelRoute>,
) -> ResolutionSnapshot {
    let mut by_prefix = HashMap::new();
    let mut by_kind = HashMap::new();
    let mut by_id = HashMap::new();
    for conn in &connections {
        // Only `Dynamic`/`Unlisted` exposures are reachable by prefix. An
        // `Explicit` connection is reachable only via a `chalk/*` route through
        // `by_id`; it is retained in `by_kind` only so implicit selection can
        // exclude it deliberately and diagnose the remaining candidates.
        let catch_all_routable = matches!(
            conn.exposure(),
            ExposurePolicy::Dynamic | ExposurePolicy::Unlisted
        );
        if let Some(prefix) = conn.prefix() {
            if catch_all_routable {
                by_prefix.insert(prefix.to_string(), conn.clone());
            }
        }
        // Only built-in kinds can be selected by the compatibility entrypoints.
        // Keep every exposure here; implicit selection deliberately filters
        // route-only (`Explicit`) connections instead of relying on insertion
        // order or silently overwriting an earlier same-kind connection.
        if is_builtin(conn.provider_kind()) {
            by_kind
                .entry(conn.provider_kind().to_string())
                .or_insert_with(Vec::new)
                .push(conn.clone());
        }
        by_id.insert(conn.id().to_string(), conn.clone());
    }
    let by_public_id = routes
        .into_iter()
        .map(|route| (route.public_model_id.clone(), IndexedRoute::new(route)))
        .collect();
    ResolutionSnapshot {
        conns: Arc::new(ConnIndex {
            by_prefix,
            by_kind,
            by_id,
            all: connections,
        }),
        routes: Arc::new(RouteIndex { by_public_id }),
        fetched_at: Instant::now(),
    }
}

/// Select the connection for a provider entrypoint that carries no explicit
/// prefix or model route. `Explicit` connections are route-only and therefore
/// ineligible. The canonical-prefix owner wins; without one, a sole eligible
/// connection preserves the historical single-connection behavior. Multiple
/// non-canonical candidates are an error rather than an order-dependent choice.
fn select_implicit_connection<'a>(
    snap: &'a ResolutionSnapshot,
    provider_kind: &str,
) -> Result<Option<&'a StoredConnection>, BackendError> {
    let Some(all) = snap.conns.by_kind.get(provider_kind) else {
        return Ok(None);
    };
    let eligible: Vec<&StoredConnection> = all
        .iter()
        .filter(|conn| conn.exposure() != ExposurePolicy::Explicit)
        .collect();

    if let Some(canonical) = canonical_prefix(provider_kind) {
        if let Some(conn) = eligible
            .iter()
            .find(|conn| conn.prefix() == Some(canonical))
        {
            return Ok(Some(*conn));
        }
    }

    match eligible.as_slice() {
        [] => Ok(None),
        [conn] => Ok(Some(*conn)),
        _ => Err(BackendError::ConfigError(format!(
            "multiple '{provider_kind}' connections are configured, but none owns the canonical prefix '{}'; assign that prefix or route explicitly",
            canonical_prefix(provider_kind).unwrap_or("<none>")
        ))),
    }
}

/// Stale-while-refresh cache over the connection + route stores. Steady-state
/// routing reads the current snapshot with zero DB queries; once it ages past
/// [`RESOLUTION_TTL`] a single background refresh is kicked off and the stale
/// snapshot is served immediately (and kept if the refresh fails).
struct ResolutionCache {
    connections: Arc<dyn ProviderConnectionStore>,
    routes: Arc<dyn ModelRouteStore>,
    /// The current snapshot. `std` RwLock: guards are only ever held briefly to
    /// read or swap the `Arc`, never across an `.await`.
    snap: RwLock<Option<Arc<ResolutionSnapshot>>>,
    /// Single-flight guard so at most one background refresh runs at a time.
    refreshing: AtomicBool,
    /// Serializes snapshot builds and explicit invalidation. Without it, N
    /// concurrent first requests during a DB outage each issue their own
    /// `list()` pair; with it they queue behind one build attempt. The stale
    /// refresh path also holds this lock so an old build cannot repopulate the
    /// cache after an admin mutation has invalidated it.
    cold_build: tokio::sync::Mutex<()>,
}

impl ResolutionCache {
    fn new(
        connections: Arc<dyn ProviderConnectionStore>,
        routes: Arc<dyn ModelRouteStore>,
    ) -> Self {
        Self {
            connections,
            routes,
            snap: RwLock::new(None),
            refreshing: AtomicBool::new(false),
            cold_build: tokio::sync::Mutex::new(()),
        }
    }

    /// List both stores (outside any lock) and build the indexes.
    async fn build(&self) -> Result<Arc<ResolutionSnapshot>, String> {
        let connections = self.connections.list().await.map_err(|e| e.to_string())?;
        let routes = self.routes.list().await.map_err(|e| e.to_string())?;
        Ok(Arc::new(build_snapshot(connections, routes)))
    }

    /// Background refresh: on success swap in the new snapshot; on failure log
    /// and keep the existing one (serve-stale-on-error).
    async fn refresh(&self) {
        let _guard = self.cold_build.lock().await;
        match self.build().await {
            Ok(snapshot) => *self.snap.write().unwrap() = Some(snapshot),
            Err(e) => {
                tracing::warn!(error = %e, "Failed to refresh routing snapshot; serving stale")
            }
        }
    }

    async fn invalidate(&self) {
        let _guard = self.cold_build.lock().await;
        *self.snap.write().unwrap() = None;
    }

    /// The current resolution snapshot, refreshed at most once per
    /// [`RESOLUTION_TTL`]. Fresh -> return it; stale -> serve it and kick off a
    /// single background refresh; cold -> build synchronously (falling back to a
    /// fresh, uncached empty snapshot on error so routing still yields normal
    /// "not configured" errors and the next request retries the load).
    async fn snapshot(self: &Arc<Self>) -> Arc<ResolutionSnapshot> {
        // Brief read lock: clone the Arc and drop the guard before any await.
        let current = self.snap.read().unwrap().clone();
        if let Some(snapshot) = current {
            if snapshot.fetched_at.elapsed() < RESOLUTION_TTL {
                return snapshot;
            }
            // Stale: single-flight a background refresh, serve the stale snapshot.
            if self
                .refreshing
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                let this = self.clone();
                tokio::spawn(async move {
                    let _ = this.refresh().await;
                    this.refreshing.store(false, Ordering::Release);
                });
            }
            return snapshot;
        }

        // Cold start: serialize the build so concurrent first requests don't each
        // hit the stores. After acquiring the lock, re-check the cache — another
        // waiter may have populated it while we waited.
        let _guard = self.cold_build.lock().await;
        if let Some(snapshot) = self.snap.read().unwrap().clone() {
            return snapshot;
        }

        // Still cold: build now. Cache on success; on error serve (but do not
        // cache) an empty snapshot so the next request retries.
        match self.build().await {
            Ok(snapshot) => {
                *self.snap.write().unwrap() = Some(snapshot.clone());
                snapshot
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to build routing snapshot; serving empty");
                Arc::new(build_snapshot(Vec::new(), Vec::new()))
            }
        }
    }
}

/// Router backend that dispatches each request to an upstream provider resolved
/// entirely from the connection + route stores (no env-var defaults).
///
/// Resolution order for a request `model`:
/// - `chalk/<id>` looks up a **published** model route, then dispatches to the
///   route's connection with the route's upstream model id.
/// - `<prefix>/...` if a connection owns `<prefix>/` (a built-in canonical
///   prefix like `openai/`, or a custom one like `chalk-gemma/`), the prefix is
///   stripped and the request forwarded there.
/// - anything else: the OpenAI catch-all: the built-in `openai` connection with
///   the model name left unchanged (backward compatibility).
///
/// Resolution reads an in-memory snapshot of the stores refreshed at most every
/// [`RESOLUTION_TTL`], so steady-state routing does ~0 DB queries and survives a
/// DB blip. A credential added/rotated at runtime (Providers UI or seeder) takes
/// effect within that window without a restart. A provider is disabled — routing
/// errors with "not configured" — until a connection for it is stored.
pub struct RouterBackend {
    /// Shared process HTTP client. Leaf adapters clone this handle and retain
    /// the same connection pool and connector state.
    http_client: reqwest::Client,
    /// Stale-while-refresh snapshot of the connection + route stores; all
    /// resolution reads go through this instead of hitting the stores per request.
    resolution: Arc<ResolutionCache>,
    /// connection id -> backend built from its current stored credential.
    cache: DashMap<String, CachedBackend>,
    /// The last resolution snapshot the backend cache was pruned against
    /// (identity-compared). Lets [`RouterBackend::snapshot`] evict cached
    /// backends for deleted connections exactly once per snapshot refresh,
    /// instead of on every request.
    pruned_snapshot: std::sync::Mutex<Option<Arc<ResolutionSnapshot>>>,
    /// provider prefix -> its last successfully-listed models (for `/models`),
    /// with stale-while-refresh + single-flight semantics.
    models_cache: Arc<ModelsCache>,
    /// Connection-identity + upstream-model-id -> bounded detail lookup result.
    model_details_cache: Arc<ModelDetailsCache>,
    /// Shared effective-capability catalog used by runtime and model projections.
    capability_catalog: Arc<crate::capabilities::CapabilityCatalog>,
    /// Which process-level identities this environment's connections may use.
    connection_policy: crate::provider_connections::ConnectionPolicy,
}

/// Build a backend adapter for `provider_kind`. `name` supplies the backend
/// identity for the [`OPENAI_COMPATIBLE_KIND`] pseudo-kind (built-in kinds
/// carry their own).
///
/// Shared by routing (stored connections, via [`RouterBackend`]) and
/// connection probing (see `build_probe_adapter` in
/// `crate::service::provider_connections`), so both paths construct backends
/// through the exact same per-kind logic. `policy` is the environment's
/// connection policy, which limits what the adapter may fetch on a request's
/// behalf.
pub(crate) fn build_provider_adapter(
    http_client: reqwest::Client,
    provider_kind: &str,
    name: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<Arc<dyn BackendProvider>, BackendError> {
    match provider_kind {
        "openai" => Ok(Arc::new(openai_backend(
            http_client,
            api_key.to_string(),
            base_url,
        ))),
        "anthropic" => Ok(Arc::new(anthropic_backend(
            http_client,
            api_key.to_string(),
            base_url,
        ))),
        "gemini" => Ok(Arc::new(gemini_backend(
            http_client,
            api_key.to_string(),
            base_url,
        ))),
        // Vertex's credential is a GCP service account key, not a bearer token:
        // the backend mints short-lived OAuth tokens from it per connection.
        "vertex" => Ok(Arc::new(vertex_backend(http_client, api_key, base_url)?)),
        "vllm" => Ok(Arc::new(vllm_backend(
            http_client,
            base_url,
            Some(api_key),
        )?)),
        "bedrock" if crate::backend::bedrock::runtime_endpoint(base_url)?.is_some() => {
            Ok(Arc::new(BedrockRuntimeBackend::with_api_key(
                http_client,
                api_key,
                base_url,
            )?))
        }
        "bedrock" => Ok(Arc::new(bedrock_backend(
            http_client,
            api_key.to_string(),
            None,
            base_url,
        ))),
        "openrouter" => Ok(Arc::new(openrouter_backend(
            http_client,
            api_key.to_string(),
            base_url,
        ))),
        "xai" => Ok(Arc::new(xai_backend(
            http_client,
            api_key.to_string(),
            base_url,
        ))),
        "cohere" => Ok(Arc::new(cohere_backend(
            http_client,
            api_key.to_string(),
            base_url,
        ))),
        "qwen" => Ok(Arc::new(qwen_backend(
            http_client,
            api_key.to_string(),
            base_url,
        ))),
        "fireworks" => Ok(Arc::new(fireworks_backend(
            http_client,
            api_key.to_string(),
            base_url,
        ))),
        "together" => Ok(Arc::new(together_backend(
            http_client,
            api_key.to_string(),
            base_url,
        ))),
        "groq" => Ok(Arc::new(groq_backend(
            http_client,
            api_key.to_string(),
            base_url,
        ))),
        "mistral" => Ok(Arc::new(mistral_backend(
            http_client,
            api_key.to_string(),
            base_url,
        ))),
        "perplexity" => Ok(Arc::new(perplexity_backend(
            http_client,
            api_key.to_string(),
            base_url,
        ))),
        // Voyage has a fixed endpoint, so any base_url override is ignored.
        "voyage" => Ok(Arc::new(VoyageBackend::new(
            http_client,
            api_key.to_string(),
        ))),
        "typesafe" => Ok(Arc::new(crate::backend::TypeSafeBackend::new(
            http_client,
            api_key.to_string(),
            base_url,
        ))),
        // An arbitrary OpenAI-compatible endpoint: base_url is required.
        OPENAI_COMPATIBLE_KIND => {
            let base_url = base_url
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .ok_or_else(|| {
                    BackendError::ConfigError(format!("provider '{name}' requires a base URL"))
                })?;
            Ok(Arc::new(OpenAICompatibleBackend::for_provider(
                http_client,
                name.to_string(),
                api_key.to_string(),
                base_url.to_string(),
            )))
        }
        _ => Err(BackendError::ConfigError(format!(
            "provider '{provider_kind}' is not supported"
        ))),
    }
}

impl RouterBackend {
    /// Construct with the process-wide HTTP client and the connection +
    /// model-route stores. Provider backends are resolved from these per
    /// request; there are no env-var defaults.
    pub fn new(
        http_client: reqwest::Client,
        connections: Arc<dyn ProviderConnectionStore>,
        routes: Arc<dyn ModelRouteStore>,
    ) -> Self {
        Self {
            http_client,
            resolution: Arc::new(ResolutionCache::new(connections, routes)),
            cache: DashMap::new(),
            pruned_snapshot: std::sync::Mutex::new(None),
            models_cache: Arc::new(ModelsCache::new()),
            model_details_cache: Arc::new(ModelDetailsCache::new()),
            capability_catalog: Arc::new(crate::capabilities::CapabilityCatalog::new()),
            connection_policy: crate::provider_connections::ConnectionPolicy::trusted(),
        }
    }

    /// Restrict which process-level identities stored connections may use.
    /// Applies when a backend is built, so a row written by any path is held to it.
    pub fn with_connection_policy(
        mut self,
        policy: crate::provider_connections::ConnectionPolicy,
    ) -> Self {
        self.connection_policy = policy;
        self
    }

    /// Take the current resolution snapshot, evicting any cached backends whose
    /// connection has disappeared from it. The backend cache (`self.cache`) is
    /// otherwise insert-only, so without this a deleted connection's built
    /// backend would leak for the process lifetime. Pruning runs only when the
    /// snapshot actually changes (identity compare), so steady-state requests —
    /// which keep seeing the same snapshot Arc — do no extra work.
    async fn snapshot(&self) -> Arc<ResolutionSnapshot> {
        let snap = self.resolution.snapshot().await;
        let mut pruned = self.pruned_snapshot.lock().unwrap();
        let changed = match pruned.as_ref() {
            Some(prev) => !Arc::ptr_eq(prev, &snap),
            None => true,
        };
        if changed {
            self.evict_stale_backends(&snap);
            *pruned = Some(snap.clone());
        }
        snap
    }

    /// Drop cached backends for connection ids not present in `snap`.
    fn evict_stale_backends(&self, snap: &ResolutionSnapshot) {
        self.cache.retain(|id, _| snap.conns.by_id.contains_key(id));
    }

    /// Build (or reuse a cached) backend for a readable connection. The cache is
    /// keyed by connection id and rebuilt when the connection's fingerprint
    /// (api_key + base_url) changes.
    fn backend_for_connection(
        &self,
        conn: &ProviderConnection,
    ) -> Result<Arc<dyn BackendProvider>, BackendError> {
        let fingerprint = connection_fingerprint(&conn.api_key, conn.base_url.as_deref());
        // A cached backend was built from this same credential and endpoint under
        // this router's fixed policy, so only a build has to pass the policy;
        // checking on every request would re-parse the URL and any Vertex key.
        if let Some(entry) = self.cache.get(&conn.id) {
            if entry.fingerprint == fingerprint {
                return Ok(entry.backend.clone());
            }
        }
        self.connection_policy
            .check(&conn.provider_kind, conn.base_url.as_deref(), &conn.api_key)
            .map_err(BackendError::ConfigError)?;
        let backend = build_provider_adapter(
            self.http_client.clone(),
            &conn.provider_kind,
            &conn.name,
            &conn.api_key,
            conn.base_url.as_deref(),
        )?;
        self.cache.insert(
            conn.id.clone(),
            CachedBackend {
                fingerprint,
                backend: backend.clone(),
            },
        );
        Ok(backend)
    }

    /// Resolve a *present* stored connection into a live backend: Readable ->
    /// build (or reuse a cached) backend; Unreadable ->
    /// [`BackendError::CredentialUnreadable`]. Absence is handled by the caller.
    ///
    /// A `routing_enabled = false` connection is the documented emergency kill
    /// switch: it never serves proxy traffic, so every routing path that turns a
    /// connection into a backend flows through here and is rejected. The
    /// `Unreadable` credential error takes precedence and is returned unchanged.
    fn backend_for(
        &self,
        conn: &StoredConnection,
    ) -> Result<Arc<dyn BackendProvider>, BackendError> {
        match conn {
            StoredConnection::Readable(conn) => {
                if !conn.routing_enabled {
                    return Err(BackendError::ConfigError(format!(
                        "provider connection '{}' is disabled",
                        conn.name
                    )));
                }
                self.backend_for_connection(conn)
            }
            StoredConnection::Unreadable { provider_kind, .. } => {
                Err(BackendError::CredentialUnreadable(provider_kind.clone()))
            }
        }
    }

    /// Resolve the OpenAI catch-all backend (the built-in `openai` connection)
    /// from an existing snapshot. Errors with "not configured" when no such
    /// connection is stored.
    fn openai_backend_from(
        &self,
        snap: &ResolutionSnapshot,
    ) -> Result<Arc<dyn BackendProvider>, BackendError> {
        match select_implicit_connection(snap, "openai")? {
            Some(conn) => self.backend_for(conn),
            None => Err(provider_not_configured("openai")),
        }
    }

    /// The OpenAI catch-all backend, taking a fresh snapshot. Errors with "not
    /// configured" when no such connection is stored.
    async fn openai_backend(&self) -> Result<Arc<dyn BackendProvider>, BackendError> {
        let snap = self.snapshot().await;
        self.openai_backend_from(&snap)
    }

    /// Parse the model name and return (backend, upstream_model_id). A thin
    /// wrapper over [`Self::resolve`] kept for `create_*`/`get_model` and the
    /// routing tests, which only need the backend + upstream model.
    async fn route_model(
        &self,
        model: &str,
    ) -> Result<(Arc<dyn BackendProvider>, String), BackendError> {
        let resolved = self.resolve(model).await?;
        Ok((resolved.backend, resolved.resolution.upstream_model_id))
    }

    /// Resolve `model` against an already-taken snapshot into a [`ResolvedRoute`]
    /// (backend + [`RouteResolution`]). Snapshot-free so a caller can reuse one
    /// snapshot for both resolution and follow-up index lookups (see
    /// [`Self::get_model`]). When `strict`, the catch-all branch is an error
    /// rather than the OpenAI backend.
    fn resolve_on(
        &self,
        model: &str,
        snap: &ResolutionSnapshot,
        strict: bool,
    ) -> Result<ResolvedRoute, BackendError> {
        // 1. `chalk/<id>`: a published model route.
        if model.starts_with(ROUTE_RESERVED_PREFIX) {
            let route = snap
                .routes
                .by_public_id
                .get(model)
                .ok_or_else(|| BackendError::model_not_found(model))?;
            if !route.published {
                return Err(BackendError::api_error(
                    format!("model route '{model}' is not published"),
                    503,
                    vec![(
                        "retry-after".to_string(),
                        RESOLUTION_TTL.as_secs().to_string(),
                    )],
                ));
            }
            let routing = route
                .routing
                .as_ref()
                .map_err(|message| BackendError::ConfigError(message.clone()))?;
            let conn = snap
                .conns
                .by_id
                .get(route.default_connection_id.as_str())
                .ok_or_else(|| {
                    BackendError::ConfigError(format!(
                        "model route '{model}' points at a connection that no longer exists"
                    ))
                })?;
            let upstream = route.default_upstream_model_id.clone();
            let mut resolution = resolution_for(conn, upstream.clone());
            resolution.model_route_id = Some(route.id.clone());
            return Ok(ResolvedRoute {
                routing: routing.clone(),
                backend: self.backend_for(conn)?,
                stripped_model: upstream.clone(),
                resolution,
                capabilities: capabilities_for(&self.capability_catalog, conn, &upstream)?,
            });
        }

        // 2. `<prefix>/...`: a connection that owns `<prefix>/` (built-in
        //    canonical or custom). Strip the prefix on a hit.
        if let Some((seg, rest)) = strip_route_prefix(model) {
            if let Some(conn) = snap.conns.by_prefix.get(&format!("{seg}/")) {
                return Ok(ResolvedRoute {
                    routing: Default::default(),
                    backend: self.backend_for(conn)?,
                    stripped_model: rest.to_string(),
                    resolution: resolution_for(conn, rest.to_string()),
                    capabilities: capabilities_for(&self.capability_catalog, conn, rest)?,
                });
            }
        }

        // 3. Catch-all: OpenAI, model name left unchanged (backward compat). A
        //    strict resolve (fallback alternate) must NOT land here — an
        //    unresolved alternate is an error, not a silent OpenAI dispatch.
        if strict {
            return Err(BackendError::ConfigError(format!(
                "model '{model}' did not resolve to a published route or a known provider prefix"
            )));
        }
        let conn = select_implicit_connection(snap, "openai")?
            .ok_or_else(|| provider_not_configured("openai"))?;
        Ok(ResolvedRoute {
            routing: Default::default(),
            backend: self.backend_for(conn)?,
            stripped_model: model.to_string(),
            resolution: resolution_for(conn, model.to_string()),
            capabilities: capabilities_for(&self.capability_catalog, conn, model)?,
        })
    }
}

fn provider_not_configured(provider: &str) -> BackendError {
    BackendError::ConfigError(format!("provider '{provider}' is not configured"))
}

/// Build the [`RouteResolution`] for a request that resolved to `conn` with the
/// given `upstream_model_id`. `provider_kind` is the connection's real kind (or
/// the generic `"openai-compatible"`); the connection's bespoke identity is
/// carried by `connection_id` / `connection_name`.
fn resolution_for(conn: &StoredConnection, upstream_model_id: String) -> RouteResolution {
    RouteResolution {
        provider_kind: conn.provider_kind().to_string(),
        connection_id: Some(conn.id().to_string()),
        connection_name: conn.name().to_string(),
        upstream_model_id,
        connection_prefix: conn.prefix().map(str::to_string),
        model_route_id: None,
    }
}

fn capabilities_for(
    catalog: &crate::capabilities::CapabilityCatalog,
    conn: &StoredConnection,
    upstream_model_id: &str,
) -> Result<Arc<crate::capabilities::ResolvedCapabilityProfile>, BackendError> {
    let fingerprint = match conn {
        StoredConnection::Readable(connection) => connection_identity(connection),
        StoredConnection::Unreadable { .. } => format!(
            "{}|{}|{}",
            conn.id(),
            conn.base_url().unwrap_or_default(),
            crate::backend::bedrock::is_runtime_connection(conn.base_url())
        ),
    };
    resolve_connection_capabilities(
        catalog,
        &fingerprint,
        conn.provider_kind(),
        conn.base_url(),
        upstream_model_id,
        None,
        conn.declared_capabilities(),
    )
    .map_err(|error| {
        BackendError::ConfigError(format!(
            "connection '{}' has an unreadable capability declaration: {error}",
            conn.name()
        ))
    })
}

pub(crate) fn resolve_connection_capabilities(
    catalog: &crate::capabilities::CapabilityCatalog,
    connection_fingerprint: &str,
    provider_kind: &str,
    base_url: Option<&str>,
    upstream_model_id: &str,
    discovered: Option<&crate::capabilities::CapabilityFactPatch>,
    declared: Option<&crate::capabilities::CapabilityPatch>,
) -> Result<
    Arc<crate::capabilities::ResolvedCapabilityProfile>,
    crate::capabilities::CapabilitySchemaError,
> {
    let connection_facts = (provider_kind == "bedrock"
        && crate::backend::bedrock::is_runtime_connection(base_url))
    .then(|| {
        let mut patch = crate::capabilities::bedrock_runtime_patch();
        if crate::backend::bedrock::is_runtime_openai_connection(base_url) {
            patch
                .interfaces
                .get_mut(&crate::capabilities::Interface::OpenaiChatCompletions)
                .and_then(|interface| {
                    interface
                        .operations
                        .get_mut(&crate::capabilities::InterfaceOperation::Create)
                })
                .expect("Runtime profile includes Chat create")
                .mode = Some(crate::capabilities::DeliveryMode::Native);
            // Runtime OpenAI does not implement the Converse token-count API.
            patch
                .interfaces
                .get_mut(&crate::capabilities::Interface::AnthropicMessages)
                .and_then(|interface| {
                    interface
                        .operations
                        .get_mut(&crate::capabilities::InterfaceOperation::CountTokens)
                })
                .expect("Runtime profile includes Messages count_tokens")
                .mode = Some(crate::capabilities::DeliveryMode::Unsupported);
        }
        patch
    });
    catalog.resolve(
        connection_fingerprint,
        provider_kind,
        upstream_model_id,
        connection_facts.as_ref(),
        discovered,
        declared,
    )
}

#[async_trait]
impl BackendProvider for RouterBackend {
    fn supports_native_chat(&self) -> bool {
        true
    }
    async fn create_native_chat_completion(
        &self,
        mut request: crate::native_chat::NativeChatRequest,
    ) -> Result<super::provider::BackendChatResponse, BackendError> {
        let route = self.resolve(&request.model).await?;
        request.set_model(route.stripped_model);
        route.backend.create_native_chat_completion(request).await
    }

    async fn create_native_chat_completion_stream(
        &self,
        mut request: crate::native_chat::NativeChatRequest,
    ) -> Result<super::provider::BackendSseResponse, BackendError> {
        let route = self.resolve(&request.model).await?;
        request.set_model(route.stripped_model);
        route
            .backend
            .create_native_chat_completion_stream(request)
            .await
    }

    async fn invalidate_routing_cache(&self) {
        self.resolution.invalidate().await;
    }

    async fn create_chat_completion(
        &self,
        mut request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        let (backend, stripped_model) = self.route_model(&request.model).await?;
        request.model = stripped_model;
        backend.create_chat_completion(request).await
    }

    async fn create_chat_completion_stream(
        &self,
        mut request: ChatCompletionRequest,
    ) -> Result<SseByteStream, BackendError> {
        let (backend, stripped_model) = self.route_model(&request.model).await?;
        request.model = stripped_model;
        backend.create_chat_completion_stream(request).await
    }

    // NOTE: capability enforcement for embeddings/images/audio now happens at
    // the handler level (see `crate::handlers::routing::run_passthrough` and
    // `check_passthrough_capabilities`), which resolves the model once and
    // gates on the same `CapabilityRequirements` this trait impl used to
    // build internally. Handlers dispatch straight to the resolved leaf
    // backend (`route.backend.create_*`), so these `RouterBackend` methods
    // are no longer reached by any handler; they remain only to satisfy the
    // `BackendProvider` trait and now do a plain resolve + strip + dispatch,
    // matching `create_chat_completion` above.
    async fn create_native_embedding(
        &self,
        mut request: crate::native_json::NativeJsonRequest<CreateEmbeddingRequest>,
    ) -> Result<super::provider::BackendJsonView<CreateEmbeddingResponse>, BackendError> {
        let route = self.resolve(&request.model).await?;
        request.set_model(route.stripped_model.clone());
        route.backend.create_native_embedding(request).await
    }
    async fn create_native_image(
        &self,
        mut request: crate::native_json::NativeJsonRequest<CreateImageRequest>,
    ) -> Result<super::provider::BackendJsonView<ImageResponse>, BackendError> {
        let route = self
            .resolve(request.model.as_deref().unwrap_or("openai/dall-e-3"))
            .await?;
        if request.model.is_some() {
            request.set_model(route.stripped_model.clone());
        }
        route.backend.create_native_image(request).await
    }
    async fn create_native_speech(
        &self,
        mut request: crate::native_json::NativeJsonRequest<CreateSpeechRequest>,
    ) -> Result<super::provider::BackendBodyResponse, BackendError> {
        let route = self.resolve(&request.model).await?;
        request.set_model(route.stripped_model.clone());
        route.backend.create_native_speech(request).await
    }
    async fn create_native_audio(
        &self,
        mut request: crate::native_audio::NativeAudioRequest,
    ) -> Result<super::provider::BackendBodyResponse, BackendError> {
        let route = self.resolve(request.model()).await?;
        request.set_model(route.stripped_model.clone());
        route.backend.create_native_audio(request).await
    }

    async fn create_embedding(
        &self,
        mut request: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        let route = self.resolve(&request.model).await?;
        request.model = route.stripped_model.clone();
        route.backend.create_embedding(request).await
    }

    async fn create_image(
        &self,
        mut request: CreateImageRequest,
    ) -> Result<ImageResponse, BackendError> {
        // Route on the model prefix and strip it before forwarding, mirroring
        // chat/embeddings. Without a model we keep the historical behavior of
        // defaulting to OpenAI.
        match request.model.take() {
            Some(model) => {
                let route = self.resolve(&model).await?;
                request.model = Some(route.stripped_model.clone());
                route.backend.create_image(request).await
            }
            None => {
                let route = self.resolve_default().await?;
                route.backend.create_image(request).await
            }
        }
    }

    async fn create_speech(
        &self,
        mut request: CreateSpeechRequest,
    ) -> Result<bytes::Bytes, BackendError> {
        let route = self.resolve(&request.model).await?;
        request.model = route.stripped_model.clone();
        route.backend.create_speech(request).await
    }

    async fn create_transcription(
        &self,
        model: String,
        file_bytes: bytes::Bytes,
        file_name: String,
        language: Option<String>,
        prompt: Option<String>,
        response_format: Option<String>,
        temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        let route = self.resolve(&model).await?;
        route
            .backend
            .create_transcription(
                route.stripped_model.clone(),
                file_bytes,
                file_name,
                language,
                prompt,
                response_format,
                temperature,
            )
            .await
    }

    async fn create_translation(
        &self,
        model: String,
        file_bytes: bytes::Bytes,
        file_name: String,
        prompt: Option<String>,
        response_format: Option<String>,
        temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        let route = self.resolve(&model).await?;
        route
            .backend
            .create_translation(
                route.stripped_model.clone(),
                file_bytes,
                file_name,
                prompt,
                response_format,
                temperature,
            )
            .await
    }

    async fn create_moderation(
        &self,
        request: CreateModerationRequest,
    ) -> Result<CreateModerationResponse, BackendError> {
        // Moderation always goes to OpenAI for now
        self.openai_backend()
            .await?
            .create_moderation(request)
            .await
    }

    async fn list_provider_connection_models(
        &self,
        connection_ids: &[String],
    ) -> Result<Vec<ConnectionModel>, BackendError> {
        let snap = self.snapshot().await;

        // Resolve which connections to list. An explicit set is looked up
        // directly by id (validating existence/enablement as we go) rather
        // than scanning every stored connection and testing membership;
        // listing everything (empty `connection_ids`) has no id set to look
        // up against, so that case still scans.
        let selected: Vec<&ProviderConnection> = if connection_ids.is_empty() {
            snap.conns
                .all
                .iter()
                .filter_map(|stored| match stored {
                    StoredConnection::Readable(connection) => Some(connection),
                    StoredConnection::Unreadable { .. } => None,
                })
                .collect()
        } else {
            let requested: std::collections::HashSet<&str> =
                connection_ids.iter().map(String::as_str).collect();
            let mut out = Vec::with_capacity(requested.len());
            for connection_id in &requested {
                let stored = snap.conns.by_id.get(*connection_id).ok_or_else(|| {
                    BackendError::ConfigError(format!(
                        "provider connection '{connection_id}' does not exist"
                    ))
                })?;
                match stored {
                    StoredConnection::Readable(connection) if connection.routing_enabled => {
                        out.push(connection);
                    }
                    StoredConnection::Readable(connection) => {
                        return Err(BackendError::ConfigError(format!(
                            "provider connection '{}' is disabled",
                            connection.name
                        )));
                    }
                    StoredConnection::Unreadable { provider_kind, .. } => {
                        return Err(BackendError::CredentialUnreadable(provider_kind.clone()));
                    }
                }
            }
            out
        };

        let mut models = Vec::new();
        for connection in selected {
            if !connection.routing_enabled || connection.exposure != ExposurePolicy::Dynamic {
                continue;
            }
            let Some(prefix) = connection.prefix.as_deref() else {
                continue;
            };
            let prefix = prefix.trim_end_matches('/');
            let identity = connection_identity(connection);
            let catalog = self
                .models_cache
                .get(prefix, &identity, self.backend_for_connection(connection)?)
                .await;
            for model in catalog.models {
                let upstream_model_id = strip_route_prefix(&model.id)
                    .map_or(model.id.as_str(), |(_, rest)| rest)
                    .to_string();
                let discovered = crate::capability_discovery::from_provider_metadata(
                    &connection.provider_kind,
                    &model.provider_metadata,
                );
                let capabilities = resolve_connection_capabilities(
                    &self.capability_catalog,
                    &identity,
                    &connection.provider_kind,
                    connection.base_url.as_deref(),
                    &upstream_model_id,
                    discovered.as_ref(),
                    connection.declared_capabilities.as_ref(),
                )
                .ok()
                .map(|resolved| resolved.profile.clone());
                models.push(ConnectionModel {
                    connection_id: connection.id.clone(),
                    public_model_id: model.id,
                    upstream_model_id,
                    capabilities,
                });
            }
        }
        models.sort_by(|left, right| {
            left.connection_id
                .cmp(&right.connection_id)
                .then_with(|| left.public_model_id.cmp(&right.public_model_id))
        });
        Ok(models)
    }

    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        let snap = self.snapshot().await;

        // Build a backend per listable connection (Readable, routing-enabled,
        // Dynamic). Connections that fail to build are logged and omitted. The
        // `identity` (connection id + backend fingerprint) ties each cache entry
        // to the exact connection that produced it, so a rotated endpoint or a
        // delete-and-replace at the same prefix invalidates the cached catalog
        // instead of serving the old connection's models for up to the TTL.
        let mut listable: Vec<(
            String,
            String,
            Arc<dyn BackendProvider>,
            String,
            Option<String>,
            Option<crate::capabilities::CapabilityPatch>,
        )> = Vec::new();
        for stored in &snap.conns.all {
            let StoredConnection::Readable(conn) = stored else {
                continue;
            };
            if !conn.routing_enabled || conn.exposure != ExposurePolicy::Dynamic {
                continue;
            }
            let Some(prefix) = conn.prefix.as_deref() else {
                continue;
            };
            // `list_prefixed_models` re-adds the `/`, so store the bare segment.
            let prefix = prefix.trim_end_matches('/').to_string();
            let identity = connection_identity(conn);
            match self.backend_for_connection(conn) {
                Ok(backend) => listable.push((
                    prefix,
                    identity,
                    backend,
                    conn.provider_kind.clone(),
                    conn.base_url.clone(),
                    conn.declared_capabilities.clone(),
                )),
                Err(error) => tracing::warn!(
                    connection = %conn.name,
                    error = %error,
                    "Failed to build backend; omitting from /models"
                ),
            }
        }

        // Fan out concurrently: fresh entries resolve immediately, stale entries
        // serve immediately and refresh in the background, and only a cold prefix
        // blocks on the network (coalesced per prefix). See [`ModelsCache`].
        let fetched = join_all(listable.iter().map(|(prefix, identity, backend, _, _, _)| {
            let models_cache = self.models_cache.clone();
            let backend = backend.clone();
            async move { models_cache.get(prefix, identity, backend).await }
        }))
        .await;

        let now = chrono::Utc::now();
        let mut all_models = Vec::new();
        let mut has_more = false;
        for (models, (_, identity, _, provider_kind, base_url, declared)) in
            fetched.into_iter().zip(listable.iter())
        {
            has_more |= models.has_more;
            all_models.extend(
                models
                    .models
                    .into_iter()
                    .filter(|model| !model_is_shutdown(model, now))
                    .map(|mut model| {
                        let upstream_model_id =
                            strip_route_prefix(&model.id).map_or(model.id.as_str(), |(_, id)| id);
                        let discovered = crate::capability_discovery::from_provider_metadata(
                            provider_kind,
                            &model.provider_metadata,
                        );
                        model.capabilities = resolve_connection_capabilities(
                            &self.capability_catalog,
                            identity,
                            provider_kind,
                            base_url.as_deref(),
                            upstream_model_id,
                            discovered.as_ref(),
                            declared.as_ref(),
                        )
                        .ok()
                        .map(|resolved| resolved.profile.clone());
                        model
                    }),
            );
        }

        // Append published `chalk/*` routes, listed under their public id.
        for (public_model_id, _) in snap.routes.by_public_id.iter().filter(|(_, r)| r.published) {
            let capabilities = self
                .resolve_on(public_model_id, &snap, false)
                .ok()
                .map(|resolved| resolved.capabilities.profile.clone());
            all_models.push(Model {
                id: public_model_id.clone(),
                object: "model".to_string(),
                created: 0,
                owned_by: "chalk".to_string(),
                capabilities,
                provider_metadata: Default::default(),
            });
        }

        Ok(ListResponse {
            object: "list".to_string(),
            data: all_models,
            first_id: None,
            last_id: None,
            has_more,
        })
    }

    async fn get_model(&self, model_id: &str) -> Result<Model, BackendError> {
        // One snapshot for both resolution and the prefix re-application check
        // below, so we never read a different snapshot mid-request.
        let snap = self.snapshot().await;
        let resolved = self.resolve_on(model_id, &snap, false)?;
        let upstream_model_id = resolved.resolution.upstream_model_id.clone();
        let connection_id = resolved
            .resolution
            .connection_id
            .as_deref()
            .ok_or_else(|| {
                BackendError::Other("resolved model route had no connection identity".to_string())
            })?;
        let stored = snap.conns.by_id.get(connection_id).ok_or_else(|| {
            BackendError::Other(format!(
                "resolved model route referenced missing connection '{connection_id}'"
            ))
        })?;
        let StoredConnection::Readable(connection) = stored else {
            return Err(BackendError::CredentialUnreadable(
                resolved.resolution.provider_kind.clone(),
            ));
        };
        let identity = connection_identity(connection);
        let mut model = self
            .model_details_cache
            .get(&identity, &upstream_model_id, resolved.backend.clone())
            .await?;

        // Re-apply the caller-facing id. A `chalk/*` route is presented under its
        // public id; a prefix-routed model gets its stripped prefix restored. The
        // OpenAI catch-all leaves the model id untouched.
        if model_id.starts_with(ROUTE_RESERVED_PREFIX) {
            model.id = model_id.to_string();
        } else if let Some((seg, _)) = strip_route_prefix(model_id) {
            let prefix = format!("{seg}/");
            if snap.conns.by_prefix.contains_key(&prefix) {
                model.id = format!("{seg}/{}", model.id);
            }
        }

        let discovered = crate::capability_discovery::from_provider_metadata(
            &connection.provider_kind,
            &model.provider_metadata,
        );
        model.capabilities = Some(
            resolve_connection_capabilities(
                &self.capability_catalog,
                &identity,
                &connection.provider_kind,
                connection.base_url.as_deref(),
                &upstream_model_id,
                discovered.as_ref(),
                connection.declared_capabilities.as_ref(),
            )
            .map_err(|error| {
                BackendError::ConfigError(format!(
                    "connection '{}' has an unreadable capability declaration: {error}",
                    connection.name
                ))
            })?
            .profile
            .clone(),
        );

        Ok(model)
    }

    fn name(&self) -> &str {
        "router"
    }

    async fn resolve_route(&self, model: &str) -> Result<RouteResolution, BackendError> {
        Ok(self.resolve(model).await?.resolution)
    }

    async fn resolve_route_strict(&self, model: &str) -> Result<RouteResolution, BackendError> {
        Ok(self.resolve_strict(model).await?.resolution)
    }

    /// Resolve `model` (lenient: an unrecognized model hits the OpenAI catch-all)
    /// against a freshly-taken snapshot — one snapshot read yields the leaf
    /// backend, stripped model, and [`RouteResolution`] together.
    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        let snap = self.snapshot().await;
        self.resolve_on(model, &snap, false)
    }

    /// Resolve `model` strictly (for fallback alternates): identical to
    /// [`resolve`](Self::resolve) except that an unrecognized model — a bare name
    /// or an `<unknown-prefix>/…` — is an error instead of falling through to the
    /// OpenAI catch-all.
    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        let snap = self.snapshot().await;
        self.resolve_on(model, &snap, true)
    }

    async fn normalize_request_fallbacks(
        &self,
        fallbacks: &crate::route_grid::FallbackMap,
    ) -> Result<crate::route_grid::FallbackMap, BackendError> {
        if fallbacks.is_empty() {
            return Ok(fallbacks.clone());
        }
        let snapshot = self.snapshot().await;
        let by_name: HashMap<_, _> = snapshot
            .conns
            .all
            .iter()
            .map(|connection| (connection.name(), connection.id()))
            .collect();
        let normalize = |model: &str| -> Result<String, BackendError> {
            let (connection, upstream) = crate::route_grid::split_target(model)
                .map_err(|error| BackendError::api_status(400, error))?;
            // An exact ID takes precedence over another connection's name.
            let id = if snapshot.conns.by_id.contains_key(connection) {
                connection
            } else {
                by_name.get(connection).copied().unwrap_or(connection)
            };
            Ok(crate::route_grid::target(id, upstream))
        };
        let mut normalized = crate::route_grid::FallbackMap::new();
        // Insert ID-keyed entries last so they win over name-keyed entries
        // for the same destination, independent of map ordering.
        for id_keys in [false, true] {
            for (model, alternatives) in fallbacks {
                let target = normalize(model)?;
                if (target == *model) != id_keys {
                    continue;
                }
                normalized.insert(
                    target,
                    alternatives
                        .iter()
                        .map(|alternative| normalize(alternative))
                        .collect::<Result<_, _>>()?,
                );
            }
        }
        // Names and IDs must not disguise repeated attempts or self-loops.
        crate::route_grid::validate_fallbacks(&normalized)
            .map_err(|error| BackendError::api_status(400, error))?;
        Ok(normalized)
    }

    async fn resolve_concrete(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        let (connection, upstream) =
            crate::route_grid::split_target(model).map_err(BackendError::ConfigError)?;
        let snapshot = self.snapshot().await;
        let connection = snapshot.conns.by_id.get(connection).ok_or_else(|| {
            BackendError::ConfigError(format!("connection '{connection}' does not exist"))
        })?;
        Ok(ResolvedRoute {
            routing: Default::default(),
            backend: self.backend_for(connection)?,
            stripped_model: upstream.to_string(),
            resolution: resolution_for(connection, upstream.to_string()),
            capabilities: capabilities_for(&self.capability_catalog, connection, upstream)?,
        })
    }

    async fn direct_model_id(&self, concrete: &str) -> Option<String> {
        let (connection, upstream) = crate::route_grid::split_target(concrete).ok()?;
        let snapshot = self.snapshot().await;
        let connection = snapshot.conns.by_id.get(connection)?;
        // Mirror `build_snapshot`'s prefix index: only these exposures are
        // addressable without a route.
        if !matches!(
            connection.exposure(),
            ExposurePolicy::Dynamic | ExposurePolicy::Unlisted
        ) {
            return None;
        }
        connection
            .prefix()
            .map(|prefix| format!("{prefix}{upstream}"))
    }

    async fn resolve_connection(&self, connection_id: &str) -> Result<ResolvedRoute, BackendError> {
        let snap = self.snapshot().await;
        let conn = snap.conns.by_id.get(connection_id).ok_or_else(|| {
            BackendError::api_status(404, "response refers to a connection that no longer exists")
        })?;
        Ok(ResolvedRoute {
            routing: Default::default(),
            backend: self.backend_for(conn)?,
            stripped_model: String::new(),
            resolution: resolution_for(conn, String::new()),
            capabilities: capabilities_for(&self.capability_catalog, conn, "")?,
        })
    }

    async fn resolve_anthropic(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        let snap = self.snapshot().await;
        let route = if model.contains('/') {
            self.resolve_on(model, &snap, true)?
        } else {
            let conn = select_implicit_connection(&snap, "anthropic")?.ok_or_else(|| {
                BackendError::api_status(503, "provider 'anthropic' is not configured")
            })?;
            ResolvedRoute {
                routing: Default::default(),
                backend: self.backend_for(conn)?,
                stripped_model: model.to_string(),
                resolution: resolution_for(conn, model.to_string()),
                capabilities: capabilities_for(&self.capability_catalog, conn, model)?,
            }
        };

        Ok(route)
    }

    /// The OpenAI catch-all connection resolved into a full [`ResolvedRoute`], so
    /// a no-model request authorizes against and dispatches to the *actual*
    /// stored connection (including a custom `name`) rather than a fabricated
    /// identity. `stripped_model` / `upstream_model_id` are empty — the caller
    /// keeps `model: None` and the upstream picks its own default.
    async fn resolve_default(&self) -> Result<ResolvedRoute, BackendError> {
        let snap = self.snapshot().await;
        let conn = select_implicit_connection(&snap, "openai")?
            .ok_or_else(|| provider_not_configured("openai"))?;
        Ok(ResolvedRoute {
            routing: Default::default(),
            backend: self.backend_for(conn)?,
            stripped_model: String::new(),
            resolution: resolution_for(conn, String::new()),
            capabilities: capabilities_for(&self.capability_catalog, conn, "")?,
        })
    }

    async fn provider_connection(
        &self,
        provider_kind: &str,
    ) -> Result<Option<StoredConnection>, BackendError> {
        let snap = self.snapshot().await;
        Ok(select_implicit_connection(&snap, provider_kind)?.cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::model_routes::{InMemoryModelRouteStore, RouteInput};
    use crate::provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore,
    };

    #[test]
    fn shutdown_dates_exclude_only_models_with_a_valid_past_date() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-14T12:00:00Z")
            .unwrap()
            .to_utc();
        for (shutdown_date, expected) in [
            (None, false),
            (Some(serde_json::Value::Null), false),
            (Some(serde_json::json!("2026-07-23")), true),
            (Some(serde_json::json!("2026-09-14")), true),
            (Some(serde_json::json!("2026-09-15")), false),
            (Some(serde_json::json!("2026-02-30")), false),
            (Some(serde_json::json!("invalid")), false),
            (Some(serde_json::json!(123)), false),
        ] {
            let mut value = serde_json::json!({"id": "test", "object": "model"});
            if let Some(date) = shutdown_date {
                value["shutdown_date"] = date;
            }
            let model: Model = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(model_is_shutdown(&model, now), expected, "{value}");
        }
        let model: Model = serde_json::from_value(serde_json::json!({
            "id": "test", "object": "model", "shutdown_date": "2026-09-14"
        }))
        .unwrap();
        let midnight = chrono::DateTime::parse_from_rfc3339("2026-09-14T00:00:00Z")
            .unwrap()
            .to_utc();
        assert!(!model_is_shutdown(&model, midnight));
        assert!(model_is_shutdown(
            &model,
            midnight + chrono::Duration::seconds(1)
        ));
    }

    #[tokio::test]
    async fn model_listing_filters_shutdown_metadata_before_publishing_capabilities() {
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({"object": "list", "data": [
                    {"id": "retired", "object": "model", "shutdown_date": "2000-01-01"},
                    {"id": "future", "object": "model", "shutdown_date": "9999-12-31"},
                    {"id": "null", "object": "model", "shutdown_date": null},
                    {"id": "missing", "object": "model"},
                    {"id": "invalid", "object": "model", "shutdown_date": "invalid"}
                ]}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store_conn(&store, "openai", Some(&base_url)).await;
        let router = router(store);
        // Filtering also applies when the raw provider catalog comes from cache.
        for _ in 0..2 {
            let listed = router.list_models().await.unwrap();
            assert_eq!(
                listed
                    .data
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>(),
                [
                    "openai/future",
                    "openai/null",
                    "openai/missing",
                    "openai/invalid"
                ]
            );
            assert!(listed.data.iter().all(|model| model.capabilities.is_some()));
        }
        server.abort();
    }

    #[tokio::test]
    async fn request_fallback_names_match_ids_and_id_entries_win() {
        use crate::route_grid::{target, FallbackMap};

        let connections: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store_conn(&connections, "openai", None).await;
        let id = connections.list().await.unwrap()[0].id().to_string();
        let router = router(connections);
        let named = FallbackMap::from([(
            "openai/mock/e100".into(),
            vec!["openai/mock/r100".into(), target(&id, "mock/fast")],
        )]);
        let normalized = router.normalize_request_fallbacks(&named).await.unwrap();
        assert_eq!(
            normalized,
            FallbackMap::from([(
                target(&id, "mock/e100"),
                vec![target(&id, "mock/r100"), target(&id, "mock/fast")],
            )])
        );
        // An explicit ID entry (including an empty chain) wins over the name.
        let mut both = named;
        both.insert(target(&id, "mock/e100"), vec![]);
        both.insert("missing/model".into(), vec!["openai/mock/fast".into()]);
        let normalized = router.normalize_request_fallbacks(&both).await.unwrap();
        assert!(normalized[&target(&id, "mock/e100")].is_empty());
        assert_eq!(normalized["missing/model"], [target(&id, "mock/fast")]);

        for alternatives in [
            vec![target(&id, "mock/e100")],
            vec!["openai/mock/fast".into(), target(&id, "mock/fast")],
        ] {
            let overrides = FallbackMap::from([("openai/mock/e100".into(), alternatives)]);
            assert!(router
                .normalize_request_fallbacks(&overrides)
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn request_fallback_connection_id_wins_over_another_connections_name() {
        use crate::route_grid::{target, FallbackMap};

        let connections: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store_conn(&connections, "openai", None).await;
        let id = connections.list().await.unwrap()[0].id().to_string();
        connections
            .create(ConnectionInput {
                name: id.clone(),
                provider_kind: "openai".into(),
                base_url: None,
                prefix: None,
                exposure: ExposurePolicy::Explicit,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "unused".into(),
            })
            .await
            .unwrap();
        let overrides = FallbackMap::from([(target(&id, "primary"), vec![target(&id, "backup")])]);
        assert_eq!(
            router(connections)
                .normalize_request_fallbacks(&overrides)
                .await
                .unwrap(),
            overrides
        );
    }

    /// A router over the given connection store and an empty route store.
    fn router(connections: Arc<dyn ProviderConnectionStore>) -> RouterBackend {
        RouterBackend::new(
            reqwest::Client::new(),
            connections,
            Arc::new(InMemoryModelRouteStore::new()),
        )
    }

    /// Store a built-in connection (name/prefix derived from the kind), as the
    /// Providers UI / seeder would.
    async fn store_conn(
        store: &Arc<dyn ProviderConnectionStore>,
        kind: &str,
        base_url: Option<&str>,
    ) {
        store
            .create(ConnectionInput {
                name: kind.to_string(),
                provider_kind: kind.to_string(),
                base_url: base_url.map(str::to_string),
                prefix: Some(format!("{kind}/")),
                exposure: ExposurePolicy::Dynamic,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "test-key".to_string(),
            })
            .await
            .unwrap();
    }

    /// A router whose only route is a published `chalk/gemma` served by an
    /// `openai-compatible` connection named `chalk-gemma`. Returns the router and
    /// the backing connection id.
    async fn chalk_gemma_router() -> (RouterBackend, String) {
        let connections: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        let conn = connections
            .create(ConnectionInput {
                name: "chalk-gemma".to_string(),
                provider_kind: OPENAI_COMPATIBLE_KIND.to_string(),
                base_url: Some("https://gemma.internal/v1".to_string()),
                prefix: Some("chalk-gemma/".to_string()),
                exposure: ExposurePolicy::Explicit,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "unused".to_string(),
            })
            .await
            .unwrap();
        let routes: Arc<dyn ModelRouteStore> = Arc::new(InMemoryModelRouteStore::new());
        routes
            .create(RouteInput {
                routing: Default::default(),
                public_model_id: "chalk/gemma".to_string(),
                connection_id: conn.id.clone(),
                upstream_model_id: "gemma-3-27b-it".to_string(),
                published: true,
            })
            .await
            .unwrap();
        (
            RouterBackend::new(reqwest::Client::new(), connections, routes),
            conn.id,
        )
    }

    #[tokio::test]
    async fn connection_model_catalog_uses_cached_dynamic_models() {
        let connections: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        let conn = connections
            .create(ConnectionInput {
                name: "openai".to_string(),
                provider_kind: "openai".to_string(),
                base_url: None,
                prefix: Some("openai/".to_string()),
                exposure: ExposurePolicy::Dynamic,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "test-key".to_string(),
            })
            .await
            .unwrap();
        let router = router(connections.clone());
        let stored = connections.get(&conn.id).await.unwrap().unwrap();
        let StoredConnection::Readable(readable) = stored else {
            panic!("test connection must be readable");
        };
        router.models_cache.store(
            "openai",
            &connection_identity(&readable),
            PrefixedModelList {
                models: vec![Model {
                    id: "openai/gpt-5".to_string(),
                    object: "model".to_string(),
                    created: 0,
                    owned_by: "openai".to_string(),
                    capabilities: None,
                    provider_metadata: Default::default(),
                }],
                has_more: false,
            },
        );

        let models = router
            .list_provider_connection_models(std::slice::from_ref(&conn.id))
            .await
            .unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].connection_id, conn.id);
        assert_eq!(models[0].public_model_id, "openai/gpt-5");
        assert_eq!(models[0].upstream_model_id, "gpt-5");
        assert!(models[0].capabilities.is_some());
    }

    #[tokio::test]
    async fn resolved_route_carries_the_connection_capability_declaration() {
        use crate::capabilities::{CapabilityPatch, ModelFeature, Support};

        let connections: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        let declared: CapabilityPatch = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "model_defaults": {
                "features": {"tool_calling": "unsupported"}
            }
        }))
        .unwrap();
        connections
            .create(ConnectionInput {
                name: "custom".to_string(),
                provider_kind: OPENAI_COMPATIBLE_KIND.to_string(),
                base_url: Some("https://custom.internal/v1".to_string()),
                prefix: Some("custom/".to_string()),
                exposure: ExposurePolicy::Dynamic,
                routing_enabled: true,
                declared_capabilities: Some(declared),
                api_key: "unused".to_string(),
            })
            .await
            .unwrap();

        let resolved = router(connections).resolve("custom/model").await.unwrap();
        assert_eq!(
            resolved.capabilities.profile.features[&ModelFeature::ToolCalling],
            Support::Unsupported
        );
        assert!(resolved.capabilities.evidence.iter().any(|evidence| {
            evidence.source == crate::capabilities::CapabilitySource::ConnectionDeclaration
        }));
    }

    #[test]
    fn connection_capability_resolution_distinguishes_bedrock_runtime_and_mantle() {
        use crate::capabilities::{DeliveryMode, Protocol, ProtocolOperation};

        let catalog = crate::capabilities::CapabilityCatalog::new();
        let runtime = resolve_connection_capabilities(
            &catalog,
            "runtime-connection",
            "bedrock",
            Some("https://bedrock-runtime.us-west-2.amazonaws.com"),
            "anthropic.claude-sonnet-4",
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            runtime
                .profile
                .operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
                .unwrap()
                .mode,
            DeliveryMode::Adapted
        );

        let mantle = resolve_connection_capabilities(
            &catalog,
            "mantle-connection",
            "bedrock",
            Some("https://bedrock-proxy.internal"),
            "anthropic.claude-sonnet-4",
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            mantle
                .profile
                .operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
                .unwrap()
                .mode,
            DeliveryMode::Translated
        );
    }

    #[tokio::test]
    async fn cold_load_snapshot_indexes_a_stored_connection() {
        // A connection stored before the first resolution is visible: the cold
        // build lists the store and indexes it by kind and by prefix.
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store_conn(&store, "openai", None).await;
        let router = router(store);

        let snap = router.resolution.snapshot().await;
        assert!(snap.conns.by_kind.contains_key("openai"));
        assert!(snap.conns.by_prefix.contains_key("openai/"));

        // And routing resolves through that same in-memory snapshot.
        let (backend, model) = router.route_model("openai/gpt-4").await.unwrap();
        assert_eq!(backend.name(), "openai");
        assert_eq!(model, "gpt-4");
    }

    #[tokio::test]
    async fn native_anthropic_resolution_strips_qualified_models_and_preserves_bare_models() {
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store_conn(&store, "anthropic", None).await;
        let router = router(store);

        let qualified = router
            .resolve_anthropic("anthropic/claude-sonnet-5")
            .await
            .unwrap();
        assert_eq!(qualified.stripped_model, "claude-sonnet-5");
        assert_eq!(qualified.resolution.provider_kind, "anthropic");

        let bare = router.resolve_anthropic("claude-sonnet-5").await.unwrap();
        assert_eq!(bare.stripped_model, "claude-sonnet-5");
        assert_eq!(bare.resolution.provider_kind, "anthropic");
    }

    #[tokio::test]
    async fn native_anthropic_resolution_supports_custom_prefixes_and_routes() {
        let connections: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        let conn = connections
            .create(ConnectionInput {
                name: "claude-primary".to_string(),
                provider_kind: "anthropic".to_string(),
                base_url: None,
                prefix: Some("claude/".to_string()),
                exposure: ExposurePolicy::Dynamic,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "test-key".to_string(),
            })
            .await
            .unwrap();
        let routes: Arc<dyn ModelRouteStore> = Arc::new(InMemoryModelRouteStore::new());
        routes
            .create(RouteInput {
                routing: Default::default(),
                public_model_id: "chalk/sonnet".to_string(),
                connection_id: conn.id,
                upstream_model_id: "claude-sonnet-5".to_string(),
                published: true,
            })
            .await
            .unwrap();
        let router = RouterBackend::new(reqwest::Client::new(), connections, routes);

        let prefixed = router
            .resolve_anthropic("claude/claude-sonnet-5")
            .await
            .unwrap();
        assert_eq!(prefixed.stripped_model, "claude-sonnet-5");

        let routed = router.resolve_anthropic("chalk/sonnet").await.unwrap();
        assert_eq!(routed.stripped_model, "claude-sonnet-5");
        assert_eq!(routed.resolution.connection_name, "claude-primary");
    }

    #[tokio::test]
    async fn anthropic_ingress_resolution_preserves_non_anthropic_route_capabilities() {
        let (router, _) = chalk_gemma_router().await;
        let route = router
            .resolve_anthropic("chalk/gemma")
            .await
            .expect("endpoint capability is checked by the handler");
        assert_eq!(route.resolution.provider_kind, OPENAI_COMPATIBLE_KIND);
        assert_eq!(route.stripped_model, "gemma-3-27b-it");
        assert_eq!(
            route
                .capabilities
                .profile
                .operation(
                    crate::capabilities::Protocol::AnthropicMessages,
                    crate::capabilities::ProtocolOperation::Create,
                )
                .unwrap()
                .mode,
            crate::capabilities::DeliveryMode::Unknown
        );
    }

    #[tokio::test]
    async fn provider_connection_resolves_by_kind_from_the_snapshot() {
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store_conn(&store, "openai", None).await;
        store_conn(&store, "anthropic", None).await;
        let router = router(store);

        let conn = router.provider_connection("anthropic").await.unwrap();
        assert_eq!(conn.unwrap().provider_kind(), "anthropic");
        assert!(router
            .provider_connection("gemini")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn provider_connection_excludes_explicit_exposure_connections() {
        // Provider-native entrypoints are implicit routing too: an Explicit
        // connection must remain reachable only through a model route.
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store
            .create(ConnectionInput {
                name: "anthropic".to_string(),
                provider_kind: "anthropic".to_string(),
                base_url: None,
                prefix: Some("anthropic/".to_string()),
                exposure: ExposurePolicy::Explicit,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "sk-ant".to_string(),
            })
            .await
            .unwrap();
        let router = router(store);

        let snap = router.resolution.snapshot().await;
        assert!(snap.conns.by_kind.contains_key("anthropic"));
        assert!(router
            .provider_connection("anthropic")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn implicit_openai_prefers_canonical_then_falls_back_to_sole_connection() {
        let sole_store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        sole_store
            .create(ConnectionInput {
                name: "openai-dev".to_string(),
                provider_kind: "openai".to_string(),
                base_url: None,
                prefix: Some("openai-dev/".to_string()),
                exposure: ExposurePolicy::Dynamic,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "dev-key".to_string(),
            })
            .await
            .unwrap();
        let sole = router(sole_store).resolve_route("gpt-4o").await.unwrap();
        assert_eq!(sole.connection_name, "openai-dev");

        let canonical_store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        canonical_store
            .create(ConnectionInput {
                name: "openai-dev".to_string(),
                provider_kind: "openai".to_string(),
                base_url: None,
                prefix: Some("openai-dev/".to_string()),
                exposure: ExposurePolicy::Dynamic,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "dev-key".to_string(),
            })
            .await
            .unwrap();
        canonical_store
            .create(ConnectionInput {
                name: "openai-prod".to_string(),
                provider_kind: "openai".to_string(),
                base_url: None,
                prefix: Some("openai/".to_string()),
                exposure: ExposurePolicy::Unlisted,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "prod-key".to_string(),
            })
            .await
            .unwrap();
        let canonical = router(canonical_store)
            .resolve_route("gpt-4o")
            .await
            .unwrap();
        assert_eq!(canonical.connection_name, "openai-prod");
    }

    #[tokio::test]
    async fn implicit_openai_does_not_fall_back_from_a_disabled_canonical_owner() {
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store
            .create(ConnectionInput {
                name: "openai-preview".to_string(),
                provider_kind: "openai".to_string(),
                base_url: None,
                prefix: Some("openai-preview/".to_string()),
                exposure: ExposurePolicy::Dynamic,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "preview-key".to_string(),
            })
            .await
            .unwrap();
        store
            .create(ConnectionInput {
                name: "openai-prod".to_string(),
                provider_kind: "openai".to_string(),
                base_url: None,
                prefix: Some("openai/".to_string()),
                exposure: ExposurePolicy::Dynamic,
                routing_enabled: false,
                declared_capabilities: None,
                api_key: "prod-key".to_string(),
            })
            .await
            .unwrap();

        let err = router(store).resolve_route("gpt-4o").await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "Configuration error: provider connection 'openai-prod' is disabled"
        );
    }

    #[tokio::test]
    async fn implicit_selection_rejects_ambiguous_same_kind_connections() {
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        for suffix in ["prod", "dev"] {
            store
                .create(ConnectionInput {
                    name: format!("openai-{suffix}"),
                    provider_kind: "openai".to_string(),
                    base_url: None,
                    prefix: Some(format!("openai-{suffix}/")),
                    exposure: ExposurePolicy::Dynamic,
                    routing_enabled: true,
                    declared_capabilities: None,
                    api_key: format!("{suffix}-key"),
                })
                .await
                .unwrap();
        }

        let err = router(store).resolve_route("gpt-4o").await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "Configuration error: multiple 'openai' connections are configured, but none owns the canonical prefix 'openai/'; assign that prefix or route explicitly"
        );
    }

    #[tokio::test]
    async fn implicit_anthropic_uses_the_same_canonical_or_sole_rule() {
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        for (name, prefix) in [
            ("anthropic-dev", "anthropic-dev/"),
            ("anthropic-prod", "anthropic/"),
        ] {
            store
                .create(ConnectionInput {
                    name: name.to_string(),
                    provider_kind: "anthropic".to_string(),
                    base_url: None,
                    prefix: Some(prefix.to_string()),
                    exposure: ExposurePolicy::Dynamic,
                    routing_enabled: true,
                    declared_capabilities: None,
                    api_key: format!("{name}-key"),
                })
                .await
                .unwrap();
        }

        let selected = router(store)
            .provider_connection("anthropic")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(selected.name(), "anthropic-prod");
    }

    #[tokio::test]
    async fn openai_catch_all_is_unconfigured_until_a_connection_is_stored() {
        let router = router(Arc::new(InMemoryProviderConnectionStore::new()));

        let err = match router.route_model("openai/gpt-4").await {
            Ok((backend, model)) => {
                panic!("expected config error, got {} for {model}", backend.name())
            }
            Err(err) => err,
        };

        assert!(matches!(err, BackendError::ConfigError(_)));
        assert_eq!(
            err.to_string(),
            "Configuration error: provider 'openai' is not configured"
        );
    }

    #[tokio::test]
    async fn unknown_prefix_without_an_openai_connection_errors_on_the_catch_all() {
        // With no connection owning `anthropic/` and no OpenAI catch-all, an
        // unrecognized prefix surfaces the OpenAI "not configured" error.
        let router = router(Arc::new(InMemoryProviderConnectionStore::new()));

        let err = match router.route_model("anthropic/claude-haiku-4-5").await {
            Ok((backend, model)) => {
                panic!("expected config error, got {} for {model}", backend.name())
            }
            Err(err) => err,
        };

        assert!(matches!(err, BackendError::ConfigError(_)));
        assert_eq!(
            err.to_string(),
            "Configuration error: provider 'openai' is not configured"
        );
    }

    #[tokio::test]
    async fn unknown_prefix_falls_through_to_openai_catch_all_unchanged() {
        // A prefix owned by no connection routes to the OpenAI catch-all with the
        // model name left fully intact (backward compatibility).
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store_conn(&store, "openai", None).await;
        let router = router(store);

        let (backend, model) = router.route_model("some-model/v2").await.unwrap();

        assert_eq!(backend.name(), "openai");
        assert_eq!(model, "some-model/v2");
    }

    #[tokio::test]
    async fn bare_model_name_routes_to_openai_catch_all_unchanged() {
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store_conn(&store, "openai", None).await;
        let router = router(store);

        let (backend, model) = router.route_model("gpt-4").await.unwrap();

        assert_eq!(backend.name(), "openai");
        assert_eq!(model, "gpt-4");
    }

    #[tokio::test]
    async fn stored_anthropic_connection_uses_its_base_url_override() {
        let backend = build_provider_adapter(
            reqwest::Client::new(),
            "anthropic",
            "anthropic",
            "test-key",
            Some("http://127.0.0.1:9/custom"),
        )
        .expect("build Anthropic backend");

        let error = backend
            .list_models()
            .await
            .expect_err("loopback connection refused");

        match error {
            BackendError::RequestError(error) => {
                let url = error.url().expect("request URL");
                assert_eq!(url.host_str(), Some("127.0.0.1"));
                assert_eq!(url.port(), Some(9));
                assert_eq!(url.path(), "/custom/models");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// A minimal service account key. `route_model` builds the live backend, so
    /// a `vertex` connection needs a credential that actually parses — the
    /// project it names is what the backend routes to.
    const VERTEX_SA_KEY: &str = r#"{
        "type": "service_account",
        "project_id": "my-proj",
        "private_key": "-----BEGIN PRIVATE KEY-----\nMIIB\n-----END PRIVATE KEY-----\n",
        "client_email": "router@my-proj.iam.gserviceaccount.com"
    }"#;

    #[tokio::test]
    async fn stored_vertex_connection_routes_and_strips_prefix() {
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store
            .create(ConnectionInput {
                name: "vertex".to_string(),
                provider_kind: "vertex".to_string(),
                base_url: None,
                prefix: Some("vertex/".to_string()),
                exposure: ExposurePolicy::Dynamic,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: VERTEX_SA_KEY.to_string(),
            })
            .await
            .unwrap();
        let router = router(store);

        let (backend, model) = router.route_model("vertex/gemini-2.5-pro").await.unwrap();

        assert_eq!(backend.name(), "vertex");
        // The publisher is applied at dispatch, not here: `gemini/` and
        // `vertex/` must strip to the same upstream id.
        assert_eq!(model, "gemini-2.5-pro");
    }

    #[tokio::test]
    async fn vertex_connection_with_an_unparseable_credential_fails_to_build() {
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        // A Gemini AI Studio API key pasted into a Vertex connection — the
        // mixup this kind exists to make legible.
        store_conn(&store, "vertex", None).await;
        let router = router(store);

        let err = match router.route_model("vertex/gemini-2.5-pro").await {
            Err(err) => err,
            Ok(_) => panic!("an API key must not build a vertex backend"),
        };

        assert!(
            err.to_string().contains("service account key"),
            "unhelpful error: {err}"
        );
    }

    #[tokio::test]
    async fn stored_builtin_connections_route_and_strip_prefixes() {
        // A routing-enabled built-in connection strips its prefix and routes to
        // a backend named for the connection; this holds identically across
        // adapter kinds (openai/gemini share `AdapterKind::OpenAiCompatible`,
        // while Gemini, Voyage and TypeSafe have native adapters), so one table covers
        // all of them.
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        for provider in [
            "openai",
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
            store_conn(&store, provider, None).await;
        }
        store_conn(&store, "vllm", Some("http://localhost:8000")).await;
        let router = router(store);

        for (provider, model) in [
            ("openai", "gpt-4"),
            ("gemini", "gemini-2.5-flash"),
            ("voyage", "voyage-3"),
            ("typesafe", "jev-1.13.0"),
            ("vllm", "local-model"),
            ("bedrock", "openai.gpt-oss-120b-1:0"),
            ("openrouter", "anthropic/claude-sonnet-4"),
            ("xai", "grok-4"),
            ("cohere", "command-a"),
            ("qwen", "qwen-plus"),
            (
                "fireworks",
                "accounts/fireworks/models/llama-v3p1-8b-instruct",
            ),
            ("together", "meta-llama/Llama-3.3-70B-Instruct-Turbo"),
            ("groq", "llama-3.3-70b-versatile"),
            ("mistral", "mistral-large-latest"),
            ("perplexity", "pplx-decider-v1.2"),
        ] {
            let prefixed_model = format!("{provider}/{model}");
            let (backend, stripped_model) = router.route_model(&prefixed_model).await.unwrap();

            assert_eq!(backend.name(), provider);
            assert_eq!(stripped_model, model);
        }
    }

    #[tokio::test]
    async fn custom_openai_compatible_prefix_routes_and_strips() {
        // An `openai-compatible` connection with a bespoke prefix (e.g. what the
        // old hardcoded `chalk-gemma/` becomes) routes by that prefix.
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store
            .create(ConnectionInput {
                name: "chalk-gemma".to_string(),
                provider_kind: OPENAI_COMPATIBLE_KIND.to_string(),
                base_url: Some("https://gemma.internal/v1".to_string()),
                prefix: Some("chalk-gemma/".to_string()),
                exposure: ExposurePolicy::Unlisted,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "unused".to_string(),
            })
            .await
            .unwrap();
        let router = router(store);

        let (backend, model) = router
            .route_model("chalk-gemma/gemma-3-27b-it")
            .await
            .unwrap();

        assert_eq!(backend.name(), "chalk-gemma");
        assert_eq!(model, "gemma-3-27b-it");
    }

    #[tokio::test]
    async fn stored_vllm_connection_requires_base_url() {
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store_conn(&store, "vllm", None).await;
        let router = router(store);

        let error = match router.route_model("vllm/local-model").await {
            Ok((backend, model)) => {
                panic!("expected config error, got {} for {model}", backend.name())
            }
            Err(error) => error,
        };

        assert!(matches!(error, BackendError::ConfigError(_)));
        assert_eq!(
            error.to_string(),
            "Configuration error: provider 'vllm' requires a base URL"
        );
    }

    #[tokio::test]
    async fn published_chalk_route_resolves_to_its_connection_and_upstream_model() {
        let connections: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        let conn = connections
            .create(ConnectionInput {
                name: "chalk-gemma".to_string(),
                provider_kind: OPENAI_COMPATIBLE_KIND.to_string(),
                base_url: Some("https://gemma.internal/v1".to_string()),
                prefix: Some("chalk-gemma/".to_string()),
                exposure: ExposurePolicy::Explicit,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "unused".to_string(),
            })
            .await
            .unwrap();
        let routes: Arc<dyn ModelRouteStore> = Arc::new(InMemoryModelRouteStore::new());
        routes
            .create(RouteInput {
                routing: Default::default(),
                public_model_id: "chalk/gemma".to_string(),
                connection_id: conn.id,
                upstream_model_id: "gemma-3-27b-it".to_string(),
                published: true,
            })
            .await
            .unwrap();
        let router = RouterBackend::new(reqwest::Client::new(), connections, routes);

        let (backend, model) = router.route_model("chalk/gemma").await.unwrap();

        assert_eq!(backend.name(), "chalk-gemma");
        assert_eq!(model, "gemma-3-27b-it");
    }

    /// Wraps a store and marks one route's stored routing as unreadable.
    struct UnreadableRoutingStore {
        inner: InMemoryModelRouteStore,
        bad_public_id: &'static str,
    }

    #[async_trait::async_trait]
    impl ModelRouteStore for UnreadableRoutingStore {
        async fn create(
            &self,
            input: RouteInput,
        ) -> Result<ModelRoute, crate::model_routes::RouteStoreError> {
            self.inner.create(input).await
        }
        async fn update(
            &self,
            id: &str,
            upd: crate::model_routes::RouteUpdate,
        ) -> Result<ModelRoute, crate::model_routes::RouteStoreError> {
            self.inner.update(id, upd).await
        }
        async fn delete(
            &self,
            id: &str,
        ) -> Result<Option<String>, crate::model_routes::RouteStoreError> {
            self.inner.delete(id).await
        }
        async fn list(&self) -> Result<Vec<ModelRoute>, crate::model_routes::RouteStoreError> {
            let mut routes = self.inner.list().await?;
            for route in &mut routes {
                if route.public_model_id == self.bad_public_id {
                    route.routing = None;
                    route.routing_error = Some("invalid stored routing: boom".to_string());
                }
            }
            Ok(routes)
        }
        async fn get_by_public_id(
            &self,
            public_model_id: &str,
        ) -> Result<Option<ModelRoute>, crate::model_routes::RouteStoreError> {
            self.inner.get_by_public_id(public_model_id).await
        }
        async fn list_by_connection(
            &self,
            connection_id: &str,
        ) -> Result<Vec<ModelRoute>, crate::model_routes::RouteStoreError> {
            self.inner.list_by_connection(connection_id).await
        }
    }

    #[tokio::test]
    async fn route_with_unreadable_stored_routing_fails_alone() {
        let connections: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        let conn = connections
            .create(ConnectionInput {
                name: "chalk-gemma".to_string(),
                provider_kind: OPENAI_COMPATIBLE_KIND.to_string(),
                base_url: Some("https://gemma.internal/v1".to_string()),
                prefix: None,
                exposure: ExposurePolicy::Explicit,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "unused".to_string(),
            })
            .await
            .unwrap();
        let inner = InMemoryModelRouteStore::new();
        for name in ["chalk/good", "chalk/bad"] {
            inner
                .create(RouteInput {
                    routing: Default::default(),
                    public_model_id: name.to_string(),
                    connection_id: conn.id.clone(),
                    upstream_model_id: "gemma-3-27b-it".to_string(),
                    published: true,
                })
                .await
                .unwrap();
        }
        let routes: Arc<dyn ModelRouteStore> = Arc::new(UnreadableRoutingStore {
            inner,
            bad_public_id: "chalk/bad",
        });
        let router = RouterBackend::new(reqwest::Client::new(), connections, routes);

        let (_, model) = router.route_model("chalk/good").await.unwrap();
        assert_eq!(model, "gemma-3-27b-it");

        let err = match router.route_model("chalk/bad").await {
            Ok((backend, model)) => {
                panic!("expected config error, got {} for {model}", backend.name())
            }
            Err(err) => err,
        };
        assert!(matches!(err, BackendError::ConfigError(_)));
        assert_eq!(
            err.to_string(),
            "Configuration error: model route 'chalk/bad' has stored routing this server cannot read"
        );
    }

    #[tokio::test]
    async fn unpublished_chalk_route_returns_retryable_error() {
        let connections: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        let conn = connections
            .create(ConnectionInput {
                name: "chalk-gemma".to_string(),
                provider_kind: OPENAI_COMPATIBLE_KIND.to_string(),
                base_url: Some("https://gemma.internal/v1".to_string()),
                prefix: None,
                exposure: ExposurePolicy::Explicit,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "unused".to_string(),
            })
            .await
            .unwrap();
        let routes: Arc<dyn ModelRouteStore> = Arc::new(InMemoryModelRouteStore::new());
        routes
            .create(RouteInput {
                routing: Default::default(),
                public_model_id: "chalk/gemma".to_string(),
                connection_id: conn.id,
                upstream_model_id: "gemma-3-27b-it".to_string(),
                published: false,
            })
            .await
            .unwrap();
        let router = RouterBackend::new(reqwest::Client::new(), connections, routes);

        let err = match router.route_model("chalk/gemma").await {
            Ok((backend, model)) => {
                panic!("expected config error, got {} for {model}", backend.name())
            }
            Err(err) => err,
        };

        let BackendError::ApiError {
            status,
            message,
            headers,
            ..
        } = err
        else {
            panic!("expected retryable API error for unpublished route");
        };
        assert_eq!(status, 503);
        assert_eq!(message, "model route 'chalk/gemma' is not published");
        assert_eq!(
            headers,
            vec![(
                "retry-after".to_string(),
                RESOLUTION_TTL.as_secs().to_string()
            )]
        );
    }

    #[tokio::test]
    async fn missing_chalk_route_is_not_routable() {
        let router = router(Arc::new(InMemoryProviderConnectionStore::new()));

        let err = match router.route_model("chalk/nope").await {
            Ok((backend, model)) => {
                panic!("expected config error, got {} for {model}", backend.name())
            }
            Err(err) => err,
        };

        let BackendError::ApiError {
            status,
            message,
            headers,
            ..
        } = err
        else {
            panic!("expected not-found API error for missing route");
        };
        assert_eq!(status, 404);
        assert_eq!(message, "model route 'chalk/nope' does not exist");
        assert!(headers.is_empty());
    }

    #[tokio::test]
    async fn chalk_route_pointing_at_a_missing_connection_errors() {
        let connections: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        let routes: Arc<dyn ModelRouteStore> = Arc::new(InMemoryModelRouteStore::new());
        routes
            .create(RouteInput {
                routing: Default::default(),
                public_model_id: "chalk/gemma".to_string(),
                connection_id: uuid::Uuid::new_v4().to_string(),
                upstream_model_id: "gemma-3-27b-it".to_string(),
                published: true,
            })
            .await
            .unwrap();
        let router = RouterBackend::new(reqwest::Client::new(), connections, routes);

        let err = match router.route_model("chalk/gemma").await {
            Ok((backend, model)) => {
                panic!("expected config error, got {} for {model}", backend.name())
            }
            Err(err) => err,
        };

        assert!(matches!(err, BackendError::ConfigError(_)));
        assert_eq!(
            err.to_string(),
            "Configuration error: model route 'chalk/gemma' points at a connection that no longer exists"
        );
    }

    #[tokio::test]
    async fn resolve_route_attributes_to_the_connection_not_the_model_prefix() {
        // T1 regression: a `chalk/*` route attributes to its upstream connection
        // kind (`openai-compatible`) and identity, not the literal `chalk` prefix.
        let (router, conn_id) = chalk_gemma_router().await;

        let res = router.resolve_route("chalk/gemma").await.unwrap();

        assert_eq!(res.provider_kind, OPENAI_COMPATIBLE_KIND);
        assert_eq!(res.connection_id, Some(conn_id));
        assert_eq!(res.connection_name, "chalk-gemma");
        assert_eq!(res.upstream_model_id, "gemma-3-27b-it");
    }

    #[tokio::test]
    async fn resolve_route_openai_catch_all_reports_the_openai_connection() {
        // The bare-name catch-all attributes to the resolved OpenAI connection,
        // with the model left unchanged.
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store_conn(&store, "openai", None).await;
        let router = router(store);

        let res = router.resolve_route("gpt-4o").await.unwrap();

        assert_eq!(res.provider_kind, "openai");
        assert_eq!(res.connection_name, "openai");
        assert!(res.connection_id.is_some());
        assert_eq!(res.upstream_model_id, "gpt-4o");
    }

    #[tokio::test]
    async fn strict_resolution_rejects_unresolved_fallback_alternates() {
        // Strict resolution (used for fallback alternates) must NOT fall through
        // to the OpenAI catch-all: an unknown prefix or a bare name is unresolved,
        // even though an OpenAI connection exists.
        let store: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        store_conn(&store, "openai", None).await;
        let router = router(store);

        // Unknown prefix: lenient routing would hit the OpenAI catch-all.
        let err = router
            .resolve_route_strict("unknown/model")
            .await
            .unwrap_err();
        assert!(matches!(err, BackendError::ConfigError(_)));
        // Bare name: same — lenient hits OpenAI, strict fails.
        let err = router.resolve_route_strict("gpt-4o").await.unwrap_err();
        assert!(matches!(err, BackendError::ConfigError(_)));

        // A recognized connection prefix still resolves strictly.
        let res = router
            .resolve_route_strict("openai/gpt-5-mini")
            .await
            .unwrap();
        assert_eq!(res.provider_kind, "openai");
        assert_eq!(res.upstream_model_id, "gpt-5-mini");
    }

    #[tokio::test]
    async fn strict_resolution_allows_published_chalk_route() {
        let (router, conn_id) = chalk_gemma_router().await;

        let res = router.resolve_route_strict("chalk/gemma").await.unwrap();

        assert_eq!(res.provider_kind, OPENAI_COMPATIBLE_KIND);
        assert_eq!(res.connection_id, Some(conn_id));
        assert_eq!(res.upstream_model_id, "gemma-3-27b-it");
    }

    #[tokio::test]
    async fn explicit_openai_connection_is_not_reachable_via_catch_all() {
        // An `Explicit` openai connection must not back the catch-all: implicit
        // selection filters it, so a bare model, an unknown-prefix model, and
        // the OpenAI-only ops all surface "not configured". It stays reachable
        // only through a published `chalk/*` route, which resolves it by id.
        let connections: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::new());
        let conn = connections
            .create(ConnectionInput {
                name: "openai".to_string(),
                provider_kind: "openai".to_string(),
                base_url: None,
                prefix: Some("openai/".to_string()),
                exposure: ExposurePolicy::Explicit,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "test-key".to_string(),
            })
            .await
            .unwrap();
        let routes: Arc<dyn ModelRouteStore> = Arc::new(InMemoryModelRouteStore::new());
        routes
            .create(RouteInput {
                routing: Default::default(),
                public_model_id: "chalk/gpt".to_string(),
                connection_id: conn.id.clone(),
                upstream_model_id: "gpt-4o".to_string(),
                published: true,
            })
            .await
            .unwrap();
        let router = RouterBackend::new(reqwest::Client::new(), connections, routes);

        // Bare model: the catch-all is dead, so this is "not configured".
        let err = match router.route_model("gpt-4o").await {
            Ok((backend, model)) => {
                panic!("expected config error, got {} for {model}", backend.name())
            }
            Err(err) => err,
        };
        assert!(matches!(err, BackendError::ConfigError(_)));
        assert_eq!(
            err.to_string(),
            "Configuration error: provider 'openai' is not configured"
        );

        // Unknown prefix: falls through to the same dead catch-all.
        let err = match router.route_model("meta-llama/foo").await {
            Ok((backend, model)) => {
                panic!("expected config error, got {} for {model}", backend.name())
            }
            Err(err) => err,
        };
        assert!(matches!(err, BackendError::ConfigError(_)));
        assert_eq!(
            err.to_string(),
            "Configuration error: provider 'openai' is not configured"
        );

        // OpenAI-only ops (speech/transcription/translation/moderation, no-model
        // image) resolve the catch-all connection directly — also unconfigured.
        let err = match router.openai_backend().await {
            Ok(backend) => panic!("expected config error, got {}", backend.name()),
            Err(err) => err,
        };
        assert!(matches!(err, BackendError::ConfigError(_)));
        assert_eq!(
            err.to_string(),
            "Configuration error: provider 'openai' is not configured"
        );

        // But the published `chalk/*` route resolves it by id.
        let (backend, model) = router.route_model("chalk/gpt").await.unwrap();
        assert_eq!(backend.name(), "openai");
        assert_eq!(model, "gpt-4o");
    }

    // -- Backend cache: non-secret fingerprint + eviction ----------------------

    #[test]
    fn isolated_policy_refuses_to_build_a_keyless_bedrock_runtime_backend() {
        let now = chrono::Utc::now();
        let connection = |api_key: &str| ProviderConnection {
            id: "bedrock".into(),
            name: "bedrock".into(),
            provider_kind: "bedrock".into(),
            base_url: Some("https://bedrock-runtime.us-west-2.amazonaws.com".into()),
            prefix: Some("bedrock/".into()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: api_key.into(),
            created_at: now,
            updated_at: now,
        };
        let router = |policy| {
            RouterBackend::new(
                reqwest::Client::new(),
                Arc::new(InMemoryProviderConnectionStore::new()),
                Arc::new(InMemoryModelRouteStore::new()),
            )
            .with_connection_policy(policy)
        };
        let isolated = router(crate::provider_connections::ConnectionPolicy::isolated());
        let error = isolated
            .backend_for_connection(&connection(
                crate::provider_connections::KEYLESS_PLACEHOLDER,
            ))
            .err()
            .expect("keyless Bedrock Runtime must not build");
        assert!(error.to_string().contains("Bedrock API key"), "{error}");
        assert!(isolated
            .backend_for_connection(&connection("bedrock-key"))
            .is_ok());

        // A stored private endpoint is refused too, whatever path wrote the row.
        let mut custom = connection("k");
        custom.provider_kind = "openai".into();
        custom.base_url = Some("http://169.254.169.254/v1".into());
        let error = isolated
            .backend_for_connection(&custom)
            .err()
            .expect("a private endpoint must not build");
        assert!(error.to_string().contains("not allowed"), "{error}");
        assert!(
            router(crate::provider_connections::ConnectionPolicy::trusted())
                .backend_for_connection(&custom)
                .is_ok()
        );
        assert!(
            router(crate::provider_connections::ConnectionPolicy::trusted())
                .backend_for_connection(&connection(
                    crate::provider_connections::KEYLESS_PLACEHOLDER
                ))
                .is_ok()
        );
    }

    #[test]
    fn connection_fingerprint_is_stable_distinct_and_non_secret() {
        assert_ne!(
            connection_fingerprint("a|b", Some("c")),
            connection_fingerprint("a", Some("b|c")),
        );
        // Same pair -> same fingerprint (a cache hit); a rotated key or endpoint
        // -> a different fingerprint (forces a rebuild).
        assert_eq!(
            connection_fingerprint("k", Some("u")),
            connection_fingerprint("k", Some("u")),
        );
        assert_ne!(
            connection_fingerprint("k1", Some("u")),
            connection_fingerprint("k2", Some("u")),
        );
        assert_ne!(
            connection_fingerprint("k", Some("u1")),
            connection_fingerprint("k", Some("u2")),
        );
        // `None` (registry default) and `Some("")` (a literal empty override)
        // must not collide — they build different backends.
        assert_ne!(
            connection_fingerprint("k", None),
            connection_fingerprint("k", Some("")),
        );
        let secret = "sk-ant-plaintext-must-not-survive";
        let fingerprint = connection_fingerprint(secret, Some("https://api.anthropic.com/v1"));
        assert_eq!(fingerprint.len(), 64);
        assert!(!fingerprint.contains(secret));
    }

    #[tokio::test]
    async fn evicts_cached_backends_for_connections_absent_from_new_snapshot() {
        // The backend cache is insert-only per request; a snapshot refresh must
        // prune entries for connections that have since been deleted, so they
        // don't leak for the process lifetime.
        let router = router(Arc::new(InMemoryProviderConnectionStore::new()));
        let backend =
            build_provider_adapter(reqwest::Client::new(), "openai", "openai", "k", None).unwrap();
        router.cache.insert(
            "keep".to_string(),
            CachedBackend {
                fingerprint: "f1".to_string(),
                backend: backend.clone(),
            },
        );
        router.cache.insert(
            "drop".to_string(),
            CachedBackend {
                fingerprint: "f2".to_string(),
                backend,
            },
        );
        assert_eq!(router.cache.len(), 2);

        // A snapshot whose only connection is `keep` (as if `drop` was deleted).
        let now = chrono::Utc::now();
        let keep = StoredConnection::Readable(ProviderConnection {
            id: "keep".to_string(),
            name: "openai".to_string(),
            provider_kind: "openai".to_string(),
            base_url: None,
            prefix: Some("openai/".to_string()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "k".to_string(),
            created_at: now,
            updated_at: now,
        });
        let snap = build_snapshot(vec![keep], vec![]);

        router.evict_stale_backends(&snap);

        assert!(
            router.cache.contains_key("keep"),
            "present connection retained"
        );
        assert!(
            !router.cache.contains_key("drop"),
            "absent connection evicted"
        );
    }

    // -- ModelsCache: stale-while-refresh + single-flight ---------------------

    mod models_cache_tests {
        use super::*;
        use crate::types::{
            ChatCompletionRequest, ChatCompletionResponse, CreateEmbeddingRequest,
            CreateEmbeddingResponse, CreateImageRequest, CreateModerationRequest,
            CreateModerationResponse, CreateSpeechRequest, ImageResponse, TranscriptionResponse,
        };
        use std::sync::atomic::AtomicUsize;

        /// Backend that counts `list_models` calls and (optionally) sleeps to
        /// widen the concurrency window. When `fail` is set, the call errors
        /// (after counting + sleeping) to exercise the failing cold path. Only
        /// `list_models` is exercised here.
        struct CountingBackend {
            calls: Arc<AtomicUsize>,
            delay: Duration,
            fail: bool,
            list_has_more: bool,
            detail: DetailBehavior,
        }

        #[derive(Clone, Copy)]
        enum DetailBehavior {
            Unsupported,
            Found,
            NotFound,
            TransientFailure,
        }

        #[async_trait]
        impl BackendProvider for CountingBackend {
            async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if !self.delay.is_zero() {
                    tokio::time::sleep(self.delay).await;
                }
                if self.fail {
                    return Err(BackendError::api_status(503, "provider down"));
                }
                Ok(ListResponse {
                    object: "list".to_string(),
                    data: vec![Model {
                        id: "m1".to_string(),
                        object: "model".to_string(),
                        created: 0,
                        owned_by: "x".to_string(),
                        capabilities: None,
                        provider_metadata: Default::default(),
                    }],
                    first_id: None,
                    last_id: None,
                    has_more: self.list_has_more,
                })
            }
            async fn create_chat_completion(
                &self,
                _r: ChatCompletionRequest,
            ) -> Result<ChatCompletionResponse, BackendError> {
                Err(BackendError::Other("n/a".into()))
            }
            async fn create_embedding(
                &self,
                _r: CreateEmbeddingRequest,
            ) -> Result<CreateEmbeddingResponse, BackendError> {
                Err(BackendError::Other("n/a".into()))
            }
            async fn create_image(
                &self,
                _r: CreateImageRequest,
            ) -> Result<ImageResponse, BackendError> {
                Err(BackendError::Other("n/a".into()))
            }
            async fn create_speech(
                &self,
                _r: CreateSpeechRequest,
            ) -> Result<bytes::Bytes, BackendError> {
                Err(BackendError::Other("n/a".into()))
            }
            async fn create_transcription(
                &self,
                _m: String,
                _b: bytes::Bytes,
                _f: String,
                _l: Option<String>,
                _p: Option<String>,
                _rf: Option<String>,
                _t: Option<f32>,
            ) -> Result<TranscriptionResponse, BackendError> {
                Err(BackendError::Other("n/a".into()))
            }
            async fn create_translation(
                &self,
                _m: String,
                _b: bytes::Bytes,
                _f: String,
                _p: Option<String>,
                _rf: Option<String>,
                _t: Option<f32>,
            ) -> Result<TranscriptionResponse, BackendError> {
                Err(BackendError::Other("n/a".into()))
            }
            async fn create_moderation(
                &self,
                _r: CreateModerationRequest,
            ) -> Result<CreateModerationResponse, BackendError> {
                Err(BackendError::Other("n/a".into()))
            }
            async fn get_model(&self, id: &str) -> Result<Model, BackendError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if !self.delay.is_zero() {
                    tokio::time::sleep(self.delay).await;
                }
                match self.detail {
                    DetailBehavior::Unsupported => Err(BackendError::Other("n/a".into())),
                    DetailBehavior::Found => Ok(Model {
                        id: id.to_string(),
                        object: "model".to_string(),
                        created: 0,
                        owned_by: "counting".to_string(),
                        capabilities: None,
                        provider_metadata: Default::default(),
                    }),
                    DetailBehavior::NotFound => Err(BackendError::api_error(
                        format!("model '{id}' not found"),
                        404,
                        vec![("retry-after".to_string(), "30".to_string())],
                    )),
                    DetailBehavior::TransientFailure => {
                        Err(BackendError::api_status(503, "provider down"))
                    }
                }
            }
            fn name(&self) -> &str {
                "counting"
            }
        }

        fn backend(calls: Arc<AtomicUsize>, delay_ms: u64) -> Arc<dyn BackendProvider> {
            Arc::new(CountingBackend {
                calls,
                delay: Duration::from_millis(delay_ms),
                fail: false,
                list_has_more: false,
                detail: DetailBehavior::Unsupported,
            })
        }

        /// A backend whose `list_models` always fails (after counting), to
        /// exercise the failing cold path.
        fn failing_backend(calls: Arc<AtomicUsize>, delay_ms: u64) -> Arc<dyn BackendProvider> {
            Arc::new(CountingBackend {
                calls,
                delay: Duration::from_millis(delay_ms),
                fail: true,
                list_has_more: false,
                detail: DetailBehavior::Unsupported,
            })
        }

        fn detail_backend(
            calls: Arc<AtomicUsize>,
            delay_ms: u64,
            detail: DetailBehavior,
        ) -> Arc<dyn BackendProvider> {
            Arc::new(CountingBackend {
                calls,
                delay: Duration::from_millis(delay_ms),
                fail: false,
                list_has_more: false,
                detail,
            })
        }

        fn incomplete_backend(calls: Arc<AtomicUsize>) -> Arc<dyn BackendProvider> {
            Arc::new(CountingBackend {
                calls,
                delay: Duration::ZERO,
                fail: false,
                list_has_more: true,
                detail: DetailBehavior::Unsupported,
            })
        }

        fn cache(ttl: Duration) -> Arc<ModelsCache> {
            Arc::new(ModelsCache {
                entries: DashMap::new(),
                gates: DashMap::new(),
                inflight: DashMap::new(),
                ttl,
            })
        }

        #[tokio::test]
        async fn fresh_hit_does_not_refetch() {
            let c = cache(MODELS_CACHE_TTL);
            let calls = Arc::new(AtomicUsize::new(0));
            let first = c.get("openai", "id", backend(calls.clone(), 0)).await;
            assert_eq!(first.len(), 1);
            assert_eq!(first[0].id, "openai/m1"); // prefix re-applied by list_prefixed_models
            assert_eq!(calls.load(Ordering::SeqCst), 1); // cold fetch
                                                         // Second call within TTL is a pure cache hit — no new network call.
            let _ = c.get("openai", "id", backend(calls.clone(), 0)).await;
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn truncated_catalog_metadata_survives_prefixing_and_caching() {
            let c = cache(MODELS_CACHE_TTL);
            let calls = Arc::new(AtomicUsize::new(0));

            let first = c
                .get("openai", "id", incomplete_backend(calls.clone()))
                .await;
            let second = c
                .get("openai", "id", incomplete_backend(calls.clone()))
                .await;

            assert!(first.has_more);
            assert!(second.has_more);
            assert_eq!(first[0].id, "openai/m1");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn cold_concurrent_gets_coalesce_to_one_fetch() {
            let c = cache(MODELS_CACHE_TTL);
            let calls = Arc::new(AtomicUsize::new(0));
            // A burst of first-time requests for the same cold prefix; the slow
            // fetch keeps them overlapping. Single-flight => exactly one fetch.
            let futs = (0..8).map(|_| {
                let c = c.clone();
                let b = backend(calls.clone(), 40);
                async move { c.get("openai", "id", b).await }
            });
            let results = join_all(futs).await;
            assert!(results.iter().all(|r| r.len() == 1));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn cold_concurrent_gets_coalesce_to_one_fetch_even_on_failure() {
            let c = cache(MODELS_CACHE_TTL);
            let calls = Arc::new(AtomicUsize::new(0));
            // A burst of first-time requests for the same cold prefix whose fetch
            // FAILS. The slow failing fetch keeps them overlapping; single-flight
            // => exactly one upstream call, and every waiter shares the (empty)
            // result rather than each re-fetching (which used to be N calls).
            let futs = (0..8).map(|_| {
                let c = c.clone();
                let b = failing_backend(calls.clone(), 40);
                async move { c.get("openai", "id", b).await }
            });
            let results = join_all(futs).await;
            assert!(
                results.iter().all(|r| r.is_empty()),
                "a failed fetch yields an empty list"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "one upstream call despite 8 concurrent cold failures"
            );

            // The failure is NOT cached: a later, non-concurrent call retries and,
            // this time with a healthy backend, succeeds.
            let retry = c.get("openai", "id", backend(calls.clone(), 0)).await;
            assert_eq!(retry.len(), 1, "later call retries the cold fetch");
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "retry issued a second upstream call"
            );
        }

        #[tokio::test]
        async fn stale_serves_immediately_and_refreshes_in_background() {
            let c = cache(Duration::from_millis(1));
            let calls = Arc::new(AtomicUsize::new(0));
            // Prime (cold) then let it go stale past the 1ms TTL.
            c.get("openai", "id", backend(calls.clone(), 0)).await;
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            tokio::time::sleep(Duration::from_millis(10)).await;

            // Stale read with a SLOW refresh backend: it must return promptly
            // (serving stale) rather than blocking on the 60ms refetch.
            let started = Instant::now();
            let served = c.get("openai", "id", backend(calls.clone(), 60)).await;
            assert!(
                started.elapsed() < Duration::from_millis(50),
                "stale read blocked on refetch ({}ms)",
                started.elapsed().as_millis()
            );
            assert_eq!(served.len(), 1);

            // Exactly one background refresh eventually lands.
            for _ in 0..60 {
                if calls.load(Ordering::SeqCst) >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(calls.load(Ordering::SeqCst), 2, "one background refresh");
        }

        #[tokio::test]
        async fn stale_single_flight_spawns_only_one_refresh() {
            let c = cache(Duration::from_millis(1));
            let calls = Arc::new(AtomicUsize::new(0));
            c.get("openai", "id", backend(calls.clone(), 0)).await; // prime -> calls = 1
            tokio::time::sleep(Duration::from_millis(10)).await;

            // 8 concurrent stale reads: all serve stale synchronously; only one
            // background refresh is spawned (the slow refetch holds the gate).
            let futs = (0..8).map(|_| {
                let c = c.clone();
                let b = backend(calls.clone(), 60);
                async move { c.get("openai", "id", b).await }
            });
            let served = join_all(futs).await;
            assert!(served.iter().all(|r| r.len() == 1));

            for _ in 0..60 {
                if calls.load(Ordering::SeqCst) >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "single-flight: one prime + one refresh despite 8 stale reads"
            );
        }

        #[tokio::test]
        async fn changed_connection_identity_is_treated_as_cold() {
            // A fresh entry belongs to the connection identity that produced it.
            // A different identity at the same prefix (endpoint/key rotated, or
            // the connection deleted and replaced) must NOT be served that entry
            // — that would return a since-changed connection's catalog for up to
            // the TTL. A mismatch is treated as cold and refetched.
            let c = cache(MODELS_CACHE_TTL);
            let calls = Arc::new(AtomicUsize::new(0));

            c.get("openai", "conn-a|fpA", backend(calls.clone(), 0))
                .await;
            assert_eq!(calls.load(Ordering::SeqCst), 1, "cold fetch for identity A");

            // Same prefix, same identity within TTL: a pure hit, no refetch.
            c.get("openai", "conn-a|fpA", backend(calls.clone(), 0))
                .await;
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "same identity is a fresh hit"
            );

            // Same prefix, DIFFERENT identity: treated as cold -> refetch.
            c.get("openai", "conn-b|fpB", backend(calls.clone(), 0))
                .await;
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "changed identity forces a refetch instead of serving the stale catalog"
            );
        }

        #[tokio::test]
        async fn cold_gets_with_different_identities_do_not_coalesce() {
            // Single-flight coalesces only within one (prefix, identity). Two
            // concurrent cold fetches for the SAME prefix but DIFFERENT identities
            // (e.g. a connection rotated while the first fetch is still in flight)
            // must each hit the upstream — otherwise the new identity would join
            // and be served the previous connection's in-flight catalog.
            let c = cache(MODELS_CACHE_TTL);
            let calls = Arc::new(AtomicUsize::new(0));
            // A slow fetch keeps the two overlapping in the in-flight window.
            let a = {
                let c = c.clone();
                let b = backend(calls.clone(), 40);
                async move { c.get("openai", "conn-a|fpA", b).await }
            };
            let d = {
                let c = c.clone();
                let b = backend(calls.clone(), 40);
                async move { c.get("openai", "conn-b|fpB", b).await }
            };
            let (ra, rb) = tokio::join!(a, d);
            assert_eq!(ra.len(), 1);
            assert_eq!(rb.len(), 1);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "different identities each fetch; they do not share one in-flight future"
            );
        }

        fn details_cache(
            capacity: usize,
            found_ttl: Duration,
            not_found_ttl: Duration,
        ) -> Arc<ModelDetailsCache> {
            Arc::new(ModelDetailsCache::with_config(
                capacity,
                found_ttl,
                not_found_ttl,
            ))
        }

        #[tokio::test]
        async fn detail_cache_coalesces_concurrent_positive_misses() {
            let cache = details_cache(8, Duration::from_secs(60), Duration::from_secs(5));
            let calls = Arc::new(AtomicUsize::new(0));
            let futures = (0..8).map(|_| {
                let cache = cache.clone();
                let backend = detail_backend(calls.clone(), 40, DetailBehavior::Found);
                async move { cache.get("conn|fp", "model-a", backend).await }
            });

            let results = join_all(futures).await;

            assert!(results
                .iter()
                .all(|result| result.as_ref().is_ok_and(|m| m.id == "model-a")));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(cache.inflight.is_empty());
        }

        #[tokio::test]
        async fn detail_cache_negatively_caches_not_found() {
            let cache = details_cache(8, Duration::from_secs(60), Duration::from_secs(5));
            let calls = Arc::new(AtomicUsize::new(0));

            let first = cache
                .get(
                    "conn|fp",
                    "missing",
                    detail_backend(calls.clone(), 0, DetailBehavior::NotFound),
                )
                .await
                .expect_err("not found");
            let second = cache
                .get(
                    "conn|fp",
                    "missing",
                    detail_backend(calls.clone(), 0, DetailBehavior::NotFound),
                )
                .await
                .expect_err("cached not found");

            for error in [&first, &second] {
                assert!(matches!(error, BackendError::ApiError { status: 404, .. }));
            }
            assert!(matches!(
                first,
                BackendError::ApiError { ref headers, .. }
                    if headers == &vec![("retry-after".to_string(), "30".to_string())]
            ));
            assert!(matches!(
                second,
                BackendError::ApiError { ref headers, .. } if headers.is_empty()
            ));

            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn detail_cache_coalesces_concurrent_transient_failures() {
            let cache = details_cache(8, Duration::from_secs(60), Duration::from_secs(5));
            let calls = Arc::new(AtomicUsize::new(0));
            let futures = (0..8).map(|_| {
                let cache = cache.clone();
                let backend = detail_backend(calls.clone(), 40, DetailBehavior::TransientFailure);
                async move { cache.get("conn|fp", "model-a", backend).await }
            });

            let results = join_all(futures).await;

            assert!(results
                .iter()
                .all(|result| matches!(result, Err(BackendError::ApiError { status: 503, .. }))));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(cache.inflight.is_empty());

            let error = cache
                .get(
                    "conn|fp",
                    "model-a",
                    detail_backend(calls.clone(), 0, DetailBehavior::TransientFailure),
                )
                .await
                .expect_err("later transient retry");
            assert!(matches!(error, BackendError::ApiError { status: 503, .. }));
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        }

        #[tokio::test]
        async fn detail_cache_negative_entries_expire() {
            let cache = details_cache(8, Duration::from_secs(60), Duration::from_millis(1));
            let calls = Arc::new(AtomicUsize::new(0));

            cache
                .get(
                    "conn|fp",
                    "missing",
                    detail_backend(calls.clone(), 0, DetailBehavior::NotFound),
                )
                .await
                .expect_err("not found");
            tokio::time::sleep(Duration::from_millis(10)).await;
            cache
                .get(
                    "conn|fp",
                    "missing",
                    detail_backend(calls.clone(), 0, DetailBehavior::NotFound),
                )
                .await
                .expect_err("not found after expiry");

            assert_eq!(calls.load(Ordering::SeqCst), 2);
        }

        #[tokio::test]
        async fn detail_cache_does_not_cache_transient_failures() {
            let cache = details_cache(8, Duration::from_secs(60), Duration::from_secs(5));
            let calls = Arc::new(AtomicUsize::new(0));

            for _ in 0..2 {
                let error = cache
                    .get(
                        "conn|fp",
                        "model-a",
                        detail_backend(calls.clone(), 0, DetailBehavior::TransientFailure),
                    )
                    .await
                    .expect_err("transient failure");
                assert!(matches!(error, BackendError::ApiError { status: 503, .. }));
            }

            assert_eq!(calls.load(Ordering::SeqCst), 2);
        }

        #[tokio::test]
        async fn detail_cache_stays_bounded_without_evicting_admitted_models() {
            let cache = details_cache(2, Duration::from_millis(1), Duration::from_millis(1));
            let calls = Arc::new(AtomicUsize::new(0));

            cache
                .get(
                    "conn|fp",
                    "a",
                    detail_backend(calls.clone(), 0, DetailBehavior::Found),
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            cache
                .get(
                    "conn|fp",
                    "a",
                    detail_backend(calls.clone(), 0, DetailBehavior::Found),
                )
                .await
                .unwrap();
            cache
                .get(
                    "conn|fp",
                    "b",
                    detail_backend(calls.clone(), 0, DetailBehavior::Found),
                )
                .await
                .unwrap();
            cache
                .get(
                    "conn|fp",
                    "c",
                    detail_backend(calls.clone(), 0, DetailBehavior::Found),
                )
                .await
                .unwrap();
            cache
                .get(
                    "conn|fp",
                    "a",
                    detail_backend(calls.clone(), 0, DetailBehavior::Found),
                )
                .await
                .unwrap();

            assert_eq!(
                calls.load(Ordering::SeqCst),
                4,
                "the full cache declines c instead of evicting the admitted a entry"
            );
            assert!(cache.found.len() <= 2);
        }

        #[tokio::test]
        async fn detail_cache_isolated_by_connection_identity() {
            let cache = details_cache(8, Duration::from_secs(60), Duration::from_secs(5));
            let calls = Arc::new(AtomicUsize::new(0));

            cache
                .get(
                    "conn|fp-a",
                    "m",
                    detail_backend(calls.clone(), 0, DetailBehavior::Found),
                )
                .await
                .unwrap();
            cache
                .get(
                    "conn|fp-a",
                    "m",
                    detail_backend(calls.clone(), 0, DetailBehavior::Found),
                )
                .await
                .unwrap();
            cache
                .get(
                    "conn|fp-b",
                    "m",
                    detail_backend(calls.clone(), 0, DetailBehavior::Found),
                )
                .await
                .unwrap();

            assert_eq!(calls.load(Ordering::SeqCst), 2);
        }

        #[tokio::test]
        async fn unknown_ids_cannot_evict_positive_entries() {
            let cache = details_cache(1, Duration::from_secs(60), Duration::from_secs(5));
            let calls = Arc::new(AtomicUsize::new(0));

            cache
                .get(
                    "conn|fp",
                    "hot",
                    detail_backend(calls.clone(), 0, DetailBehavior::Found),
                )
                .await
                .unwrap();
            for id in ["missing-a", "missing-b"] {
                cache
                    .get(
                        "conn|fp",
                        id,
                        detail_backend(calls.clone(), 0, DetailBehavior::NotFound),
                    )
                    .await
                    .expect_err("not found");
            }
            cache
                .get(
                    "conn|fp",
                    "hot",
                    detail_backend(calls.clone(), 0, DetailBehavior::Found),
                )
                .await
                .unwrap();

            assert_eq!(
                calls.load(Ordering::SeqCst),
                3,
                "the hot model remained cached"
            );
            assert_eq!(cache.found.len(), 1);
            assert!(cache.not_found.len() <= 1);
        }

        #[tokio::test]
        async fn detail_cache_enforces_unequal_admission_limits_independently() {
            let cache = Arc::new(ModelDetailsCache::with_limits(
                2,
                1,
                1,
                Duration::from_secs(60),
                Duration::from_secs(5),
                Duration::from_secs(1),
            ));
            let calls = Arc::new(AtomicUsize::new(0));

            for id in ["positive-a", "positive-b"] {
                cache
                    .get(
                        "conn|fp",
                        id,
                        detail_backend(calls.clone(), 0, DetailBehavior::Found),
                    )
                    .await
                    .unwrap();
            }
            for id in ["negative-a", "negative-b"] {
                cache
                    .get(
                        "conn|fp",
                        id,
                        detail_backend(calls.clone(), 0, DetailBehavior::NotFound),
                    )
                    .await
                    .expect_err("not found");
            }

            assert_eq!(cache.found.len(), 2);
            assert_eq!(cache.not_found.len(), 1);
            assert_eq!(cache.found_slots.available_permits(), 0);
            assert_eq!(cache.not_found_slots.available_permits(), 0);

            let lookup = tokio::spawn({
                let cache = cache.clone();
                let backend = detail_backend(calls.clone(), 40, DetailBehavior::TransientFailure);
                async move { cache.get("conn|fp", "inflight", backend).await }
            });
            while cache.inflight.is_empty() {
                tokio::task::yield_now().await;
            }
            assert_eq!(cache.inflight.len(), 1);
            assert_eq!(cache.inflight_slots.available_permits(), 0);
            lookup.await.unwrap().expect_err("transient failure");
            assert!(cache.inflight.is_empty());
            assert_eq!(cache.inflight_slots.available_permits(), 1);
        }

        #[tokio::test]
        async fn cancelling_a_waiter_does_not_retain_the_inflight_entry() {
            let cache = details_cache(1, Duration::from_secs(60), Duration::from_secs(5));
            let calls = Arc::new(AtomicUsize::new(0));
            let request = tokio::spawn({
                let cache = cache.clone();
                let backend = detail_backend(calls.clone(), 40, DetailBehavior::Found);
                async move { cache.get("conn|fp", "model-a", backend).await }
            });

            while calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            request.abort();
            tokio::time::sleep(Duration::from_millis(80)).await;

            assert!(cache.inflight.is_empty());
            assert_eq!(cache.inflight_slots.available_permits(), 1);
        }

        #[tokio::test]
        async fn detail_fetch_deadline_is_shared_by_concurrent_waiters() {
            let cache = Arc::new(ModelDetailsCache::with_limits(
                8,
                8,
                8,
                Duration::from_secs(60),
                Duration::from_secs(5),
                Duration::from_millis(10),
            ));
            let calls = Arc::new(AtomicUsize::new(0));
            let futures = (0..8).map(|_| {
                let cache = cache.clone();
                let backend = detail_backend(calls.clone(), 100, DetailBehavior::Found);
                async move { cache.get("conn|fp", "model-a", backend).await }
            });

            let results = join_all(futures).await;

            assert!(results
                .iter()
                .all(|result| matches!(result, Err(BackendError::ApiError { status: 504, .. }))));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(cache.inflight.is_empty());
        }
    }
}
