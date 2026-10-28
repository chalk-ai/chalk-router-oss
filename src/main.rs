use axum::http::{HeaderName, HeaderValue};
use axum::{
    extract::DefaultBodyLimit,
    http::{Method, StatusCode},
    middleware,
    routing::{delete, get, post},
    Json, Router,
};
use openai_server::config::UsageBackendKind;
use openai_server::{
    api_keys::{ApiKeyStore, InMemoryApiKeyStore, PostgresApiKeyStore},
    backend::{BackendProvider, RouterBackend},
    grpc::{build_grpc_router, HealthState},
    handlers,
    middleware::auth_middleware,
    model_routes::{InMemoryModelRouteStore, ModelRouteStore, PostgresModelRouteStore},
    provider_connections::{
        InMemoryProviderConnectionStore, PostgresProviderConnectionStore, ProviderConnectionStore,
        StoreError,
    },
    sampling_policy::{
        InMemorySamplingPolicyStore, PostgresSamplingPolicyStore, SamplingPolicy,
        SamplingPolicyStore,
    },
    telemetry,
    telemetry_settings::{
        default_telemetry_exporters, InMemoryTelemetryExporterStore,
        PostgresTelemetryExporterStore, TelemetrySettingsService, TelemetrySettingsStore,
    },
    usage_pools::{InMemoryUsagePoolStore, PostgresUsagePoolStore, UsagePoolStore},
    usage_tracker::UsageTracker,
    AppState, DebugLog,
};
use serde::Serialize;
use std::sync::Arc;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

#[tokio::main]
async fn main() {
    // Load .env first so telemetry config (sampler, endpoint) and the
    // content-logging toggle all see the same environment.
    dotenvy::dotenv().ok();

    // `openai-server migrate` runs migrations and exits — before building any of
    // the serve stack (no telemetry, stores, hasher, or ROUTER_KEY_PEPPER).
    if std::env::args().nth(1).as_deref() == Some("migrate") {
        let db_url = std::env::var("ROUTER_DATABASE_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let Some(db_url) = db_url else {
            eprintln!("migrate: set ROUTER_DATABASE_URL");
            std::process::exit(1);
        };
        // Migrations are environment-agnostic and run only through the
        // deployment workflow; see `check_migrate_environment` for the tripwire.
        if let Err(error) = openai_server::config::check_migrate_environment() {
            eprintln!("migrate: {error}");
            std::process::exit(1);
        }
        // The serve stack's telemetry is never initialized on this path, so
        // install the plain logging layer or the runner's lease and
        // history-adoption logs are dropped before they reach the Job's output.
        telemetry::init_logging_only();
        match openai_server::migrate::run(&db_url).await {
            Ok(report) => println!(
                "migrate: router schema up to date ({} applied, {} already present)",
                report.applied, report.already_applied
            ),
            Err(e) => {
                eprintln!("migrate: failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // `openai-server seed-connections [--file <path>]` inserts any provider
    // connections + model routes from a JSON seed file that are not already
    // present (insert-if-absent — it never updates an existing row), then exits.
    // Like `migrate`, this runs before any serve stack is built. Requires a
    // database (ROUTER_DATABASE_URL) and the KEK (ROUTER_SECRETS_KEK) to encrypt keys.
    if std::env::args().nth(1).as_deref() == Some("seed-connections") {
        let file = seed_file_arg();
        match openai_server::seed::run(file.as_deref()).await {
            Ok(summary) => println!("seed-connections: {summary}"),
            Err(e) => {
                eprintln!("seed-connections: failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // `openai-server seed-usage` adds a small, idempotent local fixture for
    // manually exercising the Token Usage explorer. It runs before the serve
    // stack and creates only disabled/unpublished demo routing resources.
    if std::env::args().nth(1).as_deref() == Some("seed-usage") {
        match openai_server::usage_seed::run().await {
            Ok(summary) => println!("seed-usage: {summary}"),
            Err(e) => {
                eprintln!("seed-usage: failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let config = openai_server::config::RouterConfig::from_env();

    let object_store = match &config.object_store_uri {
        Some(uri) => {
            let store = openai_server::storage::ObjectStore::from_uri(uri)
                .expect("Failed to initialize object store from ROUTER_OBJECT_STORE_URI");
            Some(std::sync::Arc::new(store))
        }
        None => None,
    };

    let telemetry_runtime = telemetry::init_telemetry(object_store.clone(), &config)
        .expect("Failed to initialize OpenTelemetry");

    if config.dangerously_permit_unauthenticated {
        tracing::warn!(
            "ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED is enabled: proxy requests without credentials are allowed. Use only for local development. Admin routes still require authentication."
        );
    }

    // Resolve trace content-logging once, after .env is loaded.
    handlers::init_content_logging(config.trace_log_content);

    // The Anthropic Messages API pass-through (`/v1/messages`) resolves its key
    // from the stored `anthropic` provider connection at request time.

    // Key hasher. On the Postgres path ROUTER_KEY_PEPPER is required (fail fast);
    // the in-memory dev path falls back to a fixed, insecure dev pepper.
    let hasher = match openai_server::hashing::KeyHasher::from_env() {
        Ok(h) => h,
        Err(e) if config.database_url.is_none() => {
            tracing::warn!("{e}; using an insecure dev pepper (in-memory mode)");
            openai_server::hashing::KeyHasher::new(b"dev-insecure-pepper".to_vec())
        }
        Err(e) => panic!("{e}"),
    };
    telemetry_runtime.set_sampling_secret(hasher.sampling_secret());

    // Tenancy scope for every store query (the DB may be shared across chalk
    // environments; this deployment serves exactly one).
    let env_id = config.environment_id.clone();
    tracing::info!(environment_id = %env_id, "Router serving chalk environment");

    // Open the core pool once (if configured) so the stores and the
    // The usage ledger, definition stores, and quota runtime can share it.
    let pool: Option<sqlx::PgPool> = match config.database_url.as_deref() {
        Some(database_url) => {
            tracing::info!(
                "Connecting to PostgreSQL for persistent storage (migrations must have run)"
            );
            // Runtime queries are `chalk_router.`-qualified. Do not set session
            // state here: transaction-pooling proxies can reuse the underlying
            // server connection for another client after a transaction.
            let connect_options = openai_server::database::connect_options(database_url)
                .unwrap_or_else(|error| {
                    panic!("Invalid PostgreSQL connection configuration: {error}")
                });
            Some(
                openai_server::database::serving_pool_options(
                    config.database_max_connections,
                    config.database_acquire_timeout,
                    config.database_health_check_idle,
                )
                .connect_with(connect_options)
                .await
                .expect("Failed to connect to PostgreSQL"),
            )
        }
        None => {
            tracing::info!("Using in-memory storage (set ROUTER_DATABASE_URL for persistence)");
            None
        }
    };

    #[allow(clippy::type_complexity)]
    let (
        api_key_store,
        sampling_policy_store,
        usage_pool_store,
        provider_connection_store,
        model_route_store,
        telemetry_settings_store,
    ): (
        Arc<dyn ApiKeyStore>,
        Arc<dyn SamplingPolicyStore>,
        Arc<dyn UsagePoolStore>,
        Arc<dyn ProviderConnectionStore>,
        Arc<dyn ModelRouteStore>,
        Arc<dyn TelemetrySettingsStore>,
    ) = match &pool {
        Some(pool) => {
            // Required whenever a DB is configured: provider connection keys are
            // encrypted at rest under this KEK. Fail fast (like ROUTER_KEY_PEPPER)
            // rather than persist keys we can't protect.
            let cipher =
                openai_server::secrets::SecretCipher::from_env().unwrap_or_else(|e| panic!("{e}"));
            (
                Arc::new(PostgresApiKeyStore::new(
                    pool.clone(),
                    hasher.clone(),
                    env_id.clone(),
                )),
                // Uncached: the read that has to be cheap is the synchronous
                // one inside the span processor, which keeps its own snapshot
                // (see `telemetry::sampling::SamplingGate`).
                Arc::new(PostgresSamplingPolicyStore::new(
                    pool.clone(),
                    env_id.clone(),
                )),
                Arc::new(PostgresUsagePoolStore::new(pool.clone(), env_id.clone())),
                Arc::new(PostgresProviderConnectionStore::new(
                    pool.clone(),
                    cipher.clone(),
                    env_id.clone(),
                )),
                Arc::new(PostgresModelRouteStore::new(pool.clone(), env_id.clone())),
                Arc::new(PostgresTelemetryExporterStore::new(
                    pool.clone(),
                    env_id.clone(),
                    cipher,
                    default_telemetry_exporters(),
                )),
            )
        }
        None => {
            // Built first so the connection store can own its own
            // route-reference check on delete (see
            // `InMemoryProviderConnectionStore::with_routes`), instead of the
            // service layer pre-scanning routes under a mutex.
            let model_route_store: Arc<dyn ModelRouteStore> =
                Arc::new(InMemoryModelRouteStore::new());
            (
                Arc::new(InMemoryApiKeyStore::new(hasher.clone())),
                Arc::new(InMemorySamplingPolicyStore::new(SamplingPolicy::default())),
                Arc::new(InMemoryUsagePoolStore::new()),
                Arc::new(InMemoryProviderConnectionStore::with_routes(
                    model_route_store.clone(),
                )),
                model_route_store,
                Arc::new(InMemoryTelemetryExporterStore::new(
                    default_telemetry_exporters(),
                )),
            )
        }
    };

    // Capability declarations are operator-authored routing inputs. Validate
    // every non-null document before serving so an old or malformed pre-rollout
    // shape cannot be silently treated as absent.
    enforce_capability_declaration_preflight(
        provider_connection_store.list_metadata().await.map(|_| ()),
    );

    // The sampling gate was built at `init_telemetry` time, before any store
    // existed; hand it the policy now that one does. Spans that ended in the
    // window between the two were all kept.
    telemetry_runtime.set_sampling_policy_store(sampling_policy_store.clone());
    telemetry_runtime.prime_sampling_policy().await;

    // Taken before the runtime handle is moved into the settings service, and
    // handed to the span-making layer far below: the request path resolves each
    // frontend trace's keep decision against this same gate.
    let sampling_gate = telemetry_runtime.sampling_gate();

    let cluster_telemetry = telemetry_runtime.clone();
    let telemetry_settings = Arc::new(TelemetrySettingsService::new(
        telemetry_settings_store,
        telemetry_runtime,
    ));
    if let Err(error) = telemetry_settings.refresh().await {
        tracing::warn!(%error, "Initial telemetry settings unavailable; refresh will retry");
    }

    // Usage accounting remains independent from quota enforcement. Budget and
    // rate-limit definitions follow the core store, while ROUTER_QUOTA_BACKEND
    // selects only their shared mutable admission state.
    let usage_tracker: Option<UsageTracker> = match config.usage_backend {
        UsageBackendKind::Disabled => None,
        UsageBackendKind::Postgres => match &pool {
            Some(pool) => match UsageTracker::new(pool.clone(), env_id.clone()).await {
                Ok(tracker) => {
                    tracing::info!("Usage tracking enabled (postgres backend)");
                    Some(tracker)
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to start usage tracker; usage tracking disabled");
                    None
                }
            },
            None => {
                tracing::warn!("ROUTER_USAGE_BACKEND=postgres but no database is configured; usage tracking disabled");
                None
            }
        },
        UsageBackendKind::Clickhouse => {
            tracing::warn!("ROUTER_USAGE_BACKEND is not implemented (only 'postgres'); usage tracking disabled");
            None
        }
    };
    let budget_store = openai_server::budget::from_core_store(pool.clone(), env_id.clone());
    let rate_limit_store = openai_server::ratelimit::from_core_store(pool.clone(), env_id.clone());
    let quota_backend = openai_server::quota::from_kind(config.quota_backend, pool.clone());
    let quota_resolver = Arc::new(openai_server::quota_policy::QuotaPolicyResolver::new(
        env_id.clone(),
        budget_store.clone(),
        rate_limit_store.clone(),
    ));

    // Exact-prefix completion cache. Always constructed (a disabled backend is
    // a real object), so the request path checks the key's opt-in rather than
    // whether a cache exists. Backends never fail the boot: an unusable one
    // degrades to serving uncached.
    let prompt_cache = openai_server::prompt_cache::from_config(&config.prompt_cache);
    let prompt_cache_config = Arc::new(config.prompt_cache.clone());

    // Construct the outbound client once. Every backend and administrative
    // probe receives a cheap clone backed by the same connection pool.
    let http_client = openai_server::upstream_client(config.upstream_connect_timeout);
    let backend: Arc<dyn BackendProvider> = Arc::new(RouterBackend::new(
        http_client.clone(),
        provider_connection_store.clone(),
        model_route_store.clone(),
    ));

    if let Some(uri) = &config.object_store_uri {
        tracing::info!(uri = %uri, "Object store initialized");
    } else {
        tracing::info!("No ROUTER_OBJECT_STORE_URI set, file uploads will be disabled");
    }

    // Federated Chalk JWT auth: RS256 JWTs minted by the Chalk metadata API
    // servers listed in CHALK_JWT_ISSUERS are accepted on both the admin
    // surface (as an alternative to ROUTER_ADMIN_KEY) and the proxied surface (as an
    // alternative to a stored API key). Each comma-separated entry is either a
    // discovery URL (`https://api.chalk.ai`) or `iss=jwks_uri` for a bare-string
    // issuer like `be` (`be=https://issuer.example.invalid/.well-known/jwks.json`).
    // Signing keys are fetched from each issuer's JWKS, so no secret is shared
    // with the API server. CHALK_JWT_AUDIENCE must pin `aud`.
    let federated_auth = {
        let issuers = config
            .federated_issuer_configs()
            .expect("Invalid federated JWT authorization configuration");
        if !issuers.is_empty() {
            let names: Vec<&str> = issuers.iter().map(|i| i.issuer.as_str()).collect();
            tracing::info!(issuers = ?names, "Chalk JWT federation enabled");
        }
        std::sync::Arc::new(
            chalk_federated_auth::OidcRegistry::from_config(&issuers)
                .expect("Invalid CHALK_JWT_ISSUERS configuration"),
        )
    };

    // Auth config is resolved at boot rather than re-read on every request.
    let admin_key = config.admin_key.clone();
    let accept_cross_environment_tokens = config.accept_cross_environment_tokens;

    let state = AppState {
        route_judge: Default::default(),
        backend,
        http_client,
        debug_log: DebugLog::new(),
        environment_id: env_id.clone(),
        response_locator: hasher.response_locator_codec(),
        api_key_store,
        usage_pool_store,
        sampling_policy: sampling_policy_store,
        sampling_gate: sampling_gate.clone(),
        usage_tracker,
        background_responses: Arc::new(
            openai_server::background_responses::BackgroundResponses::new(
                pool.clone(),
                env_id.clone(),
                config.background_responses,
            ),
        ),
        budget_store,
        rate_limit_store,
        quota_resolver,
        quota_backend,
        prompt_cache,
        prompt_cache_config,
        object_store,
        provider_connection_store,
        model_route_store,
        telemetry_settings,
        federated_auth,
        federated_admin_team_id: config.federated_admin_team_id.clone(),
        federated_jwt_audience: config.chalk_jwt_audience.clone(),
        admin_key,
        dangerously_permit_unauthenticated: config.dangerously_permit_unauthenticated,
        accept_cross_environment_tokens,
        connection_policy: openai_server::provider_connections::ConnectionPolicy::for_environment(
            &config, &env_id,
        ),
    };

    let background_workers =
        Arc::new(openai_server::cluster_lifecycle::BackgroundWorkers::default());
    let (state, cluster_auth, registry, recovery) = if let Some(cluster) = &config.cluster {
        // Cluster mode: environments are confirmed with the metadata API and
        // loaded on first use. Only the legacy environment is loaded here;
        // environments with pending background jobs are resumed by the
        // recovery task `ClusterWorkers` starts, without delaying startup.
        let factory = openai_server::environment_runtime::RuntimeFactory {
            shared: state.clone(),
            pool: pool.clone(),
            hasher: hasher.clone(),
            cipher: pool
                .as_ref()
                .map(|_| openai_server::secrets::SecretCipher::from_env().expect("cluster KEK")),
            config: config.clone(),
        };
        let registry = openai_server::tenancy::EnvironmentRegistry::new(Vec::new())
            .expect("empty environment registry");
        let provisioner = Arc::new(openai_server::environment_runtime::RuntimeProvisioner::new(
            factory,
            registry.clone(),
            cluster_telemetry,
            background_workers.clone(),
        ));
        let directory: Arc<dyn openai_server::environment_directory::EnvironmentDirectory> =
            Arc::new(
                openai_server::metadata_client::MetadataEnvironmentDirectory::connect(cluster)
                    .await
                    .unwrap_or_else(|error| {
                        panic!("Cannot configure the metadata API client: {error}")
                    }),
            );
        // The legacy environment comes from trusted deployment configuration,
        // so it is loaded without a directory lookup.
        let default_state = match &config.legacy_environment_id {
            Some(environment) => provisioner
                .get_or_provision(environment)
                .await
                .unwrap_or_else(|error| {
                    panic!("Cannot initialize environment {environment}: {error}")
                }),
            None => state,
        };
        let recovery = pool.clone().map(|pool| {
            Arc::new(openai_server::environment_runtime::PendingJobRecovery::new(
                pool,
                directory.clone(),
                provisioner.clone(),
                registry.clone(),
            ))
        });
        let auth = Arc::new(openai_server::cluster_auth::ClusterAuth {
            registry: registry.clone(),
            directory,
            provisioner: Some(provisioner),
            default_state: default_state.clone(),
            pool: pool.clone(),
            hasher: hasher.clone(),
        });
        (default_state, Some(auth), registry, recovery)
    } else {
        let registry = openai_server::tenancy::EnvironmentRegistry::new(vec![state.clone()])
            .expect("single environment registry");
        (state, None, registry, None)
    };

    let app = Router::new()
        // Anthropic Messages API pass-through
        .route("/v1/messages", post(handlers::anthropic::proxy_messages))
        .route(
            "/v1/messages/count_tokens",
            post(handlers::anthropic::proxy_messages_count_tokens),
        )
        // Gemini Interactions API passthrough
        .route(
            "/v1beta/interactions",
            post(handlers::gemini::create_interaction),
        )
        .route(
            "/v1beta/interactions/:interaction_id",
            get(handlers::gemini::unsupported_interaction_operation)
                .delete(handlers::gemini::unsupported_interaction_operation),
        )
        .route(
            "/v1beta/interactions/:interaction_id/cancel",
            post(handlers::gemini::unsupported_interaction_operation),
        )
        // Chat endpoints (OpenAI-compatible)
        .route(
            "/v1/chat/completions",
            post(handlers::chat::create_chat_completion),
        )
        // Router Decisions, OpenAI's Decisions API, and the TypeSafe-compatible
        // System One surface, all served by any configured decision model.
        .route(
            "/v1/alpha/decisions",
            post(handlers::decisions::create_decision),
        )
        .route(
            "/v1/decisions",
            post(handlers::decisions::create_openai_decision),
        )
        .route(
            "/v1/systemone",
            post(handlers::decisions::create_system_one),
        )
        // Responses endpoints (OpenAI-compatible, including Codex compaction)
        .route("/v1/responses", post(handlers::responses::create_response))
        .route(
            "/v1/responses/compact",
            post(handlers::responses::compact_response),
        )
        .route(
            "/v1/responses/:response_id",
            get(handlers::responses::retrieve_response)
                .delete(handlers::responses::delete_response),
        )
        .route(
            "/v1/responses/:response_id/cancel",
            post(handlers::responses::cancel_response),
        )
        // Client span ingestion (session / turn / reasoning / tool spans from
        // agent-side tracers like the opencode plugin)
        .route("/v1/traces/spans", post(handlers::traces::ingest_spans))
        // Assistant endpoints
        .route("/v1/assistants", get(handlers::assistants::list_assistants))
        .route(
            "/v1/assistants",
            post(handlers::assistants::create_assistant),
        )
        .route(
            "/v1/assistants/:assistant_id",
            get(handlers::assistants::get_assistant),
        )
        .route(
            "/v1/assistants/:assistant_id",
            post(handlers::assistants::modify_assistant),
        )
        .route(
            "/v1/assistants/:assistant_id",
            delete(handlers::assistants::delete_assistant),
        )
        // Model endpoints
        .route("/v1/models", get(handlers::models::list_models))
        .route(
            "/v1/models/projections/vscode",
            get(handlers::models::project_models_for_vscode),
        )
        .route("/v1/models/:model_id", get(handlers::models::get_model))
        .route(
            "/v1/models/:model_id",
            delete(handlers::models::delete_model),
        )
        // Embedding endpoints
        .route(
            "/v1/embeddings",
            post(handlers::embeddings::create_embedding),
        )
        // Image endpoints
        .route(
            "/v1/images/generations",
            post(handlers::images::create_image),
        )
        .route(
            "/v1/images/edits",
            post(handlers::images::create_image_edit),
        )
        .route(
            "/v1/images/variations",
            post(handlers::images::create_image_variation),
        )
        // Audio endpoints
        .route("/v1/audio/speech", post(handlers::audio::create_speech))
        .route(
            "/v1/audio/transcriptions",
            post(handlers::audio::create_transcription),
        )
        .route(
            "/v1/audio/translations",
            post(handlers::audio::create_translation),
        )
        // File endpoints
        .route("/v1/files", get(handlers::files::list_files))
        .route("/v1/files", post(handlers::files::upload_file))
        .route("/v1/files/:file_id", get(handlers::files::get_file))
        .route("/v1/files/:file_id", delete(handlers::files::delete_file))
        .route(
            "/v1/files/:file_id/content",
            get(handlers::files::download_file),
        )
        // Fine-tuning endpoints
        .route(
            "/v1/fine_tuning/jobs",
            post(handlers::fine_tuning::create_fine_tuning_job),
        )
        .route(
            "/v1/fine_tuning/jobs",
            get(handlers::fine_tuning::list_fine_tuning_jobs),
        )
        .route(
            "/v1/fine_tuning/jobs/:job_id",
            get(handlers::fine_tuning::get_fine_tuning_job),
        )
        .route(
            "/v1/fine_tuning/jobs/:job_id/cancel",
            post(handlers::fine_tuning::cancel_fine_tuning_job),
        )
        // Moderation endpoints
        .route(
            "/v1/moderations",
            post(handlers::moderations::create_moderation),
        )
        // Batch endpoints
        .route("/v1/batches", post(handlers::batch::create_batch))
        .route("/v1/batches", get(handlers::batch::list_batches))
        .route("/v1/batches/:batch_id", get(handlers::batch::get_batch))
        .route(
            "/v1/batches/:batch_id/cancel",
            post(handlers::batch::cancel_batch),
        )
        // Health check
        .route("/health", get(health_check))
        .route("/v1/healthcheck", get(v1_healthcheck))
        .with_state(state.clone());

    // gRPC router (Connect / gRPC-Web). Mounted at the proto service paths
    // (e.g. /chalk.router.v1.ApiKeyService/CreateKey). Browser clients reach
    // these via `@connectrpc/connect-web`'s `createGrpcWebTransport`.
    let health_state = HealthState::new();
    let grpc = build_grpc_router(state.clone(), health_state.clone());

    // Layers run outermost-first, and `.layer` adds each one *outside* the
    // previous — so this list runs bottom-up: CORS, then auth, then the
    // per-request span. Auth has to precede span creation because
    // `OtelMakeSpan` reads the resolved caller out of the request extensions
    // to stamp the identity and to resolve the trace's keep decision against
    // the caller's per-key rate. CORS stays outermost so a browser preflight,
    // which carries no credentials, is answered before auth can reject it.
    //
    // A consequence worth knowing: a request rejected by auth no longer
    // produces an `http_request` span, because it never reaches the layer that
    // makes one. Rejections are still logged by the middleware itself.
    let app = app
        .merge(grpc)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(telemetry::OtelMakeSpan::new(sampling_gate.clone()))
                // An errored response keeps its trace whatever rate matched.
                .on_response(telemetry::OtelOnResponse::new(sampling_gate)),
        )
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024));
    let app = match cluster_auth {
        Some(auth) => app.layer(middleware::from_fn_with_state(
            auth,
            openai_server::cluster_auth::cluster_auth_middleware,
        )),
        None => app.layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        )),
    };
    let app = app.layer(
        CorsLayer::new()
            .allow_origin([
                "http://localhost:4002".parse::<HeaderValue>().unwrap(),
                "http://localhost:3000".parse::<HeaderValue>().unwrap(),
                "http://localhost:4000".parse::<HeaderValue>().unwrap(),
                "https://staging.chalk.dev".parse::<HeaderValue>().unwrap(),
                "https://chalk.ai".parse::<HeaderValue>().unwrap(),
            ])
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::DELETE,
                Method::PATCH,
                Method::PUT,
                Method::OPTIONS,
            ])
            .allow_headers([
                HeaderName::from_static("authorization"),
                HeaderName::from_static("content-type"),
                HeaderName::from_static("x-api-key"),
                HeaderName::from_static("x-goog-api-key"),
                HeaderName::from_static("x-goog-api-client"),
                HeaderName::from_static("x-chalk-client-id"),
                HeaderName::from_static("x-chalk-env-id"),
                HeaderName::from_static("x-chalk-deployment-tag"),
                HeaderName::from_static("x-chalk-request-tag"),
                HeaderName::from_static("x-chalk-force-trace"),
                HeaderName::from_static("anthropic-version"),
                HeaderName::from_static("anthropic-beta"),
                HeaderName::from_static("openai-organization"),
                HeaderName::from_static("openai-project"),
                HeaderName::from_static("openai-beta"),
                HeaderName::from_static("x-client-request-id"),
                HeaderName::from_static("x-codex-installation-id"),
                HeaderName::from_static("x-codex-turn-state"),
                HeaderName::from_static("x-oai-attestation"),
                HeaderName::from_static("x-responsesapi-include-timing-metrics"),
                HeaderName::from_static("originator"),
                HeaderName::from_static("x-openai-product-sku"),
                HeaderName::from_static("traceparent"),
                HeaderName::from_static("tracestate"),
                // Session tagging. `baggage` rides beside traceparent; the rest are the
                // per-harness spellings telemetry::with_agent_session_id falls back to. A
                // browser caller that cannot get these through preflight is one whose spans
                // carry no session.
                HeaderName::from_static("baggage"),
                HeaderName::from_static(telemetry::AGENT_SESSION_ID_HEADER),
                HeaderName::from_static("x-claude-code-session-id"),
                HeaderName::from_static("x-session-id"),
                HeaderName::from_static("session-id"),
                HeaderName::from_static("thread-id"),
                HeaderName::from_static("accept"),
                // gRPC-Web / Connect-Web
                HeaderName::from_static("x-grpc-web"),
                HeaderName::from_static("x-user-agent"),
                HeaderName::from_static("connect-protocol-version"),
                HeaderName::from_static("connect-timeout-ms"),
                HeaderName::from_static("grpc-timeout"),
            ])
            .expose_headers([
                HeaderName::from_static("grpc-status"),
                HeaderName::from_static("grpc-message"),
                HeaderName::from_static("grpc-status-details-bin"),
                HeaderName::from_static("x-request-id"),
                HeaderName::from_static("retry-after"),
                HeaderName::from_static("x-ratelimit-limit-requests"),
                HeaderName::from_static("x-ratelimit-limit-tokens"),
                HeaderName::from_static("x-ratelimit-remaining-requests"),
                HeaderName::from_static("x-ratelimit-remaining-tokens"),
                HeaderName::from_static("x-ratelimit-reset-requests"),
                HeaderName::from_static("x-ratelimit-reset-tokens"),
                HeaderName::from_static("x-chalk-routed-model"),
                HeaderName::from_static("x-fallback-model"),
                HeaderName::from_static("x-trace-id"),
                HeaderName::from_static("x-span-id"),
            ])
            .allow_credentials(true),
    );

    let listener = tokio::net::TcpListener::bind(config.listen_addr)
        .await
        .unwrap();
    let addr = listener.local_addr().expect("bound listener address");

    // All required local dependencies and routes are initialized, and the h2c
    // listener can now accept RPCs. Startup is monotonic; readiness is cleared
    // below before graceful shutdown begins.
    health_state.set_ready(true);
    health_state.mark_startup_complete();

    tracing::info!("OpenAI API server listening on {}", addr);

    let workers = openai_server::cluster_lifecycle::ClusterWorkers::start(
        registry,
        background_workers,
        config.quota_backend,
        recovery,
    );
    axum::serve(listener, app)
        // Flush small SSE frames immediately instead of waiting for TCP ACKs.
        .tcp_nodelay(true)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            health_state.set_ready(false);
        })
        .await
        .unwrap();

    tracing::info!("HTTP requests drained; waiting for detached usage writes");
    workers.shutdown().await;

    telemetry::shutdown_telemetry();
}

/// Waits for either Ctrl-C or, on Unix, SIGTERM — the signal Kubernetes sends
/// on pod termination. Without the SIGTERM arm, `with_graceful_shutdown` never
/// fires under Kubernetes: readiness stays `true` and in-flight requests are
/// never drained before the process is killed.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
    }
}

/// The `--file <path>` (or `--file=<path>`) argument for `seed-connections`, if
/// present. When absent the seeder falls back to `ROUTER_CONNECTIONS_SEED_FILE`.
fn seed_file_arg() -> Option<String> {
    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        if arg == "--file" {
            return args.next();
        }
        if let Some(path) = arg.strip_prefix("--file=") {
            return Some(path.to_string());
        }
    }
    None
}

async fn health_check() -> (StatusCode, &'static str) {
    (StatusCode::OK, "OK")
}

fn enforce_capability_declaration_preflight(result: Result<(), StoreError>) {
    match result {
        Ok(()) => {}
        Err(error @ StoreError::InvalidCapabilityDeclaration { .. }) => {
            panic!("Provider connection preflight failed: {error}")
        }
        Err(error) => tracing::warn!(
            error = %error,
            "Could not preflight provider capability declarations; continuing startup"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_preflight_panics_only_for_invalid_declarations() {
        enforce_capability_declaration_preflight(Err(StoreError::Db("temporary".to_string())));

        let invalid = std::panic::catch_unwind(|| {
            enforce_capability_declaration_preflight(Err(
                StoreError::InvalidCapabilityDeclaration {
                    id: "connection-id".to_string(),
                    name: "provider".to_string(),
                    error: "old schema".to_string(),
                },
            ));
        });
        assert!(invalid.is_err());
    }
}

#[derive(Serialize)]
struct HealthCheckResponse {
    profile: String,
    status: String,
}

async fn v1_healthcheck() -> Json<HealthCheckResponse> {
    Json(HealthCheckResponse {
        profile: "chalk-router".to_string(),
        status: "ok".to_string(),
    })
}
