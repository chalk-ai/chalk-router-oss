use std::collections::HashMap;

use fieldmask::{Maskable, SelfMaskable};
use tonic::{Request, Response, Status};

use crate::service::api_keys::{CreateKeyInput, UpdateKeyInput, UsageHistogramInput};
use crate::AppState;

use super::conv;
use super::field_mask;
use super::proto;
use super::proto::api_key_service_server::ApiKeyService;

pub struct ApiKeyServer {
    pub state: AppState,
}

#[derive(Debug, Default, Maskable, SelfMaskable)]
struct KeyPatch {
    description: String,
    daily_token_budget: Option<u64>,
    usage_pool_id: Option<String>,
    cost_tags: HashMap<String, String>,
}

const KEY_UPDATE_FIELDS: &[&str] = &[
    "description",
    "daily_token_budget",
    "usage_pool_id",
    "cost_tags",
    "permission",
    "rate_limits",
    "usage_budgets",
];

/// Validate public API-key update paths before splitting the top-level
/// `permission` replacement away from the ordinary [`KeyPatch`] fields.
///
/// Permission deliberately has replacement-only semantics: the service accepts
/// one complete permission object, so supporting `permission.*` would require
/// a separate read-modify-write merge contract.
#[allow(clippy::result_large_err)] // tonic::Status is the gRPC wire error
fn validate_update_paths(resolved: &field_mask::ResolvedPaths) -> Result<(), Status> {
    let field_mask::ResolvedPaths::Paths(paths) = resolved else {
        return Ok(());
    };

    for path in paths {
        if path.starts_with("permission.") {
            return Err(Status::invalid_argument(
                "update_mask: permission only supports whole-field replacement; use 'permission'",
            ));
        }
        if path.starts_with("usage_budgets.") {
            return Err(Status::invalid_argument(
                "update_mask: usage_budgets only supports whole-field replacement; use 'usage_budgets'",
            ));
        }
        if !KEY_UPDATE_FIELDS.contains(&path.as_str()) {
            return Err(Status::invalid_argument(format!(
                "update_mask: unsupported API-key field '{path}'"
            )));
        }
    }
    Ok(())
}

/// The `KeyPatch` field-mask shape intentionally excludes fields handled by
/// dedicated service inputs rather than fieldmask message merging.
fn key_patch_paths(resolved: &field_mask::ResolvedPaths) -> field_mask::ResolvedPaths {
    match resolved {
        field_mask::ResolvedPaths::Noop => field_mask::ResolvedPaths::Noop,
        field_mask::ResolvedPaths::Full => field_mask::ResolvedPaths::Full,
        field_mask::ResolvedPaths::Paths(paths) => {
            let paths: Vec<_> = paths
                .iter()
                .filter(|path| {
                    !matches!(
                        path.as_str(),
                        "permission" | "rate_limits" | "usage_budgets"
                    )
                })
                .cloned()
                .collect();
            if paths.is_empty() {
                field_mask::ResolvedPaths::Noop
            } else {
                field_mask::ResolvedPaths::Paths(paths)
            }
        }
    }
}

#[allow(deprecated)] // Reject deprecated cost_tags while its wire field is reserved.
#[tonic::async_trait]
impl ApiKeyService for ApiKeyServer {
    async fn create_key(
        &self,
        req: Request<proto::CreateKeyRequest>,
    ) -> Result<Response<proto::CreateKeyResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let usage_budgets = if body.usage_budgets.is_empty() {
            None
        } else {
            Some(
                body.usage_budgets
                    .into_iter()
                    .map(conv::api_key_budget_from_proto)
                    .collect::<Result<Vec<_>, _>>()?,
            )
        };
        let input = CreateKeyInput {
            description: Some(body.description),
            permission: Some(conv::permission_from_proto(body.permission)?),
            labels: Some(body.labels),
            daily_token_budget: body.daily_token_budget,
            cost_tags: (!body.cost_tags.is_empty()).then_some(body.cost_tags),
            usage_pool_id: body.usage_pool_id,
            rate_limits: body
                .rate_limits
                .into_iter()
                .map(conv::api_key_rate_limit_from_proto)
                .collect(),
            usage_budgets,
        };
        let created = state.api_key_service().create(input).await?;
        let key = state
            .api_key_service()
            .get_with_usage(&created.api_key.key_hash)
            .await?;
        Ok(Response::new(proto::CreateKeyResponse {
            key: Some(conv::api_key_with_usage_to_proto(
                &key.api_key,
                &key.usage,
                &key.rate_limits,
                &key.usage_budgets,
            )),
            raw_key: created.raw_key,
        }))
    }

    async fn list_keys(
        &self,
        req: Request<proto::ListKeysRequest>,
    ) -> Result<Response<proto::ListKeysResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let (keys, next_cursor) = state
            .api_key_service()
            .list_page_with_usage(body.limit, body.cursor)
            .await?;
        Ok(Response::new(proto::ListKeysResponse {
            keys: keys
                .iter()
                .map(|item| {
                    conv::api_key_with_usage_to_proto(
                        &item.api_key,
                        &item.usage,
                        &item.rate_limits,
                        &item.usage_budgets,
                    )
                })
                .collect(),
            next_cursor,
        }))
    }

    async fn update_key(
        &self,
        req: Request<proto::UpdateKeyRequest>,
    ) -> Result<Response<proto::UpdateKeyResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let upd = body.update.unwrap_or_default();
        let mut populated = Vec::new();
        if upd.description.is_some() {
            populated.push("description");
        }
        if upd.daily_token_budget.is_some() {
            populated.push("daily_token_budget");
        }
        if upd.usage_pool_id.is_some() {
            populated.push("usage_pool_id");
        }
        if !upd.cost_tags.is_empty() {
            populated.push("cost_tags");
        }
        if upd.permission.is_some() {
            populated.push("permission");
        }
        if !upd.rate_limits.is_empty() {
            populated.push("rate_limits");
        }
        if !upd.usage_budgets.is_empty() {
            populated.push("usage_budgets");
        }
        let resolved = field_mask::resolve(&body.update_mask, &populated)?;
        validate_update_paths(&resolved)?;

        let input = if resolved.is_noop() {
            UpdateKeyInput::default()
        } else {
            let has_budget = upd.daily_token_budget.is_some();
            let has_pool = upd.usage_pool_id.is_some();
            let permission = if resolved.contains("permission") {
                Some(conv::permission_from_proto(upd.permission.clone())?)
            } else {
                None
            };
            let rate_limits = resolved.contains("rate_limits").then(|| {
                upd.rate_limits
                    .iter()
                    .map(|limit| crate::ratelimit::ApiKeyRateLimitInput {
                        connection_id: limit.connection_id.clone(),
                        models: limit.models.clone(),
                        requests_per_minute: limit.requests_per_minute,
                        request_burst: limit.request_burst,
                        tokens_per_minute: limit.tokens_per_minute,
                        token_burst: limit.token_burst,
                        concurrent_requests: limit.concurrent_requests,
                        enabled: limit.enabled.unwrap_or(true),
                    })
                    .collect()
            });
            let usage_budgets = if resolved.contains("usage_budgets") {
                Some(
                    upd.usage_budgets
                        .clone()
                        .into_iter()
                        .map(conv::api_key_budget_from_proto)
                        .collect::<Result<Vec<_>, _>>()?,
                )
            } else {
                None
            };
            let mut target = KeyPatch::default();
            let source = KeyPatch {
                description: upd.description.unwrap_or_default(),
                daily_token_budget: upd.daily_token_budget,
                usage_pool_id: upd.usage_pool_id,
                cost_tags: upd.cost_tags,
            };
            let patch_paths = key_patch_paths(&resolved);
            if !patch_paths.is_noop() {
                field_mask::apply(&patch_paths, &mut target, source)?;
            }
            UpdateKeyInput {
                permission,
                description: resolved
                    .contains("description")
                    .then_some(target.description),
                cost_tags: resolved.contains("cost_tags").then_some(target.cost_tags),
                daily_token_budget: field_mask::clearable_update(
                    &resolved,
                    "daily_token_budget",
                    has_budget,
                    target.daily_token_budget,
                ),
                usage_pool_id: field_mask::clearable_update(
                    &resolved,
                    "usage_pool_id",
                    has_pool,
                    target.usage_pool_id,
                ),
                rate_limits,
                usage_budgets,
            }
        };

        let key = state.api_key_service().update(&body.key, input).await?;
        let key = state
            .api_key_service()
            .get_with_usage(&key.key_hash)
            .await?;
        Ok(Response::new(proto::UpdateKeyResponse {
            key: Some(conv::api_key_with_usage_to_proto(
                &key.api_key,
                &key.usage,
                &key.rate_limits,
                &key.usage_budgets,
            )),
        }))
    }

    async fn revoke_key(
        &self,
        req: Request<proto::RevokeKeyRequest>,
    ) -> Result<Response<proto::RevokeKeyResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        if body.key.is_empty() {
            return Err(Status::invalid_argument("key must be non-empty"));
        }
        state.api_key_service().revoke(&body.key).await?;
        Ok(Response::new(proto::RevokeKeyResponse { key: body.key }))
    }

    async fn get_key_usage(
        &self,
        req: Request<proto::GetKeyUsageRequest>,
    ) -> Result<Response<proto::GetKeyUsageResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let view = state.api_key_service().get_usage(&body.key).await?;
        Ok(Response::new(conv::key_usage_to_proto(&view)))
    }

    async fn list_all_usage(
        &self,
        _req: Request<proto::ListAllUsageRequest>,
    ) -> Result<Response<proto::ListAllUsageResponse>, Status> {
        let state = self.state.for_request(&_req);
        let views = state.api_key_service().list_usage().await?;
        Ok(Response::new(proto::ListAllUsageResponse {
            keys: views.iter().map(conv::key_usage_to_proto).collect(),
        }))
    }

    async fn get_usage_histogram(
        &self,
        req: Request<proto::GetUsageHistogramRequest>,
    ) -> Result<Response<proto::GetUsageHistogramResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let input = UsageHistogramInput {
            bucket_size: if body.bucket_size.is_empty() {
                None
            } else {
                Some(body.bucket_size)
            },
            start: body.start.as_ref().and_then(conv::ts_from_proto),
            end: body.end.as_ref().and_then(conv::ts_from_proto),
            api_token: body.api_token,
        };
        let rows = state.api_key_service().usage_histogram(input).await?;
        Ok(Response::new(proto::GetUsageHistogramResponse {
            rows: rows
                .iter()
                .map(conv::usage_histogram_row_to_proto)
                .collect(),
        }))
    }
}

#[cfg(test)]
#[allow(deprecated)] // Regression tests exercise the public legacy aliases.
mod tests {
    use std::sync::Arc;

    use prost_types::FieldMask;

    use super::*;
    use crate::{
        api_keys::{ApiKeyStore, InMemoryApiKeyStore},
        backend::RouterBackend,
        config::{PromptCacheConfig, QuotaBackendKind},
        hashing::KeyHasher,
        model_routes::InMemoryModelRouteStore,
        prompt_cache::DisabledPromptCache,
        provider_connections::InMemoryProviderConnectionStore,
        quota::from_kind,
        quota_policy::QuotaPolicyResolver,
        response_locator::ResponseLocatorCodec,
        storage::SharedObjectStore,
        usage_pools::InMemoryUsagePoolStore,
        AppState, DebugLog,
    };

    fn test_server() -> ApiKeyServer {
        let connections = Arc::new(InMemoryProviderConnectionStore::new());
        let routes = Arc::new(InMemoryModelRouteStore::new());
        let backend = Arc::new(RouterBackend::new(
            reqwest::Client::new(),
            connections.clone(),
            routes.clone(),
        ));
        ApiKeyServer {
            state: AppState {
                route_judge: Default::default(),
                backend,
                http_client: reqwest::Client::new(),
                debug_log: DebugLog::new(),
                environment_id: "test".to_string(),
                response_locator: ResponseLocatorCodec::new(b"test-locator".to_vec()),
                api_key_store: Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
                    b"test-pepper".to_vec(),
                ))) as Arc<dyn ApiKeyStore>,
                usage_pool_store: Arc::new(InMemoryUsagePoolStore::new()),
                sampling_policy: Arc::new(
                    crate::sampling_policy::InMemorySamplingPolicyStore::new(
                        crate::sampling_policy::SamplingPolicy::default(),
                    ),
                ),
                sampling_gate: Arc::new(crate::telemetry::sampling::SamplingGate::new()),
                usage_tracker: None,
                background_responses: Arc::new(
                    crate::background_responses::BackgroundResponses::new(
                        None,
                        "test".to_string(),
                        Default::default(),
                    ),
                ),
                budget_store: crate::budget::from_core_store(None, "test".to_string()),
                rate_limit_store: crate::ratelimit::from_core_store(None, "test".to_string()),
                quota_resolver: QuotaPolicyResolver::in_memory("test".to_string()),
                quota_backend: from_kind(QuotaBackendKind::Memory, None),
                prompt_cache: Arc::new(DisabledPromptCache),
                prompt_cache_config: Arc::new(PromptCacheConfig::disabled()),
                object_store: None as SharedObjectStore,
                provider_connection_store: connections,
                model_route_store: routes,
                telemetry_settings: crate::default_telemetry_settings_service(),
                federated_auth: Arc::new(chalk_federated_auth::OidcRegistry::empty()),
                federated_admin_team_id: None,
                federated_jwt_audience: None,
                admin_key: None,
                dangerously_permit_unauthenticated: false,
                accept_cross_environment_tokens: false,
                connection_policy: crate::provider_connections::ConnectionPolicy::trusted(),
            },
        }
    }

    async fn create_key(server: &ApiKeyServer) -> proto::ApiKey {
        server
            .create_key(Request::new(proto::CreateKeyRequest {
                description: "original description".to_string(),
                ..Default::default()
            }))
            .await
            .expect("create key")
            .into_inner()
            .key
            .expect("created key")
    }

    async fn add_test_route(server: &ApiKeyServer, name: &str) -> crate::model_routes::ModelRoute {
        server
            .state
            .model_route_store
            .create(crate::model_routes::RouteInput {
                public_model_id: name.into(),
                connection_id: "test-connection".into(),
                upstream_model_id: "gpt-4o".into(),
                published: true,
                routing: None,
            })
            .await
            .unwrap()
    }

    fn route_permission(
        refs: Vec<proto::api_key_route_ref::Identifier>,
    ) -> proto::ApiKeyPermission {
        proto::ApiKeyPermission {
            routes: Some(proto::ApiKeyRouteScope {
                refs: refs
                    .into_iter()
                    .map(|identifier| proto::ApiKeyRouteRef {
                        identifier: Some(identifier),
                    })
                    .collect(),
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn route_names_and_ids_normalize_to_the_same_immutable_grants() {
        use proto::api_key_route_ref::Identifier::{PublicModelId, RouteId};
        let server = test_server();
        let route = add_test_route(&server, "chalk/assistant-pro").await;
        let other = add_test_route(&server, "chalk/other").await;
        let key = server
            .create_key(Request::new(proto::CreateKeyRequest {
                description: "named routes".into(),
                permission: Some(route_permission(vec![
                    PublicModelId(route.public_model_id.clone()),
                    RouteId(route.id.clone()),
                    RouteId(other.id.clone()),
                ])),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner()
            .key
            .unwrap();
        let canonical =
            route_permission(vec![RouteId(route.id.clone()), RouteId(other.id.clone())]);
        assert_eq!(key.permission, Some(canonical.clone()));
        let stored = server.state.api_key_store.get(&key.key_hash).await.unwrap();
        assert_eq!(
            stored.permission.route_ids,
            Some(vec![route.id.clone(), other.id.clone()])
        );

        server
            .state
            .model_route_store
            .delete(&route.id)
            .await
            .unwrap();
        let replacement = add_test_route(&server, &route.public_model_id).await;
        assert_ne!(replacement.id, route.id);
        assert!(!stored.permission.grants_model_route(Some(&replacement.id)));
        // A read/edit/write retains the deleted identity rather than resolving
        // the old display name to the replacement route.
        let unchanged = server
            .update_key(update_request(
                &key,
                proto::UpdateKeyOperation {
                    permission: Some(canonical.clone()),
                    ..Default::default()
                },
                &["permission"],
            ))
            .await
            .unwrap()
            .into_inner()
            .key
            .unwrap();
        assert_eq!(unchanged.permission, Some(canonical));

        // Explicitly granting the name again binds to its new identity.
        let updated = server
            .update_key(update_request(
                &key,
                proto::UpdateKeyOperation {
                    permission: Some(route_permission(vec![PublicModelId(route.public_model_id)])),
                    ..Default::default()
                },
                &["permission"],
            ))
            .await
            .unwrap()
            .into_inner()
            .key
            .unwrap();
        assert_eq!(
            updated.permission,
            Some(route_permission(vec![RouteId(replacement.id)]))
        );
    }

    #[tokio::test]
    async fn invalid_route_refs_fail_before_create_or_update() {
        use proto::api_key_route_ref::Identifier::{PublicModelId, RouteId};
        let server = test_server();
        let key = create_key(&server).await;
        let _route = add_test_route(&server, "chalk/assistant-pro").await;
        for identifier in [
            None,
            Some(RouteId("".into())),
            Some(PublicModelId("  ".into())),
            Some(PublicModelId("chalk/*".into())),
            Some(PublicModelId("chalk/missing".into())),
            // The route_id variant must not interpret a name as an id.
            Some(RouteId("chalk/assistant-pro".into())),
        ] {
            let permission = proto::ApiKeyPermission {
                routes: Some(proto::ApiKeyRouteScope {
                    refs: vec![proto::ApiKeyRouteRef { identifier }],
                }),
                ..Default::default()
            };
            let error = server
                .create_key(Request::new(proto::CreateKeyRequest {
                    description: "invalid identifier".into(),
                    permission: Some(permission.clone()),
                    ..Default::default()
                }))
                .await
                .unwrap_err();
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
            assert!(error.message().contains("permission.routes.refs[0]"));
            let error = server
                .update_key(update_request(
                    &key,
                    proto::UpdateKeyOperation {
                        permission: Some(permission),
                        ..Default::default()
                    },
                    &["permission"],
                ))
                .await
                .unwrap_err();
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
            assert!(error.message().contains("permission.routes.refs[0]"));
        }
        assert_eq!(server.state.api_key_store.list().await.len(), 1);
        assert_eq!(
            server
                .state
                .api_key_store
                .get(&key.key_hash)
                .await
                .unwrap()
                .permission
                .route_ids,
            None
        );
    }

    /// A connection-scoped permission as responses present it: restricted keys
    /// always carry `routes`, here with no route granted.
    fn scoped_permission(connection_id: &str, model: &str) -> proto::ApiKeyPermission {
        proto::ApiKeyPermission {
            connection_scopes: vec![proto::ApiKeyConnectionScope {
                connection_id: connection_id.to_string(),
                models: vec![model.to_string()],
            }],
            routes: Some(proto::ApiKeyRouteScope { refs: Vec::new() }),
            ..Default::default()
        }
    }

    fn update_request(
        key: &proto::ApiKey,
        update: proto::UpdateKeyOperation,
        paths: &[&str],
    ) -> Request<proto::UpdateKeyRequest> {
        Request::new(proto::UpdateKeyRequest {
            key: key.key_hash.clone(),
            update: Some(update),
            update_mask: Some(FieldMask {
                paths: paths.iter().map(|path| (*path).to_string()).collect(),
            }),
        })
    }

    #[tokio::test]
    async fn update_key_permission_mask_replaces_connection_scopes() {
        let server = test_server();
        let key = create_key(&server).await;
        let permission = scoped_permission("conn-openai", "openai/gpt-4o");

        let updated = server
            .update_key(update_request(
                &key,
                proto::UpdateKeyOperation {
                    permission: Some(permission.clone()),
                    ..Default::default()
                },
                &["permission"],
            ))
            .await
            .expect("whole permission replacement")
            .into_inner()
            .key
            .expect("updated key");

        assert_eq!(updated.permission, Some(permission));
    }

    #[tokio::test]
    async fn update_key_combines_description_and_permission() {
        let server = test_server();
        let key = create_key(&server).await;
        let permission = scoped_permission("conn-anthropic", "anthropic/claude");

        let updated = server
            .update_key(update_request(
                &key,
                proto::UpdateKeyOperation {
                    description: Some("updated description".to_string()),
                    permission: Some(permission.clone()),
                    ..Default::default()
                },
                &["description", "permission"],
            ))
            .await
            .expect("combined update")
            .into_inner()
            .key
            .expect("updated key");

        assert_eq!(updated.description, "updated description");
        assert_eq!(updated.permission, Some(permission));
    }

    #[tokio::test]
    async fn update_key_rejects_nested_permission_mask_paths() {
        let server = test_server();
        let key = create_key(&server).await;
        let error = server
            .update_key(update_request(
                &key,
                proto::UpdateKeyOperation {
                    permission: Some(scoped_permission("conn-openai", "openai/gpt-4o")),
                    ..Default::default()
                },
                &["permission.connection_scopes"],
            ))
            .await
            .expect_err("nested permission masks are not supported");

        assert_eq!(error.code(), tonic::Code::InvalidArgument);
        assert!(error.message().contains("whole-field replacement"));
    }

    #[tokio::test]
    async fn update_key_rejects_unknown_mask_paths() {
        let server = test_server();
        let key = create_key(&server).await;
        let error = server
            .update_key(update_request(
                &key,
                proto::UpdateKeyOperation::default(),
                &["not_a_key_field"],
            ))
            .await
            .expect_err("unknown API-key fields must not be accepted");

        assert_eq!(error.code(), tonic::Code::InvalidArgument);
        assert!(error.message().contains("unsupported API-key field"));
    }

    fn usage_budget(period: proto::UsageBudgetPeriod, token_limit: u64) -> proto::UsageBudgetSpec {
        proto::UsageBudgetSpec {
            token_limit,
            period: period as i32,
            enabled: None,
        }
    }

    #[tokio::test]
    async fn update_key_replaces_complete_direct_budget_set_and_allows_empty_removal() {
        let server = test_server();
        let key = create_key(&server).await;
        let updated = server
            .update_key(update_request(
                &key,
                proto::UpdateKeyOperation {
                    usage_budgets: vec![
                        usage_budget(proto::UsageBudgetPeriod::Daily, 10),
                        usage_budget(proto::UsageBudgetPeriod::Weekly, 50),
                    ],
                    ..Default::default()
                },
                &["usage_budgets"],
            ))
            .await
            .expect("replace direct budgets")
            .into_inner()
            .key
            .expect("updated key");
        assert_eq!(updated.usage_budgets.len(), 2);
        assert_eq!(updated.daily_token_budget, Some(10));

        let cleared = server
            .update_key(update_request(
                &updated,
                proto::UpdateKeyOperation::default(),
                &["usage_budgets"],
            ))
            .await
            .expect("explicit empty list removes direct budgets")
            .into_inner()
            .key
            .expect("cleared key");
        assert!(cleared.usage_budgets.is_empty());
        assert_eq!(cleared.daily_token_budget, None);
    }

    #[tokio::test]
    async fn update_key_rejects_nested_or_conflicting_direct_budget_inputs() {
        let server = test_server();
        let key = create_key(&server).await;
        let nested = server
            .update_key(update_request(
                &key,
                proto::UpdateKeyOperation {
                    usage_budgets: vec![usage_budget(proto::UsageBudgetPeriod::Daily, 10)],
                    ..Default::default()
                },
                &["usage_budgets.token_limit"],
            ))
            .await
            .expect_err("repeated budget elements cannot be patched");
        assert_eq!(nested.code(), tonic::Code::InvalidArgument);

        let conflict = server
            .update_key(update_request(
                &key,
                proto::UpdateKeyOperation {
                    daily_token_budget: Some(10),
                    usage_budgets: vec![usage_budget(proto::UsageBudgetPeriod::Daily, 20)],
                    ..Default::default()
                },
                &["daily_token_budget", "usage_budgets"],
            ))
            .await
            .expect_err("legacy and canonical inputs cannot be combined");
        assert_eq!(conflict.code(), tonic::Code::InvalidArgument);
    }

    fn rpm_limit(connection_id: Option<&str>) -> proto::ApiKeyRateLimit {
        proto::ApiKeyRateLimit {
            connection_id: connection_id.map(str::to_string),
            requests_per_minute: Some(10),
            request_burst: Some(10),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn update_key_rate_limit_mask_replaces_and_clears_direct_limits() {
        let server = test_server();
        let key = create_key(&server).await;

        // An absent path must preserve policies created through standalone CRUD.
        server
            .state
            .rate_limit_service()
            .create(crate::service::rate_limits::CreateRateLimitInput {
                limit_type: "requests_per_minute".to_string(),
                limit_value: 5,
                burst_value: Some(5),
                connection_id: None,
                models: vec![],
                scope: "api_key".to_string(),
                scope_id: key.key_hash.clone(),
                enabled: true,
            })
            .await
            .expect("create standalone key policy");
        server
            .update_key(update_request(
                &key,
                proto::UpdateKeyOperation {
                    description: Some("description only".to_string()),
                    ..Default::default()
                },
                &["description"],
            ))
            .await
            .expect("description update");
        assert_eq!(server.state.rate_limit_store.list().await.len(), 1);

        let mut replacement = rpm_limit(None);
        replacement.enabled = Some(false);
        let response = server
            .update_key(update_request(
                &key,
                proto::UpdateKeyOperation {
                    rate_limits: vec![replacement],
                    ..Default::default()
                },
                &["rate_limits"],
            ))
            .await
            .expect("replace direct rate limits");
        let returned = response.into_inner().key.expect("updated key");
        assert_eq!(returned.rate_limits[0].enabled, Some(false));
        let policies = server.state.rate_limit_store.list().await;
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].limit_value, 10);
        assert!(!policies[0].enabled);

        server
            .update_key(update_request(
                &key,
                proto::UpdateKeyOperation::default(),
                &["rate_limits"],
            ))
            .await
            .expect("clear direct rate limits");
        assert!(server.state.rate_limit_store.list().await.is_empty());
    }

    #[tokio::test]
    async fn update_key_validates_rate_limits_against_replacement_permission() {
        let server = test_server();
        let key = create_key(&server).await;
        let permission = scoped_permission("conn-openai", "openai/gpt-4o");
        let mut rate_limit = rpm_limit(Some("conn-openai"));
        rate_limit.models = vec!["openai/gpt-4o".to_string()];

        server
            .update_key(update_request(
                &key,
                proto::UpdateKeyOperation {
                    permission: Some(permission),
                    rate_limits: vec![rate_limit],
                    ..Default::default()
                },
                &["permission", "rate_limits"],
            ))
            .await
            .expect("permission and rate-limit replacement");
        assert_eq!(server.state.rate_limit_store.list().await.len(), 1);
    }
}
