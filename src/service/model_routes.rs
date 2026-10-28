//! Model route service.
//!
//! Owns validation of `chalk/*` aliases and computes a per-route *diagnostic*
//! describing the resolution status of the route's soft `connection_id`
//! reference (the connection may be missing, disabled, unreadable, etc.).

use std::collections::HashMap;
use std::sync::Arc;

use crate::model_routes::{
    ModelRoute, ModelRouteStore, RouteInput, RouteListFilter, RouteStoreError, RouteUpdate,
};
use crate::provider_connections::{ConnectionMetadata, ProviderConnectionStore};
use crate::service::pagination::{parse_stable_cursor, resolve_limit, StablePageCursor};
use crate::service::ServiceError;

/// Resolution status of a route's soft connection reference. Mirrors the proto
/// `RouteDiagnostic` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteDiagnostic {
    /// Connection resolves, is enabled, and has a routable endpoint.
    /// Credential readability is preserved by the metadata health cache.
    Ready,
    /// Connection is enabled but lacks a required endpoint.
    Unconfigured,
    /// Connection resolves but `routing_enabled` is false (kill switch).
    Disabled,
    /// The referenced connection's credential cannot be decrypted.
    Unreadable,
    /// The referenced `connection_id` does not resolve to any connection.
    MissingConnection,
    /// The diagnostic could not be computed (the post-commit connection lookup
    /// failed). The route state itself is fine — maps to proto UNSPECIFIED.
    Unknown,
}

/// A route paired with its computed connection diagnostic.
#[derive(Debug, Clone)]
pub struct RouteWithDiagnostic {
    pub route: ModelRoute,
    pub diagnostic: RouteDiagnostic,
}

pub struct ModelRouteService {
    routes: Arc<dyn ModelRouteStore>,
    connections: Arc<dyn ProviderConnectionStore>,
}

impl ModelRouteService {
    pub fn new(
        routes: Arc<dyn ModelRouteStore>,
        connections: Arc<dyn ProviderConnectionStore>,
    ) -> Self {
        Self {
            routes,
            connections,
        }
    }

    pub async fn list_page(
        &self,
        limit: Option<i32>,
        cursor: Option<String>,
        filter: RouteListFilter,
    ) -> Result<(Vec<RouteWithDiagnostic>, Option<String>), ServiceError> {
        let limit = resolve_limit(limit)?;
        let cursor = parse_stable_cursor(cursor.as_deref())?;
        let mut routes = self
            .routes
            .list_page(cursor.as_ref(), limit + 1, &filter)
            .await?;
        let next_cursor = if routes.len() as i64 > limit {
            routes.truncate(limit as usize);
            routes.last().map(|route| {
                StablePageCursor {
                    created_at: route.created_at,
                    id: route.id.clone(),
                }
                .encode()
            })
        } else {
            None
        };
        let connection_ids: Vec<String> = routes
            .iter()
            .map(|route| route.default_connection_id().to_owned())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let by_id = self
            .connections
            .get_metadata_many(&connection_ids)
            .await
            .map_err(|_| ServiceError::internal("failed to resolve provider connections"))?
            .into_iter()
            .map(|connection| (connection.id.clone(), connection))
            .collect::<HashMap<_, _>>();
        let routes = routes
            .into_iter()
            .map(|route| {
                let diagnostic = classify_route(&route, by_id.get(route.default_connection_id()));
                RouteWithDiagnostic { route, diagnostic }
            })
            .collect();
        Ok((routes, next_cursor))
    }

    /// Unpaged compatibility helper for internal callers. RPCs use
    /// [`Self::list_page`] so their response size remains bounded.
    pub async fn list(&self) -> Result<Vec<RouteWithDiagnostic>, ServiceError> {
        let (routes, by_id) = tokio::join!(self.routes.list(), self.connections_by_id());
        let routes = routes?;
        let by_id = by_id?;
        Ok(routes
            .into_iter()
            .map(|route| {
                let diagnostic = classify_route(&route, by_id.get(route.default_connection_id()));
                RouteWithDiagnostic { route, diagnostic }
            })
            .collect())
    }

    pub async fn create(&self, input: RouteInput) -> Result<RouteWithDiagnostic, ServiceError> {
        crate::model_routes::validate_destination(
            &input.connection_id,
            &input.upstream_model_id,
            input.routing.as_ref(),
        )?;
        validate_public_model_id(&input.public_model_id)?;
        if let Some(judge) = input.routing.as_ref().and_then(|grid| grid.judge.as_ref()) {
            self.validate_judge_backend(&judge.backend).await?;
        }
        // Fetch only the ONE connection this route references — a targeted point
        // lookup instead of listing (and decrypting) every connection in the
        // environment just to classify a single route. Done BEFORE the mutation:
        // it is the only fallible connection I/O, so doing it first means a
        // lookup failure aborts the request *without* having created the route.
        // Were it done after, create() would commit and then return an error,
        // and the client's retry would conflict on the now-existing
        // `public_model_id`. Classification afterward is pure and cannot fail.
        let default_connection = input
            .routing
            .as_ref()
            .map_or(input.connection_id.as_str(), |grid| {
                grid.default_target.connection_id.as_str()
            });
        let conn = self.get_connection(default_connection).await?;
        let route = self.routes.create(input).await?;
        let diagnostic = classify_route(&route, conn.as_ref());
        Ok(RouteWithDiagnostic { route, diagnostic })
    }

    pub async fn update(
        &self,
        id: &str,
        upd: RouteUpdate,
    ) -> Result<RouteWithDiagnostic, ServiceError> {
        if let Some(Some(ref routing)) = upd.routing {
            routing
                .validate()
                .map_err(|e| ServiceError::invalid_field("routing", e))?;
            if let Some(judge) = &routing.judge {
                self.validate_judge_backend(&judge.backend).await?;
            }
        }
        // A custom target is independent of the simple fields. The store
        // validates the merged destination atomically, including clears.
        // Resolve an explicitly supplied default before mutation; otherwise
        // compute the diagnostic from the committed route below.
        let explicit_default = upd
            .routing
            .as_ref()
            .and_then(|routing| routing.as_ref())
            .map(|grid| grid.default_target.connection_id.clone());
        match explicit_default {
            Some(ref cid) => {
                let conn = self.get_connection(cid).await?;
                let route = self.routes.update(id, upd).await?;
                let diagnostic = classify_route(&route, conn.as_ref());
                Ok(RouteWithDiagnostic { route, diagnostic })
            }
            None => {
                let route = self.routes.update(id, upd).await?;
                // The UPDATE is committed at this point, so a failed diagnostic
                // lookup must not turn the successful mutation into an error
                // response — the client would retry an already-applied change.
                // Degrade to `Unknown` instead; the diagnostic is informational.
                let diagnostic = match self.get_connection(route.default_connection_id()).await {
                    Ok(conn) => classify_route(&route, conn.as_ref()),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            route_id = %route.id,
                            "route updated, but the connection lookup for its diagnostic failed"
                        );
                        RouteDiagnostic::Unknown
                    }
                };
                Ok(RouteWithDiagnostic { route, diagnostic })
            }
        }
    }

    /// Check configured capabilities without calling the provider or decrypting credentials.
    /// Also validates disabled judges, so turning them on cannot reveal a known mismatch.
    pub async fn validate_judge_backend(
        &self,
        backend: &crate::route_grid::RouteJudgeBackend,
    ) -> Result<(), ServiceError> {
        let Some(target) = backend.target() else {
            return Ok(());
        };
        let invalid =
            |message| ServiceError::invalid_field("routing.judge.backend.target", message);
        target.validate().map_err(invalid)?;
        let connection = self
            .get_connection(&target.connection_id)
            .await?
            .ok_or_else(|| invalid("Judge connection does not exist".to_string()))?;
        // Use the same adapter, model registry, and declared capability layers as routing.
        // No live probe is needed; credentials and temporary availability are runtime concerns.
        let resolved = crate::backend::router::resolve_connection_capabilities(
            &crate::capabilities::CapabilityCatalog::new(),
            &connection.id,
            &connection.provider_kind,
            connection.base_url.as_deref(),
            &target.upstream_model_id,
            None,
            connection.declared_capabilities.as_ref(),
        )
        .map_err(|error| invalid(format!("Cannot validate judge capabilities: {error}")))?;
        let decisions = matches!(backend, crate::route_grid::RouteJudgeBackend::Decisions(_));
        let requirements = crate::capability_requirements::judge_requirements(decisions);
        let protocol = if decisions {
            crate::capabilities::Protocol::RouterDecisions
        } else {
            crate::capabilities::Protocol::OpenaiChatCompletions
        };
        // Unknown model facts may be attempted, but the connection's adapter
        // must actually expose the selected operation. A declaration cannot
        // add a Decisions transport to a completion-only adapter.
        let adapter = crate::provider_registry::capabilities(
            &connection.provider_kind,
            &target.upstream_model_id,
        );
        let supports_operation = adapter
            .operation(protocol, crate::capabilities::ProtocolOperation::Create)
            .is_some_and(|operation| {
                operation.support != crate::capabilities::Support::Unsupported
            });
        if !supports_operation || !resolved.profile.satisfies(&requirements) {
            let operation = if decisions {
                "Decisions"
            } else {
                "Chat Completions"
            };
            return Err(invalid(format!(
                "Judge model '{}' on connection '{}' does not support {operation}. Choose a compatible judge connection and model or change the judge backend.",
                target.upstream_model_id, connection.name,
            )));
        }
        Ok(())
    }

    /// Delete a route, returning the deleted row's `public_model_id` (`None` if
    /// `id` did not match any row) so the caller can compute dangling-reference
    /// warnings without a separate lookup.
    pub async fn delete(&self, id: &str) -> Result<Option<String>, ServiceError> {
        Ok(self.routes.delete(id).await?)
    }

    /// List every stored connection once, indexed by immutable id, for
    /// classifying routes' soft connection references without a per-route query.
    /// Used only by `list()`, which classifies a whole batch of routes that
    /// share connections; `create`/`update` classify a single route and use the
    /// targeted [`Self::get_connection`] instead. A store error is an internal
    /// failure rather than a route-level diagnostic.
    async fn connections_by_id(&self) -> Result<HashMap<String, ConnectionMetadata>, ServiceError> {
        let conns = self
            .connections
            .list_metadata()
            .await
            .map_err(|_| ServiceError::internal("failed to resolve provider connections"))?;
        Ok(conns.into_iter().map(|c| (c.id.clone(), c)).collect())
    }

    /// Point lookup of a single connection by id, for classifying one route's
    /// soft connection reference without reading every
    /// connection in the environment. A store error is an internal failure
    /// rather than a route-level diagnostic.
    async fn get_connection(
        &self,
        connection_id: &str,
    ) -> Result<Option<ConnectionMetadata>, ServiceError> {
        self.connections
            .get_metadata(connection_id)
            .await
            .map_err(|_| ServiceError::internal("failed to resolve provider connection"))
    }
}

/// A route whose stored routing is unreadable has no trustworthy default
/// connection, so it reports `Unknown` (proto UNSPECIFIED) instead of a
/// connection-based diagnostic.
fn classify_route(
    route: &crate::model_routes::ModelRoute,
    connection: Option<&ConnectionMetadata>,
) -> RouteDiagnostic {
    if route.routing_error.is_some() {
        return RouteDiagnostic::Unknown;
    }
    classify(connection)
}

/// Classify a route's soft connection reference into a [`RouteDiagnostic`] from
/// an already-resolved connection (or its absence). Pure — no I/O — so mutation
/// paths can classify after committing without any risk of a post-commit error.
fn classify(connection: Option<&ConnectionMetadata>) -> RouteDiagnostic {
    match connection {
        None => RouteDiagnostic::MissingConnection,
        Some(conn) => {
            if conn.credential_state.unreadable() {
                RouteDiagnostic::Unreadable
            } else if !conn.routing_enabled {
                RouteDiagnostic::Disabled
            } else if crate::provider_registry::requires_explicit_base_url(&conn.provider_kind)
                && conn
                    .base_url
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
            {
                // The credential decrypted (Readable) and the kill switch is on,
                // but the connection can't actually dispatch: its kind (`vllm` /
                // `openai-compatible`) has no default endpoint and no `base_url` is
                // set. `vllm` reaches this via create/update (base_url isn't
                // required for built-ins at validation time) yet `build_backend`
                // can't construct it — so the route is genuinely unconfigured.
                RouteDiagnostic::Unconfigured
            } else {
                // Readable + enabled + dispatchable.
                RouteDiagnostic::Ready
            }
        }
    }
}

fn validate_public_model_id(id: &str) -> Result<(), ServiceError> {
    let suffix = id
        .strip_prefix(crate::provider_registry::ROUTE_RESERVED_PREFIX)
        .ok_or_else(|| {
            ServiceError::invalid_field("public_model_id", "must start with 'chalk/'")
        })?;
    if suffix.trim().is_empty() {
        return Err(ServiceError::invalid_field(
            "public_model_id",
            "must not be empty after the 'chalk/' prefix",
        ));
    }
    Ok(())
}

impl From<RouteStoreError> for ServiceError {
    fn from(e: RouteStoreError) -> Self {
        match e {
            RouteStoreError::InvalidTarget(msg) => ServiceError::invalid_field("routing", msg),
            RouteStoreError::NotFound => ServiceError::not_found("model route"),
            // The only uniqueness constraint is on public_model_id.
            RouteStoreError::Conflict(msg) => ServiceError::invalid_field("public_model_id", msg),
            RouteStoreError::Db(msg) => {
                tracing::error!(error = %msg, "model route store persistence error");
                ServiceError::internal("database error")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_routes::{InMemoryModelRouteStore, RouteInput};
    use crate::provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, StoredConnection,
    };

    fn stores() -> (Arc<dyn ProviderConnectionStore>, Arc<dyn ModelRouteStore>) {
        (
            Arc::new(InMemoryProviderConnectionStore::new()),
            Arc::new(InMemoryModelRouteStore::new()),
        )
    }

    async fn add_conn(
        conns: &Arc<dyn ProviderConnectionStore>,
        name: &str,
        kind: &str,
        base_url: Option<&str>,
        routing_enabled: bool,
    ) -> String {
        conns
            .create(ConnectionInput {
                name: name.to_string(),
                provider_kind: kind.to_string(),
                base_url: base_url.map(str::to_string),
                prefix: Some(format!("{name}/")),
                exposure: ExposurePolicy::Dynamic,
                routing_enabled,
                declared_capabilities: None,
                api_key: "unused".to_string(),
            })
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn judge_backend_checks_actual_capabilities() {
        use crate::route_grid::{
            CompletionJudgeBackend, DecisionsJudgeBackend, RouteJudgeBackend, RouteTarget,
        };
        let (conns, routes) = stores();
        let openai = add_conn(&conns, "OpenAI", "openai", None, true).await;
        let typesafe = add_conn(&conns, "TypeSafe", "typesafe", None, true).await;
        let compatible = add_conn(
            &conns,
            "Compatible",
            "openai-compatible",
            Some("http://127.0.0.1:9"),
            true,
        )
        .await;
        let service = ModelRouteService::new(routes, conns);
        for (connection_id, model, decisions, valid) in [
            (openai.clone(), "gpt-4o", true, false),
            (openai.clone(), "gpt-4o", false, true),
            (openai, "text-embedding-3-small", false, false),
            (typesafe.clone(), "systemone", true, true),
            // TypeSafe's completion adapter is a valid completion judge too.
            (typesafe, "systemone", false, true),
            (compatible, "custom-model", false, true),
            ("missing".into(), "gpt-4o", true, false),
        ] {
            let target = RouteTarget {
                connection_id,
                upstream_model_id: model.into(),
            };
            let backend = if decisions {
                RouteJudgeBackend::Decisions(DecisionsJudgeBackend {
                    target,
                    timeout_ms: 1000,
                })
            } else {
                RouteJudgeBackend::Completion(CompletionJudgeBackend {
                    target,
                    timeout_ms: 1000,
                })
            };
            let result = service.validate_judge_backend(&backend).await;
            assert_eq!(result.is_ok(), valid, "{backend:?}: {result:?}");
        }
    }

    #[tokio::test]
    async fn incompatible_judge_cannot_be_created_or_updated_even_when_off() {
        use crate::route_grid::{
            Classification, DecisionsJudgeBackend, JudgeMode, JudgeOutcomeConfig, RouteGrid,
            RouteJudge, RouteJudgeBackend, RouteTarget,
        };
        let (conns, routes) = stores();
        let connection_id = add_conn(&conns, "OpenAI", "openai", None, true).await;
        let service = ModelRouteService::new(routes.clone(), conns);
        let target = RouteTarget {
            connection_id: connection_id.clone(),
            upstream_model_id: "gpt-4o".into(),
        };
        let invalid = RouteGrid {
            default_target: target.clone(),
            fallbacks: Default::default(),
            judge: Some(RouteJudge {
                mode: JudgeMode::Off,
                classification: Classification::Task,
                backend: RouteJudgeBackend::Decisions(DecisionsJudgeBackend {
                    target: target.clone(),
                    timeout_ms: 1000,
                }),
                outcomes: [(
                    "other".into(),
                    JudgeOutcomeConfig {
                        model: target.concrete_model(),
                        description: String::new(),
                        fallbacks: None,
                    },
                )]
                .into(),
            }),
        };
        let input = || RouteInput {
            public_model_id: "chalk/judge-validation".into(),
            connection_id: connection_id.clone(),
            upstream_model_id: "gpt-4o".into(),
            published: false,
            routing: Some(invalid.clone()),
        };
        let error = service.create(input()).await.unwrap_err();
        assert!(error.to_string().contains("does not support Decisions"));
        assert!(routes.list().await.unwrap().is_empty());
        let created = service
            .create(RouteInput {
                routing: None,
                ..input()
            })
            .await
            .unwrap();
        let error = service
            .update(
                &created.route.id,
                RouteUpdate {
                    routing: Some(Some(invalid)),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("does not support Decisions"));
        assert!(routes.list().await.unwrap()[0].routing.is_none());
    }

    async fn add_route(routes: &Arc<dyn ModelRouteStore>, public: &str, connection_id: &str) {
        routes
            .create(RouteInput {
                routing: Default::default(),
                public_model_id: public.to_string(),
                connection_id: connection_id.to_string(),
                upstream_model_id: "m".to_string(),
                published: true,
            })
            .await
            .unwrap();
    }

    async fn diagnostic_of(
        public: &str,
        conns: Arc<dyn ProviderConnectionStore>,
        routes: Arc<dyn ModelRouteStore>,
    ) -> RouteDiagnostic {
        let svc = ModelRouteService::new(routes, conns);
        svc.list()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.route.public_model_id == public)
            .expect("route present")
            .diagnostic
    }

    #[tokio::test]
    async fn vllm_without_base_url_is_unconfigured() {
        // vLLM has no default endpoint; a stored row with no base_url resolves
        // (Readable, enabled) but can't build a backend -> unconfigured.
        let (conns, routes) = stores();
        let id = add_conn(&conns, "vllm", "vllm", None, true).await;
        add_route(&routes, "chalk/local", &id).await;
        assert_eq!(
            diagnostic_of("chalk/local", conns, routes).await,
            RouteDiagnostic::Unconfigured
        );
    }

    #[tokio::test]
    async fn vllm_with_base_url_is_ready() {
        let (conns, routes) = stores();
        let id = add_conn(
            &conns,
            "vllm",
            "vllm",
            Some("http://localhost:8000/v1"),
            true,
        )
        .await;
        add_route(&routes, "chalk/local", &id).await;
        assert_eq!(
            diagnostic_of("chalk/local", conns, routes).await,
            RouteDiagnostic::Ready
        );
    }

    #[tokio::test]
    async fn builtin_with_default_endpoint_is_ready_without_base_url() {
        // openai owns its default endpoint internally -> no base_url required.
        let (conns, routes) = stores();
        let id = add_conn(&conns, "openai", "openai", None, true).await;
        add_route(&routes, "chalk/gpt", &id).await;
        assert_eq!(
            diagnostic_of("chalk/gpt", conns, routes).await,
            RouteDiagnostic::Ready
        );
    }

    #[tokio::test]
    async fn disabled_connection_reports_disabled_over_unconfigured() {
        // The kill switch takes precedence over the endpoint check.
        let (conns, routes) = stores();
        let id = add_conn(&conns, "vllm", "vllm", None, false).await;
        add_route(&routes, "chalk/local", &id).await;
        assert_eq!(
            diagnostic_of("chalk/local", conns, routes).await,
            RouteDiagnostic::Disabled
        );
    }

    #[tokio::test]
    async fn dangling_connection_reference_is_missing() {
        let (conns, routes) = stores();
        add_route(&routes, "chalk/orphan", "does-not-exist").await;
        assert_eq!(
            diagnostic_of("chalk/orphan", conns, routes).await,
            RouteDiagnostic::MissingConnection
        );
    }

    #[tokio::test]
    async fn create_returns_computed_diagnostic() {
        // create() must report the same diagnostic a later list() would, not a
        // placeholder — here a dangling connection reference is MissingConnection.
        let (conns, routes) = stores();
        let svc = ModelRouteService::new(routes, conns);
        let created = svc
            .create(RouteInput {
                routing: Default::default(),
                public_model_id: "chalk/orphan".to_string(),
                connection_id: "does-not-exist".to_string(),
                upstream_model_id: "m".to_string(),
                published: true,
            })
            .await
            .unwrap();
        assert_eq!(created.diagnostic, RouteDiagnostic::MissingConnection);
    }

    #[tokio::test]
    async fn update_recomputes_diagnostic() {
        // A route first points at a disabled connection (Disabled), then is
        // repointed at an enabled one — update() must report the fresh Ready.
        let (conns, routes) = stores();
        let disabled = add_conn(&conns, "off", "anthropic", None, false).await;
        let enabled = add_conn(&conns, "on", "openai", None, true).await;
        let svc = ModelRouteService::new(routes, conns);
        let created = svc
            .create(RouteInput {
                routing: Default::default(),
                public_model_id: "chalk/gpt".to_string(),
                connection_id: disabled,
                upstream_model_id: "m".to_string(),
                published: true,
            })
            .await
            .unwrap();
        assert_eq!(created.diagnostic, RouteDiagnostic::Disabled);

        let updated = svc
            .update(
                &created.route.id,
                RouteUpdate {
                    routing: Default::default(),
                    connection_id: Some(enabled),
                    upstream_model_id: None,
                    published: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.diagnostic, RouteDiagnostic::Ready);
    }

    /// A connection store whose `get()` always fails — used to prove the
    /// diagnostic load happens *before* the route mutation.
    struct FailingConnections;

    #[async_trait::async_trait]
    impl ProviderConnectionStore for FailingConnections {
        async fn create(
            &self,
            _: crate::provider_connections::ConnectionInput,
        ) -> Result<
            crate::provider_connections::ConnectionMetadata,
            crate::provider_connections::StoreError,
        > {
            unreachable!("not called by the route service")
        }
        async fn update(
            &self,
            _: &str,
            _: crate::provider_connections::ConnectionUpdate,
        ) -> Result<
            crate::provider_connections::ConnectionMetadata,
            crate::provider_connections::StoreError,
        > {
            unreachable!("not called by the route service")
        }
        async fn delete(&self, _: &str) -> Result<bool, crate::provider_connections::StoreError> {
            unreachable!("not called by the route service")
        }
        async fn get(
            &self,
            _: &str,
        ) -> Result<Option<StoredConnection>, crate::provider_connections::StoreError> {
            Err(crate::provider_connections::StoreError::Db(
                "boom".to_string(),
            ))
        }
        async fn list(
            &self,
        ) -> Result<Vec<StoredConnection>, crate::provider_connections::StoreError> {
            unreachable!("route service uses get(), not list(), for create/update")
        }
    }

    #[tokio::test]
    async fn create_aborts_before_persisting_when_connection_load_fails() {
        // Finding 2: the fallible connection load runs BEFORE the route mutation,
        // so a failure aborts the request without committing a route — otherwise a
        // client retry would conflict on the now-existing public_model_id.
        let routes: Arc<dyn ModelRouteStore> = Arc::new(InMemoryModelRouteStore::new());
        let conns: Arc<dyn ProviderConnectionStore> = Arc::new(FailingConnections);
        let svc = ModelRouteService::new(routes.clone(), conns);

        let err = svc
            .create(RouteInput {
                routing: Default::default(),
                public_model_id: "chalk/gpt".to_string(),
                connection_id: "any".to_string(),
                upstream_model_id: "m".to_string(),
                published: true,
            })
            .await
            .expect_err("expected the connection load to fail the create");
        assert!(matches!(err, ServiceError::Internal { .. }), "{err:?}");

        // Nothing was persisted, so a retry (with a working store) would not
        // conflict — the store is empty.
        assert_eq!(routes.list().await.unwrap().len(), 0);
    }
}
