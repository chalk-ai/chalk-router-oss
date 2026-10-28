//! Model routes: `chalk/*` public model aliases mapping a public model id to a
//! provider connection + upstream model id.
//!
//! `connection_id` is a **soft reference** — there is deliberately no database
//! foreign key to the connection table (a route may point at a virtual default,
//! a config-declared connection, a persisted connection, or a currently-missing
//! one). Dangling references are surfaced as diagnostics by the service layer,
//! not enforced here. The store enforces a complete default destination and
//! uniqueness of `public_model_id` within an environment.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::{DashMap, DashSet};
use std::sync::Arc;
use uuid::Uuid;

use crate::service::pagination::StablePageCursor;

#[derive(Debug, Clone)]
pub struct ModelRoute {
    pub id: String,
    pub public_model_id: String,
    pub connection_id: String,
    pub upstream_model_id: String,
    pub published: bool,
    pub routing: Option<crate::route_grid::RouteGrid>,
    /// Set when the stored `routing` could not be read or validated by this
    /// server (for example a field written by a newer replica). Such a route
    /// has `routing: None`, does not resolve, and can only be repaired by
    /// replacing its routing.
    pub routing_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ModelRoute {
    /// The executable default is owned by the custom grid when configured.
    /// Top-level fields describe only the simple route representation.
    pub fn default_connection_id(&self) -> &str {
        self.routing
            .as_ref()
            .map_or(self.connection_id.as_str(), |grid| {
                grid.default_target.connection_id.as_str()
            })
    }

    pub fn default_upstream_model_id(&self) -> &str {
        self.routing
            .as_ref()
            .map_or(self.upstream_model_id.as_str(), |grid| {
                grid.default_target.upstream_model_id.as_str()
            })
    }
}

/// Validate the effective destination after merging a partial update. Stores
/// call this under their mutation lock so concurrent updates cannot remove the
/// custom default and the simple target independently, leaving neither usable.
pub(crate) fn validate_destination(
    connection_id: &str,
    upstream_model_id: &str,
    routing: Option<&crate::route_grid::RouteGrid>,
) -> Result<(), RouteStoreError> {
    if let Some(grid) = routing {
        grid.validate().map_err(RouteStoreError::InvalidTarget)?;
    } else if connection_id.trim().is_empty() || upstream_model_id.trim().is_empty() {
        return Err(RouteStoreError::InvalidTarget(
            "simple route connection_id and upstream_model_id must not be empty".into(),
        ));
    }
    Ok(())
}

/// Fields required to create a route. `id`/`created_at`/`updated_at` are assigned
/// by the store.
pub struct RouteInput {
    pub public_model_id: String,
    pub connection_id: String,
    pub upstream_model_id: String,
    pub published: bool,
    pub routing: Option<crate::route_grid::RouteGrid>,
}

/// Partial update; `None` fields are left unchanged. `public_model_id` is not
/// updatable (it is the stable public alias).
#[derive(Default)]
pub struct RouteUpdate {
    pub connection_id: Option<String>,
    pub upstream_model_id: Option<String>,
    pub published: Option<bool>,
    /// None leaves routing unchanged; Some(None) clears it; Some(Some(grid)) replaces it.
    pub routing: Option<Option<crate::route_grid::RouteGrid>>,
}

/// Optional filters applied before model-route cursor pagination.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteListFilter {
    /// Empty means routes for every connection.
    pub connection_ids: Vec<String>,
    /// `None` means both published and unpublished routes.
    pub published: Option<bool>,
}

impl RouteListFilter {
    fn matches(&self, route: &ModelRoute) -> bool {
        (self.connection_ids.is_empty()
            || self
                .connection_ids
                .iter()
                .any(|id| id == route.default_connection_id()))
            && self
                .published
                .is_none_or(|published| route.published == published)
    }
}

/// Persistence failure from a [`ModelRouteStore`].
#[derive(Debug)]
pub enum RouteStoreError {
    /// The resulting route has no valid complete default destination.
    InvalidTarget(String),
    /// Unexpected database error.
    Db(String),
    /// The targeted route does not exist.
    NotFound,
    /// A uniqueness invariant was violated (duplicate `public_model_id`).
    Conflict(String),
}

impl std::fmt::Display for RouteStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteStoreError::InvalidTarget(msg) | RouteStoreError::Db(msg) => write!(f, "{msg}"),
            RouteStoreError::NotFound => write!(f, "model route not found"),
            RouteStoreError::Conflict(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for RouteStoreError {}

impl From<sqlx::Error> for RouteStoreError {
    fn from(e: sqlx::Error) -> Self {
        RouteStoreError::Db(e.to_string())
    }
}

/// Map a sqlx error, translating a unique-constraint violation into a
/// [`RouteStoreError::Conflict`]; everything else becomes [`RouteStoreError::Db`].
fn map_sqlx_err(e: sqlx::Error) -> RouteStoreError {
    if matches!(
        crate::db_error::classify(&e),
        Some(crate::db_error::DbConstraint::Unique)
    ) {
        return RouteStoreError::Conflict(
            "a route with this public_model_id already exists".to_string(),
        );
    }
    RouteStoreError::Db(e.to_string())
}

// Keep the existing JSONB object schema: only an empty object represents a
// simple route on disk. Every nonempty object must deserialize as a complete grid.
fn routing_from_json(
    value: serde_json::Value,
) -> Result<Option<crate::route_grid::RouteGrid>, String> {
    if value.as_object().is_some_and(|object| object.is_empty()) {
        return Ok(None);
    }
    let grid: crate::route_grid::RouteGrid = serde_json::from_value(value)
        .map_err(|error| format!("invalid stored routing: {error}"))?;
    grid.validate()
        .map_err(|error| format!("invalid stored routing: {error}"))?;
    Ok(Some(grid))
}

fn routing_to_json(
    routing: Option<&crate::route_grid::RouteGrid>,
) -> Result<serde_json::Value, RouteStoreError> {
    match routing {
        Some(grid) => {
            serde_json::to_value(grid).map_err(|error| RouteStoreError::Db(error.to_string()))
        }
        None => Ok(serde_json::json!({})),
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait ModelRouteStore: Send + Sync {
    async fn create(&self, input: RouteInput) -> Result<ModelRoute, RouteStoreError>;
    async fn update(&self, id: &str, upd: RouteUpdate) -> Result<ModelRoute, RouteStoreError>;
    /// Delete a route, returning the deleted row's `public_model_id` in the same
    /// round trip (`None` if `id` did not match any row) so callers that need it
    /// for post-delete diagnostics (e.g. fallback-policy dangling-reference
    /// warnings) don't have to `list()` first to recover it.
    async fn delete(&self, id: &str) -> Result<Option<String>, RouteStoreError>;
    async fn list(&self) -> Result<Vec<ModelRoute>, RouteStoreError>;
    /// Cursor page ordered by newest creation time. The default is suitable for
    /// lightweight stores; Postgres overrides it with a bounded query.
    async fn list_page(
        &self,
        cursor: Option<&StablePageCursor>,
        limit: i64,
        filter: &RouteListFilter,
    ) -> Result<Vec<ModelRoute>, RouteStoreError> {
        let mut routes = self.list().await?;
        routes.retain(|route| {
            filter.matches(route)
                && match cursor {
                    None => true,
                    Some(cursor) => {
                        (route.created_at, route.id.as_str())
                            < (cursor.created_at, cursor.id.as_str())
                    }
                }
        });
        routes.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        routes.truncate(limit.max(0) as usize);
        Ok(routes)
    }
    async fn get_by_public_id(
        &self,
        public_model_id: &str,
    ) -> Result<Option<ModelRoute>, RouteStoreError>;
    /// Routes whose effective default references `connection_id`. Custom grids
    /// use their own default target; simple routes use the top-level connection.
    /// Used by connection deletion guards and dangling-reference diagnostics.
    async fn list_by_connection(
        &self,
        connection_id: &str,
    ) -> Result<Vec<ModelRoute>, RouteStoreError>;
}

// ---------------------------------------------------------------------------
// PostgreSQL implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct PostgresModelRouteStore {
    pool: sqlx::PgPool,
    /// Tenancy scope (see `PostgresApiKeyStore`).
    environment_id: String,
}

impl PostgresModelRouteStore {
    /// Wrap an existing pool. Tables are provisioned by migrations before boot.
    pub fn new(pool: sqlx::PgPool, environment_id: String) -> Self {
        Self {
            pool,
            environment_id,
        }
    }

    fn row_to_route(row: &sqlx::postgres::PgRow) -> Result<ModelRoute, RouteStoreError> {
        use sqlx::Row;
        let id: String = row.get("id");
        let public_model_id: String = row.get("public_model_id");
        // One unreadable routing value must not fail the whole listing: the
        // route is surfaced with `routing_error` and fails closed on its own.
        let (routing, routing_error) = match routing_from_json(
            row.try_get::<sqlx::types::Json<serde_json::Value>, _>("routing")?
                .0,
        ) {
            Ok(routing) => (routing, None),
            Err(message) => {
                tracing::error!(
                    route_id = %id,
                    public_model_id = %public_model_id,
                    error = %message,
                    "stored model route routing is unreadable"
                );
                (None, Some(message))
            }
        };
        Ok(ModelRoute {
            id,
            public_model_id,
            connection_id: row.get("connection_id"),
            upstream_model_id: row.get("upstream_model_id"),
            published: row.get("published"),
            routing,
            routing_error,
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
    }
}

#[async_trait]
impl ModelRouteStore for PostgresModelRouteStore {
    async fn create(&self, input: RouteInput) -> Result<ModelRoute, RouteStoreError> {
        validate_destination(
            &input.connection_id,
            &input.upstream_model_id,
            input.routing.as_ref(),
        )?;
        // The app mints the id (a bare uuid string); created_at / updated_at are
        // supplied by column defaults.
        let id = Uuid::new_v4().to_string();
        let mut tx = self.pool.begin().await.map_err(map_sqlx_err)?;
        // If the referenced connection exists, serialize against its deletion.
        // A missing connection intentionally remains a valid soft reference.
        sqlx::query(
            "SELECT 1 FROM chalk_router.provider_connections \
             WHERE environment_id = $1 AND id = $2 FOR KEY SHARE",
        )
        .bind(&self.environment_id)
        .bind(
            input
                .routing
                .as_ref()
                .map_or(input.connection_id.as_str(), |grid| {
                    grid.default_target.connection_id.as_str()
                }),
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx_err)?;
        let row = sqlx::query(
            "INSERT INTO chalk_router.model_routes \
             (environment_id, id, public_model_id, connection_id, upstream_model_id, published, routing) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *",
        )
        .bind(&self.environment_id)
        .bind(&id)
        .bind(&input.public_model_id)
        .bind(&input.connection_id)
        .bind(&input.upstream_model_id)
        .bind(input.published)
        .bind(sqlx::types::Json(routing_to_json(input.routing.as_ref())?))
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx_err)?;
        let route = Self::row_to_route(&row)?;
        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(route)
    }

    async fn update(&self, id: &str, upd: RouteUpdate) -> Result<ModelRoute, RouteStoreError> {
        // Always bump updated_at, then apply whichever optional fields are set.
        let mut sets: Vec<String> = vec!["updated_at = now()".to_string()];
        let mut param_idx = 3u32; // $1 = id, $2 = environment_id
        if upd.connection_id.is_some() {
            sets.push(format!("connection_id = ${param_idx}"));
            param_idx += 1;
        }
        if upd.upstream_model_id.is_some() {
            sets.push(format!("upstream_model_id = ${param_idx}"));
            param_idx += 1;
        }
        if upd.published.is_some() {
            sets.push(format!("published = ${param_idx}"));
            param_idx += 1;
        }
        if upd.routing.is_some() {
            sets.push(format!("routing = ${param_idx}"));
        }

        let sql = format!(
            "UPDATE chalk_router.model_routes SET {} WHERE id = $1 AND environment_id = $2 RETURNING *",
            sets.join(", ")
        );
        // `sets` contains only the fixed column assignments selected above.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .bind(&self.environment_id);
        let mut tx = self.pool.begin().await.map_err(map_sqlx_err)?;
        let current = sqlx::query(
            "SELECT * FROM chalk_router.model_routes WHERE environment_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(&self.environment_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx_err)?
        .ok_or(RouteStoreError::NotFound)?;
        let current = Self::row_to_route(&current)?;
        let routing = upd.routing.as_ref().unwrap_or(&current.routing);
        let connection_id = upd
            .connection_id
            .as_deref()
            .unwrap_or(&current.connection_id);
        let upstream = upd
            .upstream_model_id
            .as_deref()
            .unwrap_or(&current.upstream_model_id);
        // An unreadable stored routing that this update leaves untouched cannot
        // be validated; the column is not rewritten, so other updates are safe.
        let stored_unreadable = upd.routing.is_none() && current.routing_error.is_some();
        if !stored_unreadable {
            validate_destination(connection_id, upstream, routing.as_ref())?;
        }
        let default_connection = routing.as_ref().map_or(connection_id, |grid| {
            grid.default_target.connection_id.as_str()
        });
        // Coordinate with deletion of the actual default connection, including
        // a routing-only update or a return to the stored simple target.
        sqlx::query(
            "SELECT 1 FROM chalk_router.provider_connections \
             WHERE environment_id = $1 AND id = $2 FOR KEY SHARE",
        )
        .bind(&self.environment_id)
        .bind(default_connection)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx_err)?;
        if let Some(cid) = upd.connection_id {
            query = query.bind(cid);
        }
        if let Some(ref upstream) = upd.upstream_model_id {
            query = query.bind(upstream);
        }
        if let Some(published) = upd.published {
            query = query.bind(published);
        }
        if let Some(routing) = upd.routing {
            query = query.bind(sqlx::types::Json(routing_to_json(routing.as_ref())?));
        }

        let row = query.fetch_optional(&mut *tx).await.map_err(map_sqlx_err)?;
        let row = row.ok_or(RouteStoreError::NotFound)?;
        let route = Self::row_to_route(&row)?;
        tx.commit().await.map_err(map_sqlx_err)?;
        Ok(route)
    }

    async fn delete(&self, id: &str) -> Result<Option<String>, RouteStoreError> {
        use sqlx::Row;
        let row = sqlx::query(
            "DELETE FROM chalk_router.model_routes WHERE id = $1 AND environment_id = $2 \
             RETURNING public_model_id",
        )
        .bind(id)
        .bind(&self.environment_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get("public_model_id")))
    }

    async fn list(&self) -> Result<Vec<ModelRoute>, RouteStoreError> {
        let rows = sqlx::query(
            "SELECT * FROM chalk_router.model_routes WHERE environment_id = $1 ORDER BY created_at ASC",
        )
        .bind(&self.environment_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_route).collect()
    }

    async fn list_page(
        &self,
        cursor: Option<&StablePageCursor>,
        limit: i64,
        filter: &RouteListFilter,
    ) -> Result<Vec<ModelRoute>, RouteStoreError> {
        let rows = sqlx::query(
            "SELECT * FROM chalk_router.model_routes \
             WHERE environment_id = $1 \
               AND ($2::timestamptz IS NULL OR (created_at, id) < ($2, $3)) \
               AND (cardinality($4::text[]) = 0 OR COALESCE(routing->'default_target'->>'connection_id', connection_id) = ANY($4)) \
               AND ($5::bool IS NULL OR published = $5) \
             ORDER BY created_at DESC, id DESC LIMIT $6",
        )
        .bind(&self.environment_id)
        .bind(cursor.map(|cursor| cursor.created_at))
        .bind(cursor.map(|cursor| cursor.id.as_str()))
        .bind(&filter.connection_ids)
        .bind(filter.published)
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_route).collect()
    }

    async fn get_by_public_id(
        &self,
        public_model_id: &str,
    ) -> Result<Option<ModelRoute>, RouteStoreError> {
        let row = sqlx::query(
            "SELECT * FROM chalk_router.model_routes WHERE environment_id = $1 AND public_model_id = $2",
        )
        .bind(&self.environment_id)
        .bind(public_model_id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(Self::row_to_route).transpose()
    }

    async fn list_by_connection(
        &self,
        connection_id: &str,
    ) -> Result<Vec<ModelRoute>, RouteStoreError> {
        let rows = sqlx::query(
            "SELECT * FROM chalk_router.model_routes \
             WHERE environment_id = $1 AND COALESCE(routing->'default_target'->>'connection_id', connection_id) = $2 ORDER BY created_at ASC",
        )
        .bind(&self.environment_id)
        .bind(connection_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_route).collect()
    }
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct InMemoryModelRouteStore {
    routes: Arc<DashMap<String, ModelRoute>>,
    // Mirrors the `public_model_id` of every entry in `routes`, so `create` can
    // reject a duplicate in O(1) instead of scanning every existing route.
    // Kept in sync at the same two call sites that mutate `routes` by key:
    // insert in `create`, remove in `delete` (`update` cannot change
    // `public_model_id`, so it never touches this set).
    public_model_ids: Arc<DashSet<String>>,
}

impl InMemoryModelRouteStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ModelRouteStore for InMemoryModelRouteStore {
    async fn create(&self, input: RouteInput) -> Result<ModelRoute, RouteStoreError> {
        validate_destination(
            &input.connection_id,
            &input.upstream_model_id,
            input.routing.as_ref(),
        )?;
        // Match the connection-delete guard: these separate in-memory maps do
        // not have PostgreSQL row locks to coordinate soft references.
        let _guard = crate::service::CONNECTION_ROUTE_MUTATION_LOCK.lock().await;
        // Enforce the unique public_model_id constraint the DB gives us for
        // free, in O(1) via the companion set instead of scanning `routes`.
        if self.public_model_ids.contains(&input.public_model_id) {
            return Err(RouteStoreError::Conflict(
                "a route with this public_model_id already exists".to_string(),
            ));
        }
        let now = chrono::Utc::now();
        let route = ModelRoute {
            id: Uuid::new_v4().to_string(),
            public_model_id: input.public_model_id,
            connection_id: input.connection_id,
            upstream_model_id: input.upstream_model_id,
            published: input.published,
            routing: input.routing,
            routing_error: None,
            created_at: now,
            updated_at: now,
        };
        self.public_model_ids.insert(route.public_model_id.clone());
        self.routes.insert(route.id.clone(), route.clone());
        Ok(route)
    }

    async fn update(&self, id: &str, upd: RouteUpdate) -> Result<ModelRoute, RouteStoreError> {
        // Repointing a route must also serialize with connection deletion.
        let _guard = crate::service::CONNECTION_ROUTE_MUTATION_LOCK.lock().await;
        let mut entry = self.routes.get_mut(id).ok_or(RouteStoreError::NotFound)?;
        validate_destination(
            upd.connection_id.as_deref().unwrap_or(&entry.connection_id),
            upd.upstream_model_id
                .as_deref()
                .unwrap_or(&entry.upstream_model_id),
            upd.routing.as_ref().unwrap_or(&entry.routing).as_ref(),
        )?;
        if let Some(cid) = upd.connection_id {
            entry.connection_id = cid;
        }
        if let Some(upstream) = upd.upstream_model_id {
            entry.upstream_model_id = upstream;
        }
        if let Some(published) = upd.published {
            entry.published = published;
        }
        if let Some(routing) = upd.routing {
            entry.routing = routing;
        }
        entry.updated_at = chrono::Utc::now();
        Ok(entry.clone())
    }

    async fn delete(&self, id: &str) -> Result<Option<String>, RouteStoreError> {
        Ok(self.routes.remove(id).map(|(_, route)| {
            self.public_model_ids.remove(&route.public_model_id);
            route.public_model_id
        }))
    }

    async fn list(&self) -> Result<Vec<ModelRoute>, RouteStoreError> {
        let mut routes: Vec<_> = self.routes.iter().map(|e| e.value().clone()).collect();
        routes.sort_by_key(|r| r.created_at);
        Ok(routes)
    }

    async fn get_by_public_id(
        &self,
        public_model_id: &str,
    ) -> Result<Option<ModelRoute>, RouteStoreError> {
        Ok(self
            .routes
            .iter()
            .find(|e| e.value().public_model_id == public_model_id)
            .map(|e| e.value().clone()))
    }

    async fn list_by_connection(
        &self,
        connection_id: &str,
    ) -> Result<Vec<ModelRoute>, RouteStoreError> {
        let mut routes: Vec<_> = self
            .routes
            .iter()
            .filter(|e| e.value().default_connection_id() == connection_id)
            .map(|e| e.value().clone())
            .collect();
        routes.sort_by_key(|r| r.created_at);
        Ok(routes)
    }
}
