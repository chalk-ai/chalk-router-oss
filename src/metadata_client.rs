//! Cluster-mode client for the Chalk metadata API.
//!
//! Confirms environments the way the dataplane API server does: one
//! `TeamService.GetEnvIncludingArchived` call per environment ID, cached,
//! authenticated with a short-lived working token that a long-lived exchange
//! token (`ROUTER_METADATA_API_TOKEN`) is traded for. Environment-mode routers
//! never construct this client.
use std::{
    sync::{Arc, PoisonError, RwLock},
    time::{Duration, SystemTime},
};

use tonic::{
    metadata::{Ascii, MetadataValue},
    service::{interceptor::InterceptedService, Interceptor},
    transport::{Channel, ClientTlsConfig, Endpoint},
    Code, Request, Status,
};

use crate::{
    config::ClusterConfig,
    environment_directory::{DirectoryUnavailable, EnvironmentDirectory, EnvironmentRecord},
};

#[allow(clippy::all, clippy::pedantic)]
pub mod proto {
    pub mod chalk {
        pub mod auth {
            pub mod v1 {
                tonic::include_proto!("chalk.auth.v1");
            }
        }
        pub mod server {
            pub mod v1 {
                tonic::include_proto!("chalk.server.v1");
            }
        }
    }
}

use proto::chalk::{
    auth::v1::Permission,
    server::v1::{
        auth_service_client::AuthServiceClient, team_service_client::TeamServiceClient,
        GetEnvIncludingArchivedRequest, GetInternalWorkingTokenRequest,
    },
};

/// Per-RPC deadline for metadata calls.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a confirmed environment stays cached. Archival or a team change
/// takes effect within this window.
const FOUND_TTL: Duration = Duration::from_secs(600);
/// How long "no such environment" stays cached, so a newly created environment
/// becomes servable quickly.
const ABSENT_TTL: Duration = Duration::from_secs(30);
const CACHE_CAPACITY: u64 = 10_000;
/// Refresh the working token this long before it expires.
const REFRESH_MARGIN: Duration = Duration::from_secs(60);
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const RETRY_INITIAL: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// Routes each call to the metadata API server.
const SERVER_HEADER: (&str, &str) = ("x-chalk-server", "go-api");
/// Selects the environment `GetEnvIncludingArchived` returns.
const ENVIRONMENT_HEADER: &str = "x-chalk-env-id";

/// Sends a fixed bearer token: the exchange token on token refreshes.
#[derive(Clone)]
struct Bearer(MetadataValue<Ascii>);

impl Interceptor for Bearer {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert("authorization", self.0.clone());
        request
            .metadata_mut()
            .insert(SERVER_HEADER.0, MetadataValue::from_static(SERVER_HEADER.1));
        Ok(request)
    }
}

/// Sends the current working token; unauthenticated until the first refresh.
#[derive(Clone, Default)]
pub struct WorkingToken(Arc<RwLock<Option<MetadataValue<Ascii>>>>);

impl Interceptor for WorkingToken {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let header = self
            .0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| Status::unauthenticated("metadata working token not yet available"))?;
        request.metadata_mut().insert("authorization", header);
        request
            .metadata_mut()
            .insert(SERVER_HEADER.0, MetadataValue::from_static(SERVER_HEADER.1));
        Ok(request)
    }
}

fn endpoint(url: &str) -> Result<Endpoint, String> {
    let endpoint = Endpoint::from_shared(url.to_owned())
        .map_err(|error| format!("invalid ROUTER_METADATA_API_ENDPOINT: {error}"))?
        .timeout(RPC_TIMEOUT)
        .connect_timeout(RPC_TIMEOUT);
    if url.starts_with("https://") {
        endpoint
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(|error| format!("metadata API TLS: {error}"))
    } else {
        Ok(endpoint)
    }
}

/// Fetch one working token; returns the bearer header and how long until it
/// should be refreshed.
async fn fetch_working_token(
    channel: Channel,
    exchange: Bearer,
) -> Result<(MetadataValue<Ascii>, Duration), String> {
    let response = AuthServiceClient::with_interceptor(channel, exchange)
        .get_internal_working_token(GetInternalWorkingTokenRequest {
            // DEPLOY_READ authorizes TeamService.GetEnvIncludingArchived.
            requested_permissions: vec![Permission::DeployRead as i32],
        })
        .await
        .map_err(|status| format!("GetInternalWorkingToken: {status}"))?
        .into_inner();
    let expires_at = response
        .expires_at
        .and_then(|at| SystemTime::try_from(at).ok())
        .ok_or("GetInternalWorkingToken returned no expiry")?;
    let lifetime = expires_at
        .duration_since(SystemTime::now())
        .map_err(|_| "GetInternalWorkingToken returned an expired token")?;
    if response.access_token.is_empty() {
        return Err("GetInternalWorkingToken returned an empty token".into());
    }
    let header = format!("Bearer {}", response.access_token)
        .parse()
        .map_err(|_| "working token contains invalid header characters")?;
    Ok((
        header,
        lifetime
            .saturating_sub(REFRESH_MARGIN)
            .max(MIN_REFRESH_INTERVAL),
    ))
}

/// Confirms environments with the metadata API.
pub struct MetadataEnvironmentDirectory {
    teams: TeamServiceClient<InterceptedService<Channel, WorkingToken>>,
    kube_cluster_id: Option<String>,
    cache: moka::future::Cache<String, Option<EnvironmentRecord>>,
    refresher: tokio::task::JoinHandle<()>,
}

struct EntryExpiry;

impl moka::Expiry<String, Option<EnvironmentRecord>> for EntryExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Option<EnvironmentRecord>,
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        Some(if value.is_some() {
            FOUND_TTL
        } else {
            ABSENT_TTL
        })
    }
}

impl MetadataEnvironmentDirectory {
    /// Build the client and fetch the first working token. A metadata outage
    /// at startup is not fatal: lookups report unavailable until the
    /// background refresh succeeds.
    pub async fn connect(config: &ClusterConfig) -> Result<Self, String> {
        let channel = endpoint(&config.metadata_api_endpoint)?.connect_lazy();
        let exchange = Bearer(
            format!("Bearer {}", config.metadata_api_token)
                .parse()
                .map_err(|_| "ROUTER_METADATA_API_TOKEN contains invalid header characters")?,
        );
        let working = WorkingToken::default();
        let first = fetch_working_token(channel.clone(), exchange.clone()).await;
        let mut wait = match first {
            Ok((header, refresh_in)) => {
                *working.0.write().unwrap_or_else(PoisonError::into_inner) = Some(header);
                refresh_in
            }
            Err(error) => {
                tracing::warn!(%error, "metadata API working token unavailable at startup; retrying");
                RETRY_INITIAL
            }
        };
        let refresher = {
            let channel = channel.clone();
            let working = working.clone();
            tokio::spawn(async move {
                let mut backoff = RETRY_INITIAL;
                loop {
                    tokio::time::sleep(wait).await;
                    match fetch_working_token(channel.clone(), exchange.clone()).await {
                        Ok((header, refresh_in)) => {
                            *working.0.write().unwrap_or_else(PoisonError::into_inner) =
                                Some(header);
                            backoff = RETRY_INITIAL;
                            wait = refresh_in;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "metadata API working token refresh failed");
                            wait = backoff;
                            backoff = (backoff * 2).min(RETRY_MAX);
                        }
                    }
                }
            })
        };
        Ok(Self {
            teams: TeamServiceClient::with_interceptor(channel, working),
            kube_cluster_id: config.kube_cluster_id.clone(),
            cache: moka::future::Cache::builder()
                .max_capacity(CACHE_CAPACITY)
                .expire_after(EntryExpiry)
                .build(),
            refresher,
        })
    }

    async fn fetch(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvironmentRecord>, DirectoryUnavailable> {
        // An ID that cannot be a header value cannot name an environment.
        let Ok(header) = MetadataValue::try_from(environment_id) else {
            return Ok(None);
        };
        let mut request = Request::new(GetEnvIncludingArchivedRequest {});
        request.metadata_mut().insert(ENVIRONMENT_HEADER, header);
        tracing::info!(environment_id, "looking up environment (uncached)");
        let environment = match self.teams.clone().get_env_including_archived(request).await {
            Ok(response) => response.into_inner().environment,
            Err(status)
                if matches!(
                    status.code(),
                    Code::NotFound | Code::InvalidArgument | Code::PermissionDenied
                ) =>
            {
                None
            }
            Err(status) => return Err(DirectoryUnavailable(status.to_string())),
        };
        Ok(environment.and_then(|environment| {
            let served = environment.id == environment_id
                && !environment.team_id.is_empty()
                && environment.archived_at.is_none()
                && environment.suspended_at.is_none()
                && self
                    .kube_cluster_id
                    .as_ref()
                    .is_none_or(|expected| environment.kube_cluster_id.as_ref() == Some(expected));
            served.then_some(EnvironmentRecord {
                id: environment.id,
                team_id: environment.team_id,
            })
        }))
    }
}

#[async_trait::async_trait]
impl EnvironmentDirectory for MetadataEnvironmentDirectory {
    async fn lookup(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvironmentRecord>, DirectoryUnavailable> {
        // Concurrent misses for one environment share a single call; failures
        // are not cached.
        self.cache
            .try_get_with_by_ref(environment_id, self.fetch(environment_id))
            .await
            .map_err(|error: Arc<DirectoryUnavailable>| (*error).clone())
    }
}

impl Drop for MetadataEnvironmentDirectory {
    fn drop(&mut self) {
        self.refresher.abort();
    }
}
