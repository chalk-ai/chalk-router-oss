use std::sync::Arc;

use axum::{body::Body, routing::get, Extension, Router};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use openai_server::{
    backend::RouterBackend,
    model_routes::InMemoryModelRouteStore,
    provider_connections::InMemoryProviderConnectionStore,
    tenancy::{EnvironmentRegistry, RequestRuntime, RequestState},
    AppState,
};
use tower::ServiceExt;

use crate::support::app::AppStateBuilder;

pub async fn runtime(environment: &str) -> AppState {
    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        Arc::new(InMemoryProviderConnectionStore::new()),
        Arc::new(InMemoryModelRouteStore::new()),
    ));
    AppStateBuilder::new(backend)
        .environment_id(environment)
        .build()
        .await
}

async fn environment(RequestState(state): RequestState<AppState>) -> String {
    state.environment_id
}

#[tokio::test]
async fn request_runtime_overrides_default_for_http_and_tonic() {
    let a = runtime("a").await;
    let b = runtime("b").await;
    let app = Router::new()
        .route("/", get(environment))
        .with_state(a.clone())
        .layer(Extension(RequestRuntime(b.clone())));
    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "b"
    );
    let mut request = tonic::Request::new(());
    request.extensions_mut().insert(RequestRuntime(b));
    assert_eq!(a.for_request(&request).environment_id, "b");
    assert_eq!(a.for_request(&tonic::Request::new(())).environment_id, "a");
}

#[tokio::test]
async fn runtime_factory_shares_clients_but_isolates_keys_diagnostics_and_quota() {
    use openai_server::{
        api_keys::ApiKeyPermission,
        config::{QuotaBackendKind, RouterConfig, UsageBackendKind},
        environment_runtime::RuntimeFactory,
        hashing::KeyHasher,
    };
    let mut config = RouterConfig::from_env();
    config.usage_backend = UsageBackendKind::Disabled;
    config.quota_backend = QuotaBackendKind::Memory;
    let factory = RuntimeFactory {
        shared: runtime("legacy").await,
        pool: None,
        cipher: None,
        config,
        hasher: KeyHasher::new(b"same-cluster-pepper".to_vec()),
    };
    let (a, _) = factory.build("a").await.unwrap();
    let (b, _) = factory.build("b").await.unwrap();
    assert!(!Arc::ptr_eq(&a.backend, &b.backend));
    assert!(!Arc::ptr_eq(&a.route_judge, &b.route_judge));
    assert!(!Arc::ptr_eq(&a.quota_backend, &b.quota_backend));
    let key = a
        .api_key_store
        .create(
            "a key".into(),
            ApiKeyPermission::default(),
            Default::default(),
            None,
        )
        .await
        .unwrap();
    assert!(a.api_key_store.authenticate(&key.raw_key).await.is_some());
    assert!(b.api_key_store.authenticate(&key.raw_key).await.is_none());
    let locator = a
        .response_locator
        .encode("same-connection", None, "same-response");
    assert!(b.response_locator.decode(&locator).is_err());
    assert!(b.debug_log.recent().await.is_empty());
}

#[tokio::test]
async fn cluster_environments_cannot_borrow_the_router_identity_or_reach_private_hosts() {
    use openai_server::{
        config::{ClusterConfig, QuotaBackendKind, RouterConfig, UsageBackendKind},
        environment_runtime::RuntimeFactory,
        hashing::KeyHasher,
        provider_connections::{ConnectionInput, ExposurePolicy},
    };
    let mut config = RouterConfig::from_env();
    config.usage_backend = UsageBackendKind::Disabled;
    config.quota_backend = QuotaBackendKind::Memory;
    config.legacy_environment_id = Some("legacy".into());
    config.cluster = Some(ClusterConfig {
        metadata_api_endpoint: "http://metadata.invalid".into(),
        metadata_api_token: "token".into(),
        kube_cluster_id: None,
    });
    let factory = RuntimeFactory {
        shared: runtime("legacy").await,
        pool: None,
        cipher: None,
        config,
        hasher: KeyHasher::new(b"cluster-pepper".to_vec()),
    };
    let keyless_bedrock = || ConnectionInput {
        name: "bedrock-runtime".into(),
        provider_kind: "bedrock".into(),
        base_url: Some("https://bedrock-runtime.us-west-2.amazonaws.com".into()),
        prefix: Some("bedrock/".into()),
        exposure: ExposurePolicy::Dynamic,
        routing_enabled: true,
        declared_capabilities: None,
        api_key: String::new(),
    };

    // The legacy environment keeps the operator's AWS identity.
    let (legacy, _) = factory.build("legacy").await.unwrap();
    legacy
        .provider_connection_service()
        .create(keyless_bedrock())
        .await
        .expect("legacy environment may use the router's AWS identity");

    // The legacy environment may target private hosts, such as an in-cluster vLLM.
    let endpoint = |name: &str, kind: &str, base_url: &str| ConnectionInput {
        name: name.into(),
        provider_kind: kind.into(),
        base_url: Some(base_url.into()),
        prefix: Some(format!("{name}/")),
        exposure: ExposurePolicy::Dynamic,
        routing_enabled: true,
        declared_capabilities: None,
        api_key: "unused".into(),
    };
    legacy
        .provider_connection_service()
        .create(endpoint("gemma", "vllm", "http://10.0.0.5:8000/v1"))
        .await
        .expect("legacy environment may target private hosts");

    // Every other environment must bring its own key and reach public hosts only.
    let (tenant, _) = factory.build("tenant").await.unwrap();
    for (name, kind, base_url) in [
        ("gemma", "vllm", "http://10.0.0.5:8000/v1"),
        ("anything", "openai-compatible", "http://169.254.169.254/v1"),
        ("openai", "openai", "http://127.0.0.1:4001/v1"),
        (
            "svc",
            "openai-compatible",
            "http://vllm.default.svc.cluster.local/v1",
        ),
    ] {
        let error = tenant
            .provider_connection_service()
            .create(endpoint(name, kind, base_url))
            .await
            .unwrap_err();
        assert!(
            error.message().contains("not allowed"),
            "{base_url}: {}",
            error.message()
        );
    }
    tenant
        .provider_connection_service()
        .create(endpoint("mine", "vllm", "https://vllm.customer.example/v1"))
        .await
        .expect("a public custom endpoint is allowed");

    // The tenant's HTTP client refuses a name that resolves to a private
    // address, which an IP-literal check cannot see; the legacy client does not.
    let mock = crate::support::mock::serve(Router::new().route("/", get(|| async { "ok" }))).await;
    let port = mock.rsplit(':').next().unwrap();
    let by_name = format!("http://localhost:{port}/");
    let refused = tenant.http_client.get(&by_name).send().await;
    assert!(refused.is_err(), "tenant client reached a loopback host");
    assert!(
        legacy.http_client.get(&by_name).send().await.is_ok(),
        "legacy client should keep reaching private hosts"
    );
    let error = tenant
        .provider_connection_service()
        .create(keyless_bedrock())
        .await
        .unwrap_err();
    assert!(
        error.message().contains("Bedrock API key"),
        "{}",
        error.message()
    );
    let mut keyed = keyless_bedrock();
    keyed.api_key = "tenant-bedrock-key".into();
    tenant
        .provider_connection_service()
        .create(keyed)
        .await
        .expect("a tenant-supplied Bedrock key is allowed");
}

#[test]
fn overlapping_client_trace_and_span_ids_do_not_share_sampling_overrides() {
    use openai_server::telemetry::{
        sampling::{
            SamplingContextSpanProcessor, SamplingGate, SamplingGateSpanProcessor,
            TenantAttributionSpanProcessor, TenantSampling,
        },
        ENVIRONMENT_ID_ATTRIBUTE, SAMPLING_DECISION_ATTRIBUTE,
    };
    use opentelemetry::{
        baggage::BaggageExt,
        trace::{Span as _, SpanId, TraceId, Tracer as _, TracerProvider as _},
        Context, KeyValue,
    };
    use opentelemetry_sdk::{
        testing::trace::InMemorySpanExporter,
        trace::{SimpleSpanProcessor, TracerProvider},
    };
    let exporter = InMemorySpanExporter::default();
    let root = Arc::new(SamplingGate::new());
    let a = Arc::new(SamplingGate::new());
    let b = Arc::new(SamplingGate::new());
    let provider = TracerProvider::builder()
        .with_span_processor(TenantAttributionSpanProcessor)
        .with_span_processor(SamplingGateSpanProcessor::new(
            SimpleSpanProcessor::new(Box::new(exporter.clone())),
            root.clone(),
        ))
        .with_span_processor(SamplingContextSpanProcessor)
        .build();
    let tracer = provider.tracer("tenant-sampling");
    let trace = TraceId::from(1u128);
    let span_id = SpanId::from(1u64);
    let context = |id: &str, gate| {
        Context::new()
            .with_value(TenantSampling {
                environment_id: id.into(),
                gate,
            })
            .with_baggage([KeyValue::new(SAMPLING_DECISION_ATTRIBUTE, "false")])
    };
    let mut span_a = tracer.build_with_context(
        tracer
            .span_builder("a")
            .with_trace_id(trace)
            .with_span_id(span_id),
        &context("a", a.clone()),
    );
    let mut span_b = tracer.build_with_context(
        tracer
            .span_builder("b")
            .with_trace_id(trace)
            .with_span_id(span_id),
        &context("b", b.clone()),
    );
    root.force_keep_trace(trace); // A process default must not override a tenant.
    a.force_keep_trace(trace);
    b.resolve_outcome(trace, "same-session", 0.0, true);
    span_b.end();
    span_a.end();
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].name, "a");
    assert!(spans[0]
        .attributes
        .iter()
        .any(|kv| kv.key.as_str() == ENVIRONMENT_ID_ATTRIBUTE && kv.value.as_str() == "a"));
    provider.shutdown().unwrap();
}

#[test]
fn multiple_destinations_stamp_one_environment_without_dropping_the_last_attribute() {
    use openai_server::telemetry::{
        sampling::{
            SamplingGate, SamplingGateSpanProcessor, TenantAttributionSpanProcessor, TenantSampling,
        },
        ENVIRONMENT_ID_ATTRIBUTE,
    };
    use opentelemetry::{
        trace::{Span as _, Tracer as _, TracerProvider as _},
        Context, KeyValue,
    };
    use opentelemetry_sdk::{
        testing::trace::InMemorySpanExporter,
        trace::{SimpleSpanProcessor, TracerProvider},
    };
    let a = InMemorySpanExporter::default();
    let b = InMemorySpanExporter::default();
    let gate = Arc::new(SamplingGate::new());
    let provider = TracerProvider::builder()
        .with_span_processor(TenantAttributionSpanProcessor)
        .with_span_processor(SamplingGateSpanProcessor::new(
            SimpleSpanProcessor::new(Box::new(a.clone())),
            gate.clone(),
        ))
        .with_span_processor(SamplingGateSpanProcessor::new(
            SimpleSpanProcessor::new(Box::new(b.clone())),
            gate.clone(),
        ))
        .with_max_attributes_per_span(2)
        .build();
    let tracer = provider.tracer("attribute-boundary");
    let context = Context::new().with_value(TenantSampling {
        environment_id: "a".into(),
        gate,
    });
    let mut span = tracer.start_with_context("llm", &context);
    span.set_attribute(KeyValue::new("last.writer.attribute", "retained"));
    span.end();
    for exporter in [a, b] {
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].dropped_attributes_count, 0);
        assert_eq!(
            spans[0]
                .attributes
                .iter()
                .filter(|kv| kv.key.as_str() == ENVIRONMENT_ID_ATTRIBUTE)
                .count(),
            1
        );
        assert!(spans[0]
            .attributes
            .iter()
            .any(|kv| kv.key.as_str() == "last.writer.attribute"));
    }
    provider.shutdown().unwrap();
}

#[test]
fn sampling_secrets_are_environment_specific_without_changing_key_hashes() {
    let hasher = openai_server::hashing::KeyHasher::new(b"existing-cluster-pepper".to_vec());
    assert_ne!(
        hasher.sampling_secret_for_environment("a"),
        hasher.sampling_secret_for_environment("b")
    );
    assert_eq!(
        hasher.sampling_secret_for_environment("a"),
        hasher.sampling_secret_for_environment("a")
    );
    assert_eq!(hasher.hash("ck-key"), hasher.clone().hash("ck-key"));
}

#[tokio::test]
async fn registry_rejects_duplicates_and_keeps_the_first_runtime() {
    let a = runtime("a").await;
    let b = runtime("b").await;
    assert!(EnvironmentRegistry::new(vec![a.clone(), a.clone()]).is_err());
    let registry = EnvironmentRegistry::new(vec![a.clone()]).unwrap();
    assert!(registry.get("b").is_none());
    assert_eq!(registry.insert(b.clone()).environment_id, "b");
    let replacement = runtime("b").await;
    let served = registry.insert(replacement);
    assert!(Arc::ptr_eq(&served.backend, &b.backend));
    assert_eq!(registry.len(), 2);
    assert_eq!(
        registry
            .runtimes()
            .iter()
            .map(|state| state.environment_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(registry.get("other").is_none());
}

fn granted(environment: &str) -> openai_server::cluster_auth::EnvironmentAccess {
    openai_server::cluster_auth::EnvironmentAccess::Granted(environment.into())
}

fn team_owned(environment: &str, team: &str) -> openai_server::cluster_auth::EnvironmentAccess {
    openai_server::cluster_auth::EnvironmentAccess::TeamOwned {
        environment_id: environment.into(),
        team_id: team.into(),
    }
}

fn two_team_directory() -> Arc<openai_server::environment_directory::StaticEnvironmentDirectory> {
    openai_server::environment_directory::StaticEnvironmentDirectory::new([
        ("a", "team-a"),
        ("b", "team-b"),
    ])
}

#[test]
fn chalk_claims_choose_environment_access_and_operation_permissions() {
    use openai_server::cluster_auth::{operation_permission, ChalkClaims};
    let claims = ChalkClaims {
        kind: Some("user".into()),
        team_id: Some("team-a".into()),
        team_permissions: vec!["query.online".into()],
        ..Default::default()
    };
    assert_eq!(claims.access(Some("a")).unwrap(), team_owned("a", "team-a"));
    assert!(claims.access(None).is_err());
    assert!(claims.permits("a", &["query.online"]));
    assert!(!claims.permits("a", &["tokens.write"]));
    let granted_user = ChalkClaims {
        kind: Some("user".into()),
        env_permissions: [("b".into(), vec!["query.online".into()])].into(),
        ..Default::default()
    };
    assert_eq!(granted_user.access(Some("b")).unwrap(), granted("b"));
    assert!(granted_user.access(Some("a")).is_err());
    let scoped = ChalkClaims {
        kind: Some("sandbox".into()),
        environment_id: Some("b".into()),
        permissions: vec!["query.online".into()],
        ..Default::default()
    };
    assert_eq!(scoped.access(None).unwrap(), granted("b"));
    assert!(scoped.access(Some("a")).is_err());
    assert!(!scoped.permits("a", &["query.online"]));
    for kind in ["exchange", "unknown", "user-exchange"] {
        assert!(ChalkClaims {
            kind: Some(kind.into()),
            environment_id: Some("a".into()),
            ..Default::default()
        }
        .access(None)
        .is_err());
    }
    assert_eq!(
        operation_permission("/chalk.router.v1.ApiKeyService/ListKeys"),
        Some(&["tokens.list"][..])
    );
    assert_eq!(
        operation_permission("/chalk.router.v1.ApiKeyService/CreateKey"),
        Some(&["tokens.write"][..])
    );
    assert_eq!(
        operation_permission("/v1/chat/completions"),
        Some(&["query.online"][..])
    );
    assert_eq!(
        operation_permission("/chalk.router.v1.UnknownService/Get"),
        None
    );
}

/// Every admin RPC must reject an inference-only principal, including in
/// cluster mode after its environment ownership has been established.
#[test]
fn admin_rpcs_require_explicit_operation_permissions() {
    use openai_server::cluster_auth::{ChalkClaims, ADMIN_OPERATION_PERMISSIONS};
    assert_eq!(ADMIN_OPERATION_PERMISSIONS.len(), 47);
    for (path, accepted) in ADMIN_OPERATION_PERMISSIONS {
        assert!(!accepted.contains(&"authenticated"), "{path}");
        assert!(!accepted.contains(&"query.online"), "{path}");
    }
    let member = ChalkClaims {
        kind: Some("user".into()),
        team_id: Some("team-a".into()),
        team_permissions: vec!["query.online".into()],
        ..Default::default()
    };
    for (path, _) in ADMIN_OPERATION_PERMISSIONS {
        assert!(!member.permits_operation("a", path), "{path}");
    }
    let exchange = ChalkClaims {
        kind: Some("exchange".into()),
        environment_id: Some("a".into()),
        ..Default::default()
    };
    assert!(!exchange.permits("a", &["authenticated"]));
}

#[tokio::test]
async fn chalk_privileged_token_semantics_remain_confined_to_the_verified_tenant() {
    use openai_server::cluster_auth::ChalkClaims;
    for permissions in [None, Some(serde_json::json!([]))] {
        let mut payload = serde_json::json!({"kind":"tenant", "environment_id":"a"});
        if let Some(permissions) = permissions {
            payload["permissions"] = permissions;
        }
        let claims: ChalkClaims = serde_json::from_value(payload).unwrap();
        assert_eq!(claims.access(None).unwrap(), granted("a"));
        assert!(claims.permits("a", &["tokens.write"]));
        assert!(claims.permits("a", &["secrets.decrypt"]));
        assert!(claims.access(Some("b")).is_err());
    }
    let limited: ChalkClaims = serde_json::from_value(
        serde_json::json!({"kind":"tenant", "environment_id":"a", "permissions":["query.online"]}),
    )
    .unwrap();
    assert!(limited.permits("a", &["query.online"]));
    assert!(!limited.permits("a", &["tokens.write"]));
    assert!(!limited.permits("a", &["secrets.decrypt"]));
    let support = ChalkClaims {
        kind: Some("user".into()),
        team_id: Some("team-a".into()),
        impersonated_team_id: Some("team-b".into()),
        ..Default::default()
    };
    assert_eq!(
        support.access(Some("b")).unwrap(),
        team_owned("b", "team-b")
    );
    assert!(support.permits("b", &["tokens.write"]));
    let service = ChalkClaims {
        kind: Some("service-account".into()),
        team_id: Some("team-a".into()),
        team_permissions: vec!["query.online".into()],
        ..Default::default()
    };
    assert_eq!(
        service.access(Some("a")).unwrap(),
        team_owned("a", "team-a")
    );
    for read in [
        "tokens.list",
        "secrets.list",
        "deploy.read",
        "monitoring.read",
        "logs.list",
    ] {
        assert!(!service.permits("a", &[read]));
    }
    for write in [
        "tokens.write",
        "secrets.write",
        "secrets.decrypt",
        "deploy.create",
        "monitoring.create",
    ] {
        assert!(!service.permits("a", &[write]));
    }
    let unprivileged = ChalkClaims {
        team_permissions: vec![],
        ..service
    };
    assert!(!unprivileged.permits("a", &["tokens.list"]));
    assert!(!unprivileged.permits("a", &["query.online"]));
}

struct UnavailableDirectory;

#[async_trait::async_trait]
impl openai_server::environment_directory::EnvironmentDirectory for UnavailableDirectory {
    async fn lookup(
        &self,
        _environment_id: &str,
    ) -> Result<
        Option<openai_server::environment_directory::EnvironmentRecord>,
        openai_server::environment_directory::DirectoryUnavailable,
    > {
        Err(openai_server::environment_directory::DirectoryUnavailable(
            "metadata server down".into(),
        ))
    }
}

#[tokio::test]
async fn directory_confirms_environments_and_team_ownership() {
    use openai_server::{
        cluster_auth::{ClusterAuth, EnvironmentDenied},
        hashing::KeyHasher,
    };
    let a = runtime("a").await;
    let registry = EnvironmentRegistry::new(vec![a.clone(), runtime("b").await]).unwrap();
    let cluster = |directory: Arc<
        dyn openai_server::environment_directory::EnvironmentDirectory,
    >| ClusterAuth {
        registry: registry.clone(),
        directory,
        provisioner: None,
        default_state: a.clone(),
        pool: None,
        hasher: KeyHasher::new(b"test-pepper".to_vec()),
    };
    let known = cluster(two_team_directory());
    let served =
        |result: Result<AppState, EnvironmentDenied>| result.map(|state| state.environment_id);
    assert_eq!(
        served(known.authorize(&team_owned("b", "team-b")).await).unwrap(),
        "b"
    );
    assert_eq!(served(known.authorize(&granted("a")).await).unwrap(), "a");
    assert!(matches!(
        known.authorize(&team_owned("a", "team-b")).await,
        Err(EnvironmentDenied::Forbidden(_))
    ));
    // Unknown, archived, suspended or other-cluster environments all look absent.
    assert!(matches!(
        known.authorize(&granted("c")).await,
        Err(EnvironmentDenied::Forbidden(_))
    ));

    let down = cluster(Arc::new(UnavailableDirectory));
    // A credential that names its environment keeps working while it is loaded.
    assert_eq!(served(down.authorize(&granted("a")).await).unwrap(), "a");
    assert!(matches!(
        down.authorize(&granted("c")).await,
        Err(EnvironmentDenied::Unavailable)
    ));
    // Team ownership cannot be proven without the directory.
    assert!(matches!(
        down.authorize(&team_owned("a", "team-a")).await,
        Err(EnvironmentDenied::Unavailable)
    ));
}

#[tokio::test]
async fn opaque_key_selects_owner_and_conflicting_header_is_denied() {
    use openai_server::{
        api_keys::ApiKeyPermission,
        cluster_auth::{cluster_auth_middleware, ClusterAuth},
        hashing::KeyHasher,
    };
    let a = runtime("a").await;
    let b = runtime("b").await;
    let key = b
        .api_key_store
        .create(
            "b key".into(),
            ApiKeyPermission::default(),
            Default::default(),
            None,
        )
        .await
        .unwrap();
    let registry = EnvironmentRegistry::new(vec![a.clone(), b.clone()]).unwrap();
    let cluster = Arc::new(ClusterAuth {
        registry,
        directory: two_team_directory(),
        provisioner: None,
        default_state: a.clone(),
        pool: None,
        hasher: KeyHasher::new(b"test-pepper".to_vec()),
    });
    let app = Router::new()
        .route("/", get(environment))
        .with_state(a)
        .layer(axum::middleware::from_fn_with_state(
            cluster,
            cluster_auth_middleware,
        ));
    for (header, status) in [
        (None, StatusCode::OK),
        (Some("a"), StatusCode::FORBIDDEN),
        (Some("b"), StatusCode::OK),
    ] {
        let mut request = Request::builder()
            .uri("/")
            .header("authorization", format!("Bearer {}", key.raw_key));
        if let Some(header) = header {
            request = request.header("x-chalk-env-id", header);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        if status == StatusCode::OK {
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "b"
            );
        }
    }
    b.api_key_store.revoke(&key.api_key.key_hash).await.unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header("authorization", format!("Bearer {}", key.raw_key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn postgres_key_directory_returns_owner_and_rejects_duplicates() {
    use openai_server::{
        api_keys::{ApiKeyPermission, ApiKeyStore, PostgresApiKeyStore},
        hashing::KeyHasher,
    };
    let Some(url) = crate::support::db::test_database_url("cluster key directory") else {
        return;
    };
    let _lock = crate::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let hasher = KeyHasher::new(uuid::Uuid::new_v4().as_bytes().to_vec());
    let a = PostgresApiKeyStore::new(pool.clone(), hasher.clone(), "directory-a".into());
    let b = PostgresApiKeyStore::new(pool.clone(), hasher.clone(), "directory-b".into());
    let key = b
        .create(
            "directory key".into(),
            ApiKeyPermission::default(),
            Default::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        PostgresApiKeyStore::authenticate_cluster(&pool, &hasher, &key.raw_key)
            .await
            .unwrap()
            .unwrap()
            .0,
        "directory-b"
    );
    assert!(
        PostgresApiKeyStore::authenticate_cluster(&pool, &hasher, "invalid")
            .await
            .unwrap()
            .is_none()
    );
    let duplicate = openai_server::api_keys::ApiKey {
        key_hash: key.api_key.key_hash.clone(),
        key_prefix: key.api_key.key_prefix.clone(),
        description: "duplicate".into(),
        created_at: chrono::Utc::now(),
        permission: Default::default(),
        labels: Default::default(),
        usage_pool_id: None,
        daily_token_budget: None,
    };
    a.insert_raw(duplicate).await.unwrap();
    assert!(
        PostgresApiKeyStore::authenticate_cluster(&pool, &hasher, &key.raw_key)
            .await
            .is_err()
    );
    a.revoke(&key.api_key.key_hash).await.unwrap();
    b.revoke(&key.api_key.key_hash).await.unwrap();
    assert!(
        PostgresApiKeyStore::authenticate_cluster(&pool, &hasher, &key.raw_key)
            .await
            .unwrap()
            .is_none()
    );
    let index: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_indexes WHERE schemaname='chalk_router' AND indexname='idx_router_api_keys_key_hash')").fetch_one(&pool).await.unwrap();
    assert!(index);
    pool.close().await;
}

#[tokio::test]
async fn verified_jwt_checks_permissions_and_cannot_reuse_another_issuer_cache() {
    use crate::support::auth::{chalk_jwt, federated_registry, future_exp};
    use openai_server::{
        cluster_auth::{cluster_auth_middleware, ClusterAuth},
        hashing::KeyHasher,
    };
    let mut a = runtime("a").await;
    let mut b = runtime("b").await;
    let registry_auth = federated_registry("cluster-test", "CLUSTER_TEST_SECRET", "cluster-secret");
    a.federated_auth = registry_auth.clone();
    b.federated_auth = registry_auth;
    let registry = EnvironmentRegistry::new(vec![a.clone(), b]).unwrap();
    let app_for = |default_state: AppState| {
        Router::new()
            .route("/", get(environment))
            .with_state(default_state.clone())
            .layer(axum::middleware::from_fn_with_state(
                Arc::new(ClusterAuth {
                    registry: registry.clone(),
                    directory: two_team_directory(),
                    provisioner: None,
                    default_state,
                    pool: None,
                    hasher: KeyHasher::new(b"test-pepper".to_vec()),
                }),
                cluster_auth_middleware,
            ))
    };
    let app = app_for(a.clone());
    for (kind, permissions, expected) in [
        ("service-account", vec!["query.online"], StatusCode::OK),
        ("service-account", vec![], StatusCode::FORBIDDEN),
        ("exchange", vec!["query.online"], StatusCode::FORBIDDEN),
    ] {
        let token = chalk_jwt(
            "cluster-secret",
            &serde_json::json!({"iss":"cluster-test", "sub":"client", "exp":future_exp(), "kind":kind, "environment_id":"b", "permissions":permissions}),
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        if expected == StatusCode::OK {
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "b"
            );
            a.federated_auth =
                federated_registry("cluster-test", "CLUSTER_OTHER_SECRET", "other-secret");
            let response = app_for(a.clone())
                .oneshot(
                    Request::builder()
                        .uri("/")
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }
}

/// Exercise the real provider adapter, HTTP handlers, native gRPC and gRPC-Web
/// against identical tenant-local names, with shared infrastructure/credentials.
async fn cluster_wire_conformance(pool: Option<sqlx::PgPool>) {
    use crate::support::{
        auth::{chalk_jwt, federated_registry, future_exp},
        mock::serve,
    };
    use openai_server::{
        api_keys::ApiKeyPermission,
        cluster_auth::{cluster_auth_middleware, ClusterAuth},
        config::{QuotaBackendKind, RouterConfig, UsageBackendKind},
        environment_runtime::RuntimeFactory,
        grpc::{
            build_grpc_router,
            proto::{ListProviderConnectionsRequest, ListProviderConnectionsResponse},
            HealthState,
        },
        hashing::KeyHasher,
        provider_connections::{ConnectionInput, ConnectionUpdate, ExposurePolicy},
        secrets::SecretCipher,
    };
    use prost::Message;
    use serde_json::{json, Value};
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recorded = calls.clone();
    let upstream = serve(Router::new().route("/chat/completions", axum::routing::post(move |headers: http::HeaderMap, axum::Json(body): axum::Json<Value>| {
        let recorded = recorded.clone();
        async move {
            recorded.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let credential = headers["authorization"].to_str().unwrap();
            let (mime, reply) = if body["stream"] == true {
                ("text/event-stream", format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    json!({"id":"same-id","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":credential},"finish_reason":null}]}),
                    json!({"id":"same-id","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}})))
            } else {
                ("application/json", json!({"id":"same-id","object":"chat.completion","created":1,"model":"gpt-4o","choices":[{"index":0,"message":{"role":"assistant","content":credential},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}).to_string())
            };
            axum::response::Response::builder().header("content-type", mime).body(Body::from(reply)).unwrap()
        }
    }))).await;
    let a_id = format!("wire-a-{}", uuid::Uuid::new_v4());
    let b_id = format!("wire-b-{}", uuid::Uuid::new_v4());
    let mut shared = runtime(&a_id).await;
    shared.federated_auth =
        federated_registry("wire-cluster", "WIRE_CLUSTER_SECRET", "wire-test-secret");
    let mut config = RouterConfig::from_env();
    config.usage_backend = if pool.is_some() {
        UsageBackendKind::Postgres
    } else {
        UsageBackendKind::Disabled
    };
    config.quota_backend = QuotaBackendKind::Memory;
    config.legacy_environment_id = Some(a_id.clone());
    let factory = RuntimeFactory {
        shared,
        pool: pool.clone(),
        // Same KEK/pepper for both environments and the reconstructed runtime.
        cipher: Some(
            SecretCipher::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap(),
        ),
        config,
        hasher: KeyHasher::new(uuid::Uuid::new_v4().as_bytes().to_vec()),
    };
    let (a, _) = factory.build(&a_id).await.unwrap();
    let (b, _) = factory.build(&b_id).await.unwrap();
    let mut keys = Vec::new();
    let mut connections = Vec::new();
    for (state, credential) in [(&a, "provider-a"), (&b, "provider-b")] {
        connections.push(
            state
                .provider_connection_store
                .create(ConnectionInput {
                    name: "same-connection".into(),
                    provider_kind: "openai".into(),
                    base_url: Some(upstream.clone()),
                    prefix: Some("same/".into()),
                    exposure: ExposurePolicy::Dynamic,
                    routing_enabled: true,
                    declared_capabilities: None,
                    api_key: credential.into(),
                })
                .await
                .unwrap(),
        );
        keys.push(
            state
                .api_key_store
                .create(
                    "same-key-name".into(),
                    ApiKeyPermission::default(),
                    Default::default(),
                    None,
                )
                .await
                .unwrap(),
        );
    }
    assert!(a
        .provider_connection_store
        .get(&connections[1].id)
        .await
        .unwrap()
        .is_none());
    assert!(b
        .provider_connection_store
        .get(&connections[0].id)
        .await
        .unwrap()
        .is_none());
    let app_for = |a: AppState, b: AppState| {
        let registry = EnvironmentRegistry::new(vec![a.clone(), b]).unwrap();
        let auth = Arc::new(ClusterAuth {
            registry,
            directory: openai_server::environment_directory::StaticEnvironmentDirectory::new([
                (a_id.clone(), "team-a"),
                (b_id.clone(), "team-b"),
            ]),
            provisioner: None,
            default_state: a.clone(),
            pool: pool.clone(),
            hasher: factory.hasher.clone(),
        });
        Router::new()
            .route(
                "/v1/chat/completions",
                axum::routing::post(openai_server::handlers::chat::create_chat_completion),
            )
            .with_state(a.clone())
            .merge(build_grpc_router(a, HealthState::new()))
            .layer(axum::middleware::from_fn_with_state(
                auth,
                cluster_auth_middleware,
            ))
    };
    let client = reqwest::Client::new();
    let token_for = |team: &str, permissions: Vec<&str>| {
        chalk_jwt(
            "wire-test-secret",
            &json!({"iss":"wire-cluster", "sub":team, "exp":future_exp(), "kind":"user", "team_id":team, "team_permissions":permissions}),
        )
    };
    // Persisted runtimes survive restart, with the original opaque identities.
    let phases = if pool.is_some() { 2 } else { 1 };
    for phase in 0..phases {
        let (current_a, current_b) = if phase == 0 {
            (a.clone(), b.clone())
        } else {
            (
                factory.build(&a_id).await.unwrap().0,
                factory.build(&b_id).await.unwrap().0,
            )
        };
        let base = serve(app_for(current_a.clone(), current_b.clone())).await;
        let generate = |key: String, expected: &'static str, stream| {
            let client = client.clone();
            let base = base.clone();
            async move {
                let response = client.post(format!("{base}/v1/chat/completions"))
                    .bearer_auth(key).header("x-session-id", "identical-client-session")
                    .header("traceparent", "00-11111111111111111111111111111111-2222222222222222-01")
                    .json(&json!({"model":"same/gpt-4o", "messages":[{"role":"user","content":"hello"}], "stream":stream})).send().await.unwrap();
                let status = response.status();
                let body = response.text().await.unwrap();
                assert_eq!(status, StatusCode::OK, "{body}");
                assert!(body.contains(expected), "{body}");
                if stream {
                    assert!(body.contains("data: [DONE]"));
                }
            }
        };
        let expected_a = if phase == 0 {
            "Bearer provider-a"
        } else {
            "Bearer provider-a-rotated"
        };
        tokio::join!(
            generate(keys[0].raw_key.clone(), expected_a, false),
            generate(keys[1].raw_key.clone(), "Bearer provider-b", true)
        );
        tokio::join!(
            generate(keys[0].raw_key.clone(), expected_a, true),
            generate(keys[1].raw_key.clone(), "Bearer provider-b", false)
        );
        for (state, team, connection) in [
            (&current_a, "team-a", &connections[0]),
            (&current_b, "team-b", &connections[1]),
        ] {
            let token = token_for(team, vec!["secrets.list", "query.online"]);
            let channel = tonic::transport::Channel::from_shared(base.clone())
                .unwrap()
                .connect()
                .await
                .unwrap();
            let mut grpc = tonic::client::Grpc::new(channel);
            grpc.ready().await.unwrap();
            let mut request = tonic::Request::new(ListProviderConnectionsRequest::default());
            request
                .metadata_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
            request
                .metadata_mut()
                .insert("x-chalk-env-id", state.environment_id.parse().unwrap());
            let response: tonic::Response<ListProviderConnectionsResponse> = grpc
                .unary(
                    request,
                    http::uri::PathAndQuery::from_static(
                        "/chalk.router.v1.ProviderConnectionService/ListProviderConnections",
                    ),
                    tonic::codec::ProstCodec::default(),
                )
                .await
                .unwrap();
            let list = response.into_inner();
            assert_eq!(list.connections.len(), 1);
            assert_eq!(list.connections[0].id, connection.id);
            assert_eq!(list.connections[0].name, "same-connection");
            let response = client
                .post(format!(
                    "{base}/chalk.router.v1.ProviderConnectionService/ListProviderConnections"
                ))
                .bearer_auth(&token)
                .header("x-chalk-env-id", &state.environment_id)
                .header("content-type", "application/grpc-web+proto")
                .header("x-grpc-web", "1")
                .body(vec![0u8; 5])
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = response.bytes().await.unwrap();
            let size = u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
            let list = ListProviderConnectionsResponse::decode(&bytes[5..5 + size]).unwrap();
            assert_eq!(list.connections.len(), 1);
            assert_eq!(list.connections[0].id, connection.id);
        }
        // The same explicit admin grants apply in cluster mode, after selecting
        // an environment the token's team actually owns.
        for kind in ["user", "service-account"] {
            let token = chalk_jwt(
                "wire-test-secret",
                &json!({"iss":"wire-cluster", "sub":"reader", "exp":future_exp(),
                    "kind":kind, "team_id":"team-a", "team_permissions":["query.online"]}),
            );
            for (path, _) in openai_server::cluster_auth::ADMIN_OPERATION_PERMISSIONS {
                let response = client
                    .post(format!("{base}{path}"))
                    .bearer_auth(&token)
                    .header("x-chalk-env-id", &a_id)
                    .header("content-type", "application/grpc-web+proto")
                    .header("x-grpc-web", "1")
                    .body(vec![0u8; 5])
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::FORBIDDEN, "{kind}: {path}");
            }
        }
        let before = calls.load(std::sync::atomic::Ordering::SeqCst);
        for token in [
            keys[0].raw_key.clone(),
            token_for("team-a", vec!["query.online"]),
        ] {
            let response = client
                .post(format!("{base}/v1/chat/completions"))
                .bearer_auth(token)
                .header("x-chalk-env-id", &b_id)
                .json(
                    &json!({"model":"same/gpt-4o","messages":[{"role":"user","content":"denied"}]}),
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        let response = client
            .post(format!(
                "{base}/chalk.router.v1.ProviderConnectionService/ListProviderConnections"
            ))
            // Even an explicit operation grant cannot reach another team.
            .bearer_auth(token_for("team-a", vec!["secrets.list"]))
            .header("x-chalk-env-id", &b_id)
            .header("content-type", "application/grpc-web+proto")
            .header("x-grpc-web", "1")
            .body(vec![0u8; 5])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), before);
        current_a
            .provider_connection_store
            .update(
                &connections[0].id,
                ConnectionUpdate {
                    api_key: Some("provider-a-rotated".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        current_a.backend.invalidate_routing_cache().await;
        tokio::join!(
            generate(keys[0].raw_key.clone(), "Bearer provider-a-rotated", false),
            generate(keys[1].raw_key.clone(), "Bearer provider-b", false)
        );
        for state in [current_a, current_b] {
            if let Some(tracker) = state.usage_tracker {
                tracker.shutdown().await;
            }
        }
    }
    if let Some(pool) = pool {
        for environment in [&a_id, &b_id] {
            let counts: (i64, i64) = sqlx::query_as("SELECT count(*),sum(total_tokens)::bigint FROM chalk_router.usage_events WHERE environment_id=$1").bind(environment).fetch_one(&pool).await.unwrap();
            assert_eq!(counts, (6, 30));
        }
    }
}

#[tokio::test]
async fn cluster_http_grpc_streaming_and_provider_rotation_are_isolated_in_memory() {
    cluster_wire_conformance(None).await;
}

#[tokio::test]
async fn quota_cleanup_runs_once_for_the_shared_database_and_for_each_memory_runtime() {
    use openai_server::{cluster_lifecycle::cleanup_quotas, config::QuotaBackendKind, quota::*};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountCleanup(AtomicUsize);
    #[async_trait::async_trait]
    impl QuotaBackend for CountCleanup {
        fn enabled(&self) -> bool {
            true
        }
        async fn acquire_many(
            &self,
            _: &[QuotaAction],
            _: Option<u64>,
        ) -> Result<QuotaDecision, QuotaBackendError> {
            Ok(QuotaDecision::Allowed(None))
        }
        async fn finish(&self, _: &QuotaGrant, _: QuotaOutcome) -> Result<(), QuotaBackendError> {
            Ok(())
        }
        async fn renew(&self, _: uuid::Uuid) -> Result<(), QuotaBackendError> {
            Ok(())
        }
        async fn cleanup(&self, _: u64) -> Result<(u64, u64), QuotaBackendError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok((0, 0))
        }
    }
    let mut a = runtime("a").await;
    let mut b = runtime("b").await;
    let shared = Arc::new(CountCleanup(AtomicUsize::new(0)));
    a.quota_backend = shared.clone();
    b.quota_backend = shared.clone();
    cleanup_quotas(&[a.clone(), b.clone()], QuotaBackendKind::Postgres).await;
    assert_eq!(shared.0.load(Ordering::SeqCst), 1);
    let other = Arc::new(CountCleanup(AtomicUsize::new(0)));
    b.quota_backend = other.clone();
    cleanup_quotas(&[a, b], QuotaBackendKind::Memory).await;
    assert_eq!(shared.0.load(Ordering::SeqCst), 2);
    assert_eq!(other.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn postgres_runtime_startup_isolates_malformed_provider_metadata() {
    use openai_server::{
        config::{RouterConfig, UsageBackendKind},
        environment_runtime::RuntimeFactory,
        hashing::KeyHasher,
        provider_connections::{ConnectionInput, ExposurePolicy},
        secrets::SecretCipher,
    };
    let Some(url) = crate::support::db::test_database_url("cluster startup metadata isolation")
    else {
        return;
    };
    let _lock = crate::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let mut config = RouterConfig::from_env();
    config.usage_backend = UsageBackendKind::Postgres;
    let factory = RuntimeFactory {
        shared: runtime("default").await,
        pool: Some(pool.clone()),
        hasher: KeyHasher::new(b"startup-test-pepper".to_vec()),
        cipher: Some(
            SecretCipher::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap(),
        ),
        config,
    };
    let a_id = format!("bad-metadata-{}", uuid::Uuid::new_v4());
    let b_id = format!("good-metadata-{}", uuid::Uuid::new_v4());
    let a = factory.build(&a_id).await.unwrap().0;
    let b = factory.build(&b_id).await.unwrap().0;
    for state in [&a, &b] {
        state
            .provider_connection_store
            .create(ConnectionInput {
                name: "same-name".into(),
                provider_kind: "openai".into(),
                base_url: None,
                prefix: Some("same/".into()),
                exposure: ExposurePolicy::Dynamic,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "test-provider".into(),
            })
            .await
            .unwrap();
    }
    sqlx::query("UPDATE chalk_router.provider_connections SET declared_capabilities='\"invalid-patch\"'::jsonb WHERE environment_id=$1").bind(&a_id).execute(&pool).await.unwrap();
    let restarted_a = factory.build(&a_id).await.unwrap().0;
    let restarted_b = factory.build(&b_id).await.unwrap().0;
    assert!(restarted_a
        .provider_connection_store
        .list_metadata()
        .await
        .is_err());
    assert!(restarted_a.backend.resolve("same/gpt-4o").await.is_err());
    assert_eq!(
        restarted_b
            .provider_connection_store
            .list_metadata()
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(restarted_b.backend.resolve("same/gpt-4o").await.is_ok());
    pool.close().await;
}

#[tokio::test]
async fn postgres_cluster_http_grpc_rotation_and_restart_preserve_isolation() {
    let Some(url) = crate::support::db::test_database_url("cluster wire isolation") else {
        return;
    };
    let _lock = crate::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    cluster_wire_conformance(Some(pool.clone())).await;
    pool.close().await;
}

async fn cluster_provisioner(
    pool: Option<sqlx::PgPool>,
) -> (
    Arc<EnvironmentRegistry>,
    Arc<openai_server::environment_runtime::RuntimeProvisioner>,
) {
    use openai_server::{
        cluster_lifecycle::BackgroundWorkers,
        config::{QuotaBackendKind, RouterConfig, UsageBackendKind},
        environment_runtime::{RuntimeFactory, RuntimeProvisioner},
        hashing::KeyHasher,
        secrets::SecretCipher,
        telemetry::TelemetryRuntime,
    };
    let mut config = RouterConfig::from_env();
    config.usage_backend = UsageBackendKind::Disabled;
    config.quota_backend = QuotaBackendKind::Memory;
    let factory = RuntimeFactory {
        shared: runtime("template").await,
        cipher: pool.as_ref().map(|_| {
            SecretCipher::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()
        }),
        pool,
        config,
        hasher: KeyHasher::new(b"cluster-pepper".to_vec()),
    };
    let registry = EnvironmentRegistry::new(Vec::new()).unwrap();
    let provisioner = Arc::new(RuntimeProvisioner::new(
        factory,
        registry.clone(),
        TelemetryRuntime::new(Vec::new()),
        Arc::new(BackgroundWorkers::default()),
    ));
    (registry, provisioner)
}

#[tokio::test]
async fn cluster_mode_loads_confirmed_environments_on_first_use() {
    use crate::support::auth::{chalk_jwt, federated_registry, future_exp};
    use openai_server::{
        cluster_auth::{cluster_auth_middleware, ClusterAuth},
        hashing::KeyHasher,
    };
    let (registry, provisioner) = cluster_provisioner(None).await;

    // Concurrent first requests for one environment build a single runtime.
    let built = futures::future::join_all((0..8).map(|_| provisioner.get_or_provision("a"))).await;
    let first = built[0].as_ref().unwrap().clone();
    assert!(built
        .iter()
        .all(|state| Arc::ptr_eq(&state.as_ref().unwrap().backend, &first.backend)));
    assert_eq!(registry.len(), 1);

    let mut template = runtime("template").await;
    template.federated_auth =
        federated_registry("lazy-cluster", "LAZY_CLUSTER_SECRET", "lazy-secret");
    let auth = Arc::new(ClusterAuth {
        registry: registry.clone(),
        directory: two_team_directory(),
        provisioner: Some(provisioner),
        default_state: template.clone(),
        pool: None,
        hasher: KeyHasher::new(b"cluster-pepper".to_vec()),
    });
    let app = Router::new()
        .route("/", get(environment))
        .with_state(template)
        .layer(axum::middleware::from_fn_with_state(
            auth,
            cluster_auth_middleware,
        ));
    let token = chalk_jwt(
        "lazy-secret",
        &serde_json::json!({"iss":"lazy-cluster", "sub":"member", "exp":future_exp(), "kind":"user", "team_id":"team-b", "team_permissions":["query.online"]}),
    );
    let send = |environment: &'static str| {
        let app = app.clone();
        let token = token.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .uri("/")
                    .header("authorization", format!("Bearer {token}"))
                    .header("x-chalk-env-id", environment)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };
    let response = send("b").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "b"
    );
    assert_eq!(registry.len(), 2);
    // Neither another team's environment nor an unknown one is loaded.
    for denied in ["a", "c"] {
        assert_eq!(send(denied).await.status(), StatusCode::FORBIDDEN);
    }
    assert_eq!(registry.len(), 2);
    assert!(registry.get("c").is_none());
}

#[tokio::test]
async fn postgres_recovery_loads_confirmed_environments_with_pending_jobs() {
    use openai_server::{
        background_responses::environments_with_pending_jobs,
        environment_directory::StaticEnvironmentDirectory, environment_runtime::PendingJobRecovery,
    };
    let Some(url) = crate::support::db::test_database_url("cluster pending job recovery") else {
        return;
    };
    let _lock = crate::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let run = uuid::Uuid::new_v4();
    let served = format!("recover-served-{run}");
    let unknown = format!("recover-unknown-{run}");
    let settled = format!("recover-settled-{run}");
    for (environment, phase) in [
        (&served, "unresolved"),
        (&unknown, "submitting"),
        (&settled, "settled"),
    ] {
        // Not due for a poll, so the recovered worker leaves the row alone.
        sqlx::query("INSERT INTO chalk_router.background_response_jobs (environment_id, event_id, phase, payload, next_poll_at) VALUES ($1, $2, $3, '{}'::jsonb, now() + interval '1 day')")
            .bind(environment)
            .bind(uuid::Uuid::new_v4())
            .bind(phase)
            .execute(&pool)
            .await
            .unwrap();
    }
    let pending = environments_with_pending_jobs(&pool).await.unwrap();
    assert!(pending.contains(&served));
    assert!(pending.contains(&unknown));
    assert!(!pending.contains(&settled));

    let (registry, provisioner) = cluster_provisioner(Some(pool.clone())).await;
    let recovery = PendingJobRecovery::new(
        pool.clone(),
        StaticEnvironmentDirectory::new([(served.clone(), "team")]),
        provisioner,
        registry.clone(),
    );
    recovery.run().await;
    assert!(registry.get(&served).is_some());
    assert!(registry.get(&unknown).is_none());
    assert_eq!(registry.len(), 1);

    sqlx::query("DELETE FROM chalk_router.background_response_jobs WHERE environment_id = ANY($1)")
        .bind(vec![served, unknown, settled])
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// Confirms environments from a fixed map, except one lookup that never answers.
struct HangingDirectory {
    hangs: String,
    inner: Arc<openai_server::environment_directory::StaticEnvironmentDirectory>,
}

#[async_trait::async_trait]
impl openai_server::environment_directory::EnvironmentDirectory for HangingDirectory {
    async fn lookup(
        &self,
        environment_id: &str,
    ) -> Result<
        Option<openai_server::environment_directory::EnvironmentRecord>,
        openai_server::environment_directory::DirectoryUnavailable,
    > {
        if environment_id == self.hangs {
            std::future::pending::<()>().await;
        }
        self.inner.lookup(environment_id).await
    }
}

#[tokio::test]
async fn postgres_recovery_skips_a_hanging_lookup_and_runs_beside_maintenance() {
    use std::time::Duration;

    use openai_server::{
        cluster_lifecycle::{BackgroundWorkers, ClusterWorkers},
        config::QuotaBackendKind,
        environment_directory::StaticEnvironmentDirectory,
        environment_runtime::PendingJobRecovery,
    };
    let Some(url) = crate::support::db::test_database_url("cluster recovery with a hanging lookup")
    else {
        return;
    };
    let _lock = crate::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let run = uuid::Uuid::new_v4();
    let stalled = format!("recover-stalled-{run}");
    let healthy = format!("recover-healthy-{run}");
    for environment in [&stalled, &healthy] {
        // Not due for a poll, so the recovered worker leaves the row alone.
        sqlx::query("INSERT INTO chalk_router.background_response_jobs (environment_id, event_id, phase, payload, next_poll_at) VALUES ($1, $2, 'unresolved', '{}'::jsonb, now() + interval '1 day')")
            .bind(environment)
            .bind(uuid::Uuid::new_v4())
            .execute(&pool)
            .await
            .unwrap();
    }
    let directory = || {
        Arc::new(HangingDirectory {
            hangs: stalled.clone(),
            inner: StaticEnvironmentDirectory::new([
                (stalled.clone(), "team"),
                (healthy.clone(), "team"),
            ]),
        })
    };

    // A pass ends when the stalled environment runs out of time, and the
    // healthy environment is loaded in the meantime.
    let (registry, provisioner) = cluster_provisioner(Some(pool.clone())).await;
    let recovery =
        PendingJobRecovery::new(pool.clone(), directory(), provisioner, registry.clone())
            .with_environment_timeout(Duration::from_millis(200));
    tokio::time::timeout(Duration::from_secs(10), recovery.run())
        .await
        .expect("a hanging lookup must not hold up the recovery pass");
    assert!(registry.get(&healthy).is_some());
    assert!(registry.get(&stalled).is_none());

    // Cluster workers run recovery as their own task, starting at once: start
    // returns while a lookup still hangs, the healthy environment loads, and
    // shutdown does not wait for the hanging lookup.
    let (registry, provisioner) = cluster_provisioner(Some(pool.clone())).await;
    let recovery = Arc::new(PendingJobRecovery::new(
        pool.clone(),
        directory(),
        provisioner,
        registry.clone(),
    ));
    let workers = ClusterWorkers::start(
        registry.clone(),
        Arc::new(BackgroundWorkers::default()),
        QuotaBackendKind::Disabled,
        Some(recovery),
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while registry.get(&healthy).is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("recovery must load the healthy environment while another lookup hangs");
    assert!(registry.get(&stalled).is_none());
    tokio::time::timeout(Duration::from_secs(10), workers.shutdown())
        .await
        .expect("shutdown must not wait for a hanging recovery lookup");

    sqlx::query("DELETE FROM chalk_router.background_response_jobs WHERE environment_id = ANY($1)")
        .bind(vec![stalled, healthy])
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}
