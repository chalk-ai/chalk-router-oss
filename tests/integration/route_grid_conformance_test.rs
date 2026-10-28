use std::{collections::BTreeMap, sync::Arc};

use openai_server::model_routes::{
    InMemoryModelRouteStore, ModelRouteStore, PostgresModelRouteStore, RouteInput, RouteListFilter,
    RouteStoreError, RouteUpdate,
};
use openai_server::route_grid::{
    Classification, CompletionJudgeBackend, DecisionsJudgeBackend, JudgeMode, JudgeOutcomeConfig,
    RouteGrid, RouteJudge, RouteJudgeBackend, RouteTarget,
};

fn grid() -> RouteGrid {
    RouteGrid {
        default_target: RouteTarget {
            connection_id: "conn".into(),
            upstream_model_id: "astra".into(),
        },
        fallbacks: BTreeMap::from([("conn/astra".into(), vec!["conn/gpt-5.6".into()])]),
        judge: Some(RouteJudge {
            mode: JudgeMode::Live,
            classification: Classification::Difficulty,
            backend: RouteJudgeBackend::Heuristic(Default::default()),
            outcomes: BTreeMap::from([
                (
                    "0".into(),
                    JudgeOutcomeConfig {
                        model: "conn/gpt-4o".into(),
                        description: "easy".into(),
                        fallbacks: Some(vec!["conn/gpt-5.5".into()]),
                    },
                ),
                (
                    "1".into(),
                    JudgeOutcomeConfig {
                        model: "conn/astra".into(),
                        description: "hard".into(),
                        fallbacks: None,
                    },
                ),
            ]),
            ..Default::default()
        }),
    }
}

async fn store_conformance(store: Arc<dyn ModelRouteStore>) -> (String, RouteGrid) {
    destination_validation_conformance(store.clone()).await;
    let expected = grid();
    expected.validate().unwrap();
    let route = store
        .create(RouteInput {
            public_model_id: "chalk/assistant".into(),
            connection_id: "conn".into(),
            upstream_model_id: "astra".into(),
            published: true,
            routing: Some(expected.clone()),
        })
        .await
        .unwrap();
    assert_eq!(route.routing.as_ref(), Some(&expected));
    assert_eq!(
        store
            .get_by_public_id("chalk/assistant")
            .await
            .unwrap()
            .unwrap()
            .routing,
        Some(expected.clone())
    );
    assert_eq!(
        store.list().await.unwrap()[0].routing.as_ref(),
        Some(&expected)
    );
    assert_eq!(
        store.list_by_connection("conn").await.unwrap()[0].routing,
        Some(expected.clone())
    );
    assert_eq!(
        store
            .list_page(None, 10, &RouteListFilter::default())
            .await
            .unwrap()[0]
            .routing,
        Some(expected.clone())
    );

    let updated = store
        .update(
            &route.id,
            RouteUpdate {
                published: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        updated.routing.as_ref(),
        Some(&expected),
        "unrelated updates preserve the grid"
    );
    let updated = store
        .update(
            &route.id,
            RouteUpdate {
                routing: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.routing, None, "an explicit clear removes routing");
    assert_eq!(updated.upstream_model_id, "astra");
    assert!(!updated.published);

    let mut changed = expected.clone();
    changed
        .judge
        .as_mut()
        .unwrap()
        .outcomes
        .get_mut("0")
        .unwrap()
        .fallbacks = Some(vec![]);
    store
        .update(
            &route.id,
            RouteUpdate {
                connection_id: Some("other-conn".into()),
                upstream_model_id: Some("gpt-4o".into()),
                published: Some(true),
                routing: Some(Some(changed.clone())),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .get_by_public_id("chalk/assistant")
            .await
            .unwrap()
            .unwrap()
            .routing,
        Some(changed.clone()),
        "explicit empty alternatives and inherited alternatives survive persistence"
    );
    assert_eq!(store.list_by_connection("conn").await.unwrap().len(), 1);
    assert!(store
        .list_by_connection("other-conn")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .list_page(
                None,
                10,
                &RouteListFilter {
                    connection_ids: vec!["conn".into()],
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(store
        .list_page(
            None,
            10,
            &RouteListFilter {
                connection_ids: vec!["other-conn".into()],
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .is_empty());
    // Remote backend configuration must retain its target and timeout through serialization.
    for backend in [
        RouteJudgeBackend::Completion(CompletionJudgeBackend {
            target: RouteTarget {
                connection_id: "conn".into(),
                upstream_model_id: "classifier".into(),
            },
            timeout_ms: 500,
        }),
        RouteJudgeBackend::Decisions(DecisionsJudgeBackend {
            target: RouteTarget {
                connection_id: "conn".into(),
                upstream_model_id: "decisions-classifier".into(),
            },
            timeout_ms: 2000,
        }),
    ] {
        changed.judge.as_mut().unwrap().backend = backend;
        let updated = store
            .update(
                &route.id,
                RouteUpdate {
                    routing: Some(Some(changed.clone())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.routing.as_ref(), Some(&changed));
        assert_eq!(updated.connection_id, "other-conn");
        assert_eq!(updated.upstream_model_id, "gpt-4o");
        assert!(updated.published);
    }
    (route.id, changed)
}

async fn destination_validation_conformance(store: Arc<dyn ModelRouteStore>) {
    // A custom route can omit the simple representation completely.
    let created = store
        .create(RouteInput {
            public_model_id: "chalk/custom-only".into(),
            connection_id: String::new(),
            upstream_model_id: String::new(),
            published: true,
            routing: Some(grid()),
        })
        .await
        .unwrap();
    // Clearing routing must validate the merged result, and failed changes
    // cannot damage the stored grid.
    assert!(matches!(
        store
            .update(
                &created.id,
                RouteUpdate {
                    routing: Some(None),
                    ..Default::default()
                }
            )
            .await,
        Err(RouteStoreError::InvalidTarget(_))
    ));
    assert_eq!(
        store
            .get_by_public_id("chalk/custom-only")
            .await
            .unwrap()
            .unwrap()
            .routing,
        Some(grid())
    );
    let simple = store
        .update(
            &created.id,
            RouteUpdate {
                routing: Some(None),
                connection_id: Some("simple".into()),
                upstream_model_id: Some("model".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(simple.default_connection_id(), "simple");
    assert_eq!(simple.default_upstream_model_id(), "model");
    assert!(matches!(
        store
            .update(
                &created.id,
                RouteUpdate {
                    connection_id: Some(String::new()),
                    ..Default::default()
                }
            )
            .await,
        Err(RouteStoreError::InvalidTarget(_))
    ));
    let mut missing_default = grid();
    missing_default.default_target.connection_id.clear();
    assert!(matches!(
        store
            .update(
                &created.id,
                RouteUpdate {
                    routing: Some(Some(missing_default)),
                    ..Default::default()
                }
            )
            .await,
        Err(RouteStoreError::InvalidTarget(_))
    ));
    store.delete(&created.id).await.unwrap();
    assert!(matches!(
        store
            .create(RouteInput {
                public_model_id: "chalk/incomplete".into(),
                connection_id: String::new(),
                upstream_model_id: "model".into(),
                published: true,
                routing: None,
            })
            .await,
        Err(RouteStoreError::InvalidTarget(_))
    ));
}

async fn default_connection_deletion_conformance(
    routes: Arc<dyn ModelRouteStore>,
    connections: Arc<dyn openai_server::provider_connections::ProviderConnectionStore>,
) {
    use openai_server::provider_connections::{
        ConnectionInput, DeleteConnectionOutcome, ExposurePolicy,
    };
    let mut ids = Vec::new();
    for name in ["ignored-simple", "custom-default", "replacement-simple"] {
        ids.push(
            connections
                .create(ConnectionInput {
                    name: name.into(),
                    provider_kind: "openai-compatible".into(),
                    base_url: Some("https://example.invalid/v1".into()),
                    prefix: None,
                    exposure: ExposurePolicy::Explicit,
                    routing_enabled: true,
                    declared_capabilities: None,
                    api_key: "unused-test-key".into(),
                })
                .await
                .unwrap()
                .id,
        );
    }
    let route = routes
        .create(RouteInput {
            public_model_id: "chalk/delete-default-guard".into(),
            connection_id: ids[0].clone(),
            upstream_model_id: "simple-model".into(),
            published: true,
            routing: Some(RouteGrid {
                default_target: RouteTarget {
                    connection_id: ids[1].clone(),
                    upstream_model_id: "custom-model".into(),
                },
                fallbacks: BTreeMap::new(),
                judge: None,
            }),
        })
        .await
        .unwrap();
    let blocked = connections.delete_if_unreferenced(&ids[1]).await.unwrap();
    assert!(
        matches!(blocked, DeleteConnectionOutcome::Referenced { public_model_ids }
        if public_model_ids == vec!["chalk/delete-default-guard"])
    );
    assert!(connections.get_metadata(&ids[1]).await.unwrap().is_some());
    assert!(matches!(
        connections.delete_if_unreferenced(&ids[0]).await.unwrap(),
        DeleteConnectionOutcome::Deleted { .. }
    ));
    // Removing custom routing changes which connection the route references.
    routes
        .update(
            &route.id,
            RouteUpdate {
                routing: Some(None),
                connection_id: Some(ids[2].clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        connections.delete_if_unreferenced(&ids[1]).await.unwrap(),
        DeleteConnectionOutcome::Deleted { .. }
    ));
    let blocked = connections.delete_if_unreferenced(&ids[2]).await.unwrap();
    assert!(
        matches!(blocked, DeleteConnectionOutcome::Referenced { public_model_ids }
        if public_model_ids == vec!["chalk/delete-default-guard"])
    );
    routes.delete(&route.id).await.unwrap();
    assert!(matches!(
        connections.delete_if_unreferenced(&ids[2]).await.unwrap(),
        DeleteConnectionOutcome::Deleted { .. }
    ));
}

#[tokio::test]
async fn memory_route_grid_conformance() {
    let routes = Arc::new(InMemoryModelRouteStore::new());
    let connections = Arc::new(
        openai_server::provider_connections::InMemoryProviderConnectionStore::with_routes(
            routes.clone(),
        ),
    );
    default_connection_deletion_conformance(routes.clone(), connections).await;
    store_conformance(routes).await;
}

#[tokio::test]
async fn postgres_route_grid_conformance() {
    let Some(url) = crate::support::db::test_database_url("postgres_route_grid_conformance") else {
        return;
    };
    let _lock = crate::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let environment = format!("route-grid-{}", uuid::Uuid::new_v4());
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let store = Arc::new(PostgresModelRouteStore::new(
        pool.clone(),
        environment.clone(),
    ));
    let cipher = openai_server::secrets::SecretCipher::from_base64(
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    )
    .unwrap();
    let connections = Arc::new(
        openai_server::provider_connections::PostgresProviderConnectionStore::new(
            pool.clone(),
            cipher,
            environment.clone(),
        ),
    );
    default_connection_deletion_conformance(store.clone(), connections).await;
    let (id, expected) = store_conformance(store.clone()).await;
    route_admin_conformance(store).await;
    pool.close().await;

    // Recreate both pool and store to prove the grid survives process-local state.
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let reloaded = PostgresModelRouteStore::new(pool.clone(), environment.clone());
    assert_eq!(
        reloaded
            .get_by_public_id("chalk/assistant")
            .await
            .unwrap()
            .unwrap()
            .routing,
        Some(expected.clone())
    );
    let other = PostgresModelRouteStore::new(pool.clone(), format!("{environment}-other"));
    assert!(other
        .get_by_public_id("chalk/assistant")
        .await
        .unwrap()
        .is_none());
    assert!(other.list().await.unwrap().is_empty());
    assert!(matches!(
        other
            .update(
                &id,
                RouteUpdate {
                    routing: Some(None),
                    ..Default::default()
                }
            )
            .await,
        Err(RouteStoreError::NotFound)
    ));
    assert_eq!(
        reloaded
            .get_by_public_id("chalk/assistant")
            .await
            .unwrap()
            .unwrap()
            .routing,
        Some(expected.clone()),
        "an update in another environment must not alter this grid"
    );
    sqlx::query("DELETE FROM chalk_router.model_routes WHERE environment_id = $1")
        .bind(&environment)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_route_grid_legacy_defaults_and_decode_errors() {
    let Some(url) = crate::support::db::test_database_url(
        "postgres_route_grid_legacy_defaults_and_decode_errors",
    ) else {
        return;
    };
    let _lock = crate::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let environment = format!("route-grid-json-{}", uuid::Uuid::new_v4());
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let store = PostgresModelRouteStore::new(pool.clone(), environment.clone());
    // Simulate an older writer that does not supply the routing column.
    sqlx::query(
        "INSERT INTO chalk_router.model_routes \
         (environment_id, id, public_model_id, connection_id, upstream_model_id) \
         VALUES ($1, 'legacy', 'chalk/legacy', 'conn', 'astra')",
    )
    .bind(&environment)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        store
            .get_by_public_id("chalk/legacy")
            .await
            .unwrap()
            .unwrap()
            .routing,
        None
    );
    let created = store
        .create(RouteInput {
            public_model_id: "chalk/test".into(),
            connection_id: "conn".into(),
            upstream_model_id: "astra".into(),
            published: true,
            routing: None,
        })
        .await
        .unwrap();
    assert_eq!(created.routing, None);

    // A clear is persisted as the schema's empty object, never JSON null.
    store
        .update(
            &created.id,
            RouteUpdate {
                routing: Some(Some(grid())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let cleared = store
        .update(
            &created.id,
            RouteUpdate {
                routing: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(cleared.routing.is_none());
    let stored: serde_json::Value = sqlx::query_scalar(
        "SELECT routing FROM chalk_router.model_routes WHERE environment_id = $1 AND id = $2",
    )
    .bind(&environment)
    .bind(&created.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, serde_json::json!({}));

    // Only {} means no routing. Malformed or incomplete nonempty objects must
    // never silently turn into a simple route.
    for invalid in [
        serde_json::json!({"fallbacks": {}}),
        serde_json::json!({"default_target": null}),
        serde_json::json!({"default_target": {"connection_id": "conn", "upstream_model_id": ""}}),
        serde_json::json!({"default_target": {"connection_id": "conn", "upstream_model_id": "astra"}, "judge": {"mode": "invalid"}}),
    ] {
        sqlx::query("UPDATE chalk_router.model_routes SET routing = $2 WHERE environment_id = $1 AND id = 'legacy'")
            .bind(&environment)
            .bind(invalid)
            .execute(&pool)
            .await
            .unwrap();
        // An unreadable routing fails only its own route, never the listing.
        let bad = store
            .get_by_public_id("chalk/legacy")
            .await
            .unwrap()
            .unwrap();
        assert!(bad.routing.is_none() && bad.routing_error.is_some());
        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(
            listed
                .iter()
                .filter(|route| route.routing_error.is_some())
                .count(),
            1
        );
        assert!(listed
            .iter()
            .find(|route| route.public_model_id == "chalk/test")
            .unwrap()
            .routing_error
            .is_none());
        assert_eq!(
            store
                .list_page(None, 10, &RouteListFilter::default())
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(store.list_by_connection("conn").await.is_ok());
    }

    // A field this server does not know (e.g. written by a newer replica)
    // isolates the route; it can still be unpublished and then repaired.
    sqlx::query("UPDATE chalk_router.model_routes SET routing = $2 WHERE environment_id = $1 AND id = 'legacy'")
        .bind(&environment)
        .bind(serde_json::json!({"default_target":{"connection_id":"c","upstream_model_id":"m"},"future_field":1}))
        .execute(&pool)
        .await
        .unwrap();
    let listed = store.list().await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed
        .iter()
        .find(|route| route.id == "legacy")
        .unwrap()
        .routing_error
        .is_some());
    assert!(listed
        .iter()
        .find(|route| route.id == created.id)
        .unwrap()
        .routing_error
        .is_none());
    let unpublished = store
        .update(
            "legacy",
            RouteUpdate {
                published: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!unpublished.published && unpublished.routing_error.is_some());
    let repaired = store
        .update(
            "legacy",
            RouteUpdate {
                routing: Some(None),
                connection_id: Some("conn".into()),
                upstream_model_id: Some("astra".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(repaired.routing_error.is_none() && repaired.routing.is_none());
    sqlx::query("DELETE FROM chalk_router.model_routes WHERE environment_id = $1")
        .bind(&environment)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

async fn admin_rpc<Q: prost::Message, R: prost::Message + Default>(
    base: &str,
    method: &str,
    request: Q,
) -> Result<R, i32> {
    let payload = request.encode_to_vec();
    let mut frame = vec![0];
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend(payload);
    let response = reqwest::Client::new()
        .post(format!("{base}/chalk.router.v1.{method}"))
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .body(frame)
        .send()
        .await
        .unwrap();
    if let Some(status) = response.headers().get("grpc-status") {
        let status: i32 = status.to_str().unwrap().parse().unwrap();
        if status != 0 {
            return Err(status);
        }
    }
    let body = response.bytes().await.unwrap();
    assert_eq!(body[0], 0, "expected protobuf data frame");
    let len = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
    Ok(R::decode(&body[5..5 + len]).unwrap())
}

#[tokio::test]
async fn route_admin_roundtrip_masks_and_concrete_resolution() {
    route_admin_conformance(Arc::new(InMemoryModelRouteStore::new())).await;
}

async fn route_admin_conformance(routes: Arc<dyn ModelRouteStore>) {
    use openai_server::{
        backend::{BackendProvider, RouterBackend},
        grpc::{self, proto as p},
        provider_connections::{
            ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore,
            ProviderConnectionStore,
        },
    };
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    let connection = connections
        .create(ConnectionInput {
            name: "private".into(),
            provider_kind: "openai-compatible".into(),
            base_url: Some("https://example.invalid/v1".into()),
            prefix: None,
            exposure: ExposurePolicy::Explicit,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "unused".into(),
        })
        .await
        .unwrap();
    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections.clone(),
        routes.clone(),
    ));
    let state = crate::support::app::AppStateBuilder::new(backend.clone())
        .provider_connection_store(connections)
        .model_route_store(routes)
        .build()
        .await;
    let base =
        crate::support::mock::serve(grpc::build_grpc_router(state, grpc::HealthState::new())).await;
    let mut routing = p::RouteGrid {
        default_target: Some(p::RouteTarget {
            connection_id: connection.id.clone(),
            upstream_model_id: "vendor/astra".into(),
        }),
        fallbacks: std::collections::HashMap::from([(
            "conn/astra".into(),
            p::RouteFallbacks {
                models: vec!["conn/opus".into()],
            },
        )]),
        judge: Some(p::RouteJudge {
            mode: p::JudgeMode::Live as i32,
            classification: p::Classification::Task as i32,
            backend: Some(p::RouteJudgeBackend {
                kind: Some(p::route_judge_backend::Kind::Completion(
                    p::CompletionJudgeBackend {
                        target: Some(p::RouteTarget {
                            connection_id: connection.id.clone(),
                            upstream_model_id: "judge".into(),
                        }),
                        timeout_ms: 500,
                    },
                )),
            }),
            outcomes: std::collections::HashMap::from([
                (
                    "code".into(),
                    p::JudgeOutcomeConfig {
                        model: "conn/astra".into(),
                        description: "code".into(),
                        fallbacks: Some(p::RouteFallbacks { models: vec![] }),
                    },
                ),
                (
                    "chat".into(),
                    p::JudgeOutcomeConfig {
                        model: "conn/astra".into(),
                        description: "conversation".into(),
                        fallbacks: None,
                    },
                ),
            ]),
        }),
    };
    let _: p::ValidateRouteJudgeBackendResponse = admin_rpc(
        &base,
        "ModelRouteService/ValidateRouteJudgeBackend",
        p::ValidateRouteJudgeBackendRequest {
            backend: routing.judge.as_ref().unwrap().backend.clone(),
        },
    )
    .await
    .unwrap();
    // Prime the resolution cache before creation: the first lookup after the
    // admin RPC must already see the new route, without waiting for its TTL.
    assert!(backend.resolve("chalk/grid").await.is_err());
    let created: p::CreateModelRouteResponse = admin_rpc(
        &base,
        "ModelRouteService/CreateModelRoute",
        p::CreateModelRouteRequest {
            public_model_id: "chalk/grid".into(),
            connection_id: "missing-simple-connection".into(),
            upstream_model_id: "simple-model".into(),
            published: true,
            routing: Some(routing.clone()),
        },
    )
    .await
    .unwrap();
    let created = created.route.unwrap();
    assert_eq!(created.routing, Some(routing.clone()));
    assert_eq!(created.diagnostic, p::RouteDiagnostic::Ready as i32);
    assert_eq!(created.connection_id, "missing-simple-connection");
    let resolved = backend.resolve("chalk/grid").await.unwrap();
    assert_eq!(
        resolved
            .routing
            .as_ref()
            .unwrap()
            .judge
            .as_ref()
            .unwrap()
            .outcomes["code"]
            .fallbacks,
        Some(vec![])
    );
    assert_eq!(resolved.stripped_model, "vendor/astra");
    let concrete = backend
        .resolve_concrete(&format!("{}/vendor/astra", connection.id))
        .await
        .unwrap();
    assert_eq!(
        concrete.resolution.connection_id.as_deref(),
        Some(connection.id.as_str())
    );
    assert_eq!(concrete.stripped_model, "vendor/astra");
    assert!(concrete.routing.is_none());
    assert!(backend.resolve_concrete("chalk/grid").await.is_err());
    // A routing-only edit must replace the cached policy on the very next
    // request, even though its default connection/model did not change.
    routing.judge.as_mut().unwrap().mode = p::JudgeMode::Shadow as i32;
    routing
        .judge
        .as_mut()
        .unwrap()
        .outcomes
        .get_mut("code")
        .unwrap()
        .fallbacks = Some(p::RouteFallbacks {
        models: vec!["conn/opus".into()],
    });
    let _: p::UpdateModelRouteResponse = admin_rpc(
        &base,
        "ModelRouteService/UpdateModelRoute",
        p::UpdateModelRouteRequest {
            id: created.id.clone(),
            update: Some(p::UpdateModelRouteOperation {
                routing: Some(routing.clone()),
                ..Default::default()
            }),
            update_mask: None,
        },
    )
    .await
    .unwrap();
    let refreshed = backend.resolve("chalk/grid").await.unwrap();
    let judge = refreshed.routing.as_ref().unwrap().judge.as_ref().unwrap();
    assert_eq!(judge.mode, JudgeMode::Shadow);
    assert_eq!(
        judge.outcomes["code"].fallbacks,
        Some(vec!["conn/opus".into()])
    );
    // An excluded malformed field must not be parsed, validated, or applied.
    let invalid = p::RouteGrid {
        judge: Some(p::RouteJudge {
            mode: 999,
            ..Default::default()
        }),
        ..Default::default()
    };
    let updated: p::UpdateModelRouteResponse = admin_rpc(
        &base,
        "ModelRouteService/UpdateModelRoute",
        p::UpdateModelRouteRequest {
            id: created.id.clone(),
            update: Some(p::UpdateModelRouteOperation {
                published: Some(false),
                routing: Some(invalid.clone()),
                ..Default::default()
            }),
            update_mask: Some(prost_types::FieldMask {
                paths: vec!["published".into()],
            }),
        },
    )
    .await
    .unwrap();
    assert_eq!(updated.route.unwrap().routing, Some(routing));
    assert!(backend.resolve("chalk/grid").await.is_err());
    let invalid_update: Result<p::UpdateModelRouteResponse, _> = admin_rpc(
        &base,
        "ModelRouteService/UpdateModelRoute",
        p::UpdateModelRouteRequest {
            id: created.id.clone(),
            update: Some(p::UpdateModelRouteOperation {
                routing: Some(invalid),
                ..Default::default()
            }),
            update_mask: None,
        },
    )
    .await;
    assert_eq!(
        invalid_update.unwrap_err(),
        tonic::Code::InvalidArgument as i32
    );
    let cleared: p::UpdateModelRouteResponse = admin_rpc(
        &base,
        "ModelRouteService/UpdateModelRoute",
        p::UpdateModelRouteRequest {
            id: created.id,
            update: None,
            update_mask: Some(prost_types::FieldMask {
                paths: vec!["routing".into()],
            }),
        },
    )
    .await
    .unwrap();
    let cleared = cleared.route.unwrap();
    assert_eq!(cleared.routing, None);
    assert_eq!(
        cleared.diagnostic,
        p::RouteDiagnostic::MissingConnection as i32
    );
    assert!(!cleared.published);
    // The already resolved request retains the complete old policy snapshot.
    assert!(resolved.routing.as_ref().unwrap().judge.is_some());
    let retired: Result<p::GetFallbackPolicyResponse, _> = admin_rpc(
        &base,
        "FallbackPolicyService/GetFallbackPolicy",
        p::GetFallbackPolicyRequest {},
    )
    .await;
    assert_eq!(retired.unwrap_err(), tonic::Code::FailedPrecondition as i32);
    let retired: Result<p::UpdateJudgePolicyResponse, _> = admin_rpc(
        &base,
        "JudgePolicyService/UpdateJudgePolicy",
        p::UpdateJudgePolicyRequest::default(),
    )
    .await;
    assert_eq!(retired.unwrap_err(), tonic::Code::FailedPrecondition as i32);
}
