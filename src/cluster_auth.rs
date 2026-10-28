//! Cluster authentication: verified credentials choose a served environment.
use std::{collections::BTreeMap, sync::Arc};

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use http::StatusCode;
use serde::Deserialize;

use crate::{
    api_keys::{ApiKey, ApiKeyStoreError, PostgresApiKeyStore},
    environment_directory::EnvironmentDirectory,
    environment_runtime::RuntimeProvisioner,
    hashing::KeyHasher,
    middleware::{
        extract_bearer, forbidden, is_admin_path, is_skip_path, unauthorized,
        validate_federated_token, AuthenticatedPrincipal,
    },
    tenancy::{EnvironmentRegistry, RequestRuntime},
    types::ErrorResponse,
    AppState,
};

/// Chalk claims, deserialized from the JWT payload only after federation has
/// verified its signature.
#[derive(Debug, Default, Deserialize)]
pub struct ChalkClaims {
    pub kind: Option<String>,
    pub environment_id: Option<String>,
    pub team_id: Option<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub env_permissions: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub team_permissions: Vec<String>,
    #[serde(default)]
    pub impersonated: bool,
    pub impersonated_team_id: Option<String>,
}

/// Token kinds that may act on a router; exchange tokens may not.
const WORKING_KINDS: &[&str] = &[
    "user",
    "service-account",
    "sandbox",
    "engine",
    "metadata-service-agent",
    "tenant",
];

/// The slug of `PERMISSION_AUTHENTICATED`.
const AUTHENTICATED: &str = "authenticated";

/// How a credential reaches an environment. Either way the environment
/// directory must confirm it before a runtime is selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentAccess {
    /// The credential itself names the environment: an environment-scoped token,
    /// a user grant for that environment, or an opaque key's owning row.
    Granted(String),
    /// A team-scoped credential; the directory must report this team as owner.
    TeamOwned {
        environment_id: String,
        team_id: String,
    },
}

impl EnvironmentAccess {
    pub fn environment_id(&self) -> &str {
        match self {
            Self::Granted(environment_id) | Self::TeamOwned { environment_id, .. } => {
                environment_id
            }
        }
    }
}

impl ChalkClaims {
    /// Shared operation policy for both tenancy modes. Unknown control-plane
    /// paths fail closed rather than inheriting an inference or login grant.
    pub fn permits_operation(&self, environment: &str, path: &str) -> bool {
        operation_permission(path).is_some_and(|accepted| self.permits(environment, accepted))
    }

    /// An environment-mode process cannot resolve team ownership dynamically.
    /// Require both the signed environment scope and its configured owner.
    pub fn authorize_environment_admin(
        &self,
        environment: &str,
        team: Option<&str>,
        path: &str,
    ) -> Result<(), String> {
        let team = team
            .filter(|id| !id.trim().is_empty())
            .ok_or("federated admin team is not configured")?;
        if environment.trim().is_empty() || self.environment_id.as_deref() != Some(environment) {
            return Err("admin token must name the configured environment".into());
        }
        self.access(Some(environment))?;
        let token_team = if self.kind.as_deref() == Some("user") {
            self.impersonated_team_id
                .as_deref()
                .or(self.team_id.as_deref())
        } else {
            self.team_id.as_deref()
        };
        if token_team != Some(team) {
            return Err("admin token must belong to the configured team".into());
        }
        if !self.permits_operation(environment, path) {
            return Err("credential lacks the required router operation permission".into());
        }
        Ok(())
    }

    /// Decide which environment the request targets from the claims and the
    /// optional `x-chalk-env-id` selector. Performs no I/O.
    pub fn access(&self, header: Option<&str>) -> Result<EnvironmentAccess, String> {
        let kind = self.kind.as_deref().unwrap_or("");
        if !WORKING_KINDS.contains(&kind) {
            return Err("bearer token kind is not a working credential".into());
        }
        if let Some(claimed) = self
            .environment_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
        {
            if header.is_some_and(|header| header != claimed) {
                return Err("x-chalk-env-id conflicts with the credential environment".into());
            }
            return Ok(EnvironmentAccess::Granted(claimed.into()));
        }
        if kind == "sandbox" {
            return Err("sandbox token names no environment".into());
        }
        let requested = header.ok_or("x-chalk-env-id must select an environment")?;
        let impersonated =
            kind == "user" && (self.impersonated || self.impersonated_team_id.is_some());
        if kind == "user" && !impersonated && self.env_permissions.contains_key(requested) {
            return Ok(EnvironmentAccess::Granted(requested.into()));
        }
        let team = self
            .impersonated_team_id
            .as_deref()
            .or(self.team_id.as_deref())
            .filter(|team| !team.trim().is_empty());
        match team {
            Some(team)
                if !(kind == "user" && !impersonated && self.team_permissions.is_empty()) =>
            {
                Ok(EnvironmentAccess::TeamOwned {
                    environment_id: requested.into(),
                    team_id: team.into(),
                })
            }
            _ => Err("credential cannot access the requested environment".into()),
        }
    }

    /// Whether the claims grant any of `accepted` in `environment`.
    pub fn permits(&self, environment: &str, accepted: &[&str]) -> bool {
        // `authenticated` names no specific grant: any working credential that
        // access() admitted to the environment may call the operation.
        if accepted.contains(&AUTHENTICATED)
            && WORKING_KINDS.contains(&self.kind.as_deref().unwrap_or(""))
        {
            return true;
        }
        let contains = |values: &[String]| {
            values
                .iter()
                .any(|value| accepted.contains(&value.as_str()))
        };
        match self.kind.as_deref() {
            Some("engine" | "metadata-service-agent") => true,
            Some("user") => {
                // Support impersonation grants every operation; access() still
                // confines the token to the team it impersonates.
                self.impersonated
                    || self.impersonated_team_id.is_some()
                    || contains(&self.team_permissions)
                    || self
                        .env_permissions
                        .get(environment)
                        .is_some_and(|values| contains(values))
            }
            Some("service-account") => {
                contains(&self.team_permissions)
                    || (self.environment_id.as_deref() == Some(environment)
                        && contains(&self.permissions))
            }
            Some("sandbox") => {
                self.environment_id.as_deref() == Some(environment) && contains(&self.permissions)
            }
            // Tenant tokens treat an empty or absent list as unrestricted, not as
            // an empty grant. access() has already confined them to their scope.
            Some("tenant") => self.permissions.is_empty() || contains(&self.permissions),
            _ => false,
        }
    }
}

mod generated {
    include!(concat!(env!("OUT_DIR"), "/operation_permissions.rs"));
}

pub use generated::ADMIN_OPERATION_PERMISSIONS;

/// Permission slugs accepted for a request path. Admin RPCs take theirs from
/// the `(chalk.auth.v1.permission)` options in `proto/chalk/router/v1`;
/// inference requires `query.online`. Unknown admin paths fail closed.
pub fn operation_permission(path: &str) -> Option<&'static [&'static str]> {
    if !is_admin_path(path) {
        return Some(&["query.online"]);
    }
    ADMIN_OPERATION_PERMISSIONS
        .iter()
        .find(|(rpc, _)| *rpc == path)
        .map(|(_, accepted)| *accepted)
}

/// Why an authenticated credential was not given a runtime.
#[derive(Debug)]
pub enum EnvironmentDenied {
    Forbidden(&'static str),
    Unavailable,
}

impl IntoResponse for EnvironmentDenied {
    fn into_response(self) -> Response {
        match self {
            Self::Forbidden(message) => forbidden(message),
            Self::Unavailable => ErrorResponse::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "Environment temporarily unavailable",
            )
            .into_response(),
        }
    }
}

pub struct ClusterAuth {
    pub registry: Arc<EnvironmentRegistry>,
    pub directory: Arc<dyn EnvironmentDirectory>,
    /// Loads confirmed environments on first use. Without one, only the
    /// registry's runtimes are served.
    pub provisioner: Option<Arc<RuntimeProvisioner>>,
    pub default_state: AppState,
    pub pool: Option<sqlx::PgPool>,
    pub hasher: KeyHasher,
}

impl ClusterAuth {
    /// Confirm the environment with the directory, then select its runtime.
    ///
    /// A directory outage still serves environments the credential names
    /// itself when their runtime is already loaded; team-scoped access fails
    /// with 503 because ownership cannot be proven.
    pub async fn authorize(
        &self,
        access: &EnvironmentAccess,
    ) -> Result<AppState, EnvironmentDenied> {
        let environment = access.environment_id();
        match self.directory.lookup(environment).await {
            Ok(Some(record)) => {
                if let EnvironmentAccess::TeamOwned { team_id, .. } = access {
                    if record.team_id != *team_id {
                        return Err(EnvironmentDenied::Forbidden(
                            "credential cannot access the requested environment",
                        ));
                    }
                }
            }
            Ok(None) => {
                return Err(EnvironmentDenied::Forbidden(
                    "environment is not served by this cluster",
                ))
            }
            Err(error) => match access {
                EnvironmentAccess::Granted(_) if self.registry.get(environment).is_some() => {
                    tracing::warn!(environment_id = %environment, %error, "serving a loaded environment while the directory is unavailable");
                }
                _ => {
                    tracing::warn!(environment_id = %environment, %error, "environment directory unavailable");
                    return Err(EnvironmentDenied::Unavailable);
                }
            },
        }
        self.runtime(environment).await
    }

    async fn runtime(&self, environment: &str) -> Result<AppState, EnvironmentDenied> {
        if let Some(state) = self.registry.get(environment) {
            return Ok(state);
        }
        let Some(provisioner) = &self.provisioner else {
            return Err(EnvironmentDenied::Forbidden(
                "environment is not served by this cluster",
            ));
        };
        provisioner
            .get_or_provision(environment)
            .await
            .map_err(|error| {
                tracing::error!(environment_id = %environment, %error, "could not load environment runtime");
                EnvironmentDenied::Unavailable
            })
    }

    /// The owning environment of an opaque router key, if the key is valid.
    pub async fn authenticate_key(
        &self,
        token: &str,
    ) -> Result<Option<(String, Arc<ApiKey>)>, ApiKeyStoreError> {
        if let Some(pool) = &self.pool {
            return PostgresApiKeyStore::authenticate_cluster(pool, &self.hasher, token).await;
        }
        let mut found = None;
        for state in self.registry.runtimes() {
            if let Some(key) = state.api_key_store.authenticate(token).await {
                if found.is_some() {
                    return Err(ApiKeyStoreError::Internal(
                        "ambiguous credential identity".into(),
                    ));
                }
                found = Some((state.environment_id.clone(), key));
            }
        }
        Ok(found)
    }
}

pub async fn cluster_auth_middleware(
    State(cluster): State<Arc<ClusterAuth>>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    if is_skip_path(&path) {
        return next.run(request).await;
    }
    let header = match request
        .headers()
        .get("x-chalk-env-id")
        .map(|value| value.to_str())
    {
        Some(Err(_)) => return forbidden("invalid x-chalk-env-id"),
        Some(Ok(id)) if id.trim().is_empty() => return forbidden("empty x-chalk-env-id"),
        Some(Ok(id)) => Some(id.to_owned()),
        None => None,
    };
    let Some(token) = extract_bearer(&request) else {
        return unauthorized("Missing credential");
    };
    // Legacy static admin access stays in its original environment.
    if cluster.default_state.admin_key.as_deref() == Some(token.as_str()) {
        if header
            .as_deref()
            .is_some_and(|id| id != cluster.default_state.environment_id)
        {
            return forbidden("static admin credential is scoped to its original environment");
        }
        request
            .extensions_mut()
            .insert(RequestRuntime(cluster.default_state.clone()));
        return crate::middleware::auth_middleware(
            State(cluster.default_state.clone()),
            request,
            next,
        )
        .await;
    }
    let (state, key, principal) =
        match validate_federated_token(&cluster.default_state, &token).await {
            Some(Ok(verified)) => {
                let Some(claims) = verified.chalk_claims.clone() else {
                    return unauthorized("invalid Chalk claims");
                };
                let access = match claims.access(header.as_deref()) {
                    Ok(access) => access,
                    Err(error) => return forbidden(error),
                };
                if !claims.permits_operation(access.environment_id(), &path) {
                    return forbidden("credential lacks the required router operation permission");
                }
                let state = match cluster.authorize(&access).await {
                    Ok(state) => state,
                    Err(denied) => return denied.into_response(),
                };
                let principal = AuthenticatedPrincipal::federated(&verified.claims.sub);
                let key = Arc::new(ApiKey {
                    key_hash: format!("federated:{}", verified.claims.sub),
                    key_prefix: "federated".into(),
                    description: "Federated Chalk JWT".into(),
                    created_at: chrono::Utc::now(),
                    permission: Default::default(),
                    labels: Default::default(),
                    usage_pool_id: None,
                    daily_token_budget: None,
                });
                request.extensions_mut().insert(claims);
                (state, key, principal)
            }
            Some(Err(_)) => return unauthorized("Invalid federated token"),
            None => {
                if is_admin_path(&path) {
                    return forbidden("Router API keys cannot access control-plane RPCs");
                }
                let (environment, key) = match cluster.authenticate_key(&token).await {
                    Ok(Some(found)) => found,
                    Ok(None) => return unauthorized("Invalid API key"),
                    Err(_) => {
                        return ErrorResponse::new(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "server_error",
                            "Authentication temporarily unavailable",
                        )
                        .into_response()
                    }
                };
                if header.as_deref().is_some_and(|id| id != environment) {
                    return forbidden("x-chalk-env-id conflicts with the credential environment");
                }
                let state = match cluster
                    .authorize(&EnvironmentAccess::Granted(environment))
                    .await
                {
                    Ok(state) => state,
                    Err(denied) => return denied.into_response(),
                };
                let principal = AuthenticatedPrincipal::api_key(&key.key_hash);
                (state, key, principal)
            }
        };
    request.extensions_mut().insert(RequestRuntime(state));
    request.extensions_mut().insert(key);
    request.extensions_mut().insert(principal);
    next.run(request).await
}
