//! The cluster-mode metadata client against an in-process metadata API.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};

use openai_server::{
    config::ClusterConfig,
    environment_directory::{EnvironmentDirectory, EnvironmentRecord},
    metadata_client::{
        proto::chalk::server::v1::{
            auth_service_server::{AuthService, AuthServiceServer},
            team_service_server::{TeamService, TeamServiceServer},
            Environment, GetEnvIncludingArchivedRequest, GetEnvIncludingArchivedResponse,
            GetInternalWorkingTokenRequest, GetInternalWorkingTokenResponse,
        },
        MetadataEnvironmentDirectory,
    },
};
use tonic::{Request, Response, Status};

fn header<'a, T>(request: &'a Request<T>, name: &str) -> Option<&'a str> {
    request
        .metadata()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

struct FakeAuth;

#[tonic::async_trait]
impl AuthService for FakeAuth {
    async fn get_internal_working_token(
        &self,
        request: Request<GetInternalWorkingTokenRequest>,
    ) -> Result<Response<GetInternalWorkingTokenResponse>, Status> {
        if header(&request, "authorization") != Some("Bearer exchange-token")
            || header(&request, "x-chalk-server") != Some("go-api")
        {
            return Err(Status::unauthenticated("bad exchange token"));
        }
        // PERMISSION_DEPLOY_READ authorizes the environment lookups.
        assert_eq!(request.get_ref().requested_permissions, [11]);
        Ok(Response::new(GetInternalWorkingTokenResponse {
            access_token: "working-token".into(),
            expires_at: Some((SystemTime::now() + Duration::from_secs(3600)).into()),
        }))
    }
}

#[derive(Default)]
struct FakeTeams {
    calls: AtomicUsize,
    environments: BTreeMap<String, Environment>,
}

#[tonic::async_trait]
impl TeamService for FakeTeams {
    async fn get_env_including_archived(
        &self,
        request: Request<GetEnvIncludingArchivedRequest>,
    ) -> Result<Response<GetEnvIncludingArchivedResponse>, Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if header(&request, "authorization") != Some("Bearer working-token") {
            return Err(Status::unauthenticated("bad working token"));
        }
        let environment = header(&request, "x-chalk-env-id").unwrap_or_default();
        match environment {
            "flaky" => Err(Status::unavailable("metadata database down")),
            "denied" => Err(Status::permission_denied("not yours")),
            _ => self
                .environments
                .get(environment)
                .cloned()
                .map(|environment| {
                    Response::new(GetEnvIncludingArchivedResponse {
                        environment: Some(environment),
                    })
                })
                .ok_or_else(|| Status::not_found("no such environment")),
        }
    }
}

fn environment(id: &str, team: &str, cluster: &str) -> (String, Environment) {
    (
        id.into(),
        Environment {
            id: id.into(),
            team_id: team.into(),
            kube_cluster_id: Some(cluster.into()),
            ..Default::default()
        },
    )
}

async fn directory(teams: Arc<FakeTeams>) -> MetadataEnvironmentDirectory {
    let routes = tonic::service::Routes::new(AuthServiceServer::new(FakeAuth))
        .add_service(TeamServiceServer::from_arc(teams))
        .into_axum_router();
    let endpoint = crate::support::mock::serve(routes).await;
    MetadataEnvironmentDirectory::connect(&ClusterConfig {
        metadata_api_endpoint: endpoint,
        metadata_api_token: "exchange-token".into(),
        kube_cluster_id: Some("kube-1".into()),
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn metadata_directory_confirms_live_environments_on_this_cluster() {
    let mut archived = environment("archived", "team-a", "kube-1");
    archived.1.archived_at = Some(SystemTime::now().into());
    let mut suspended = environment("suspended", "team-a", "kube-1");
    suspended.1.suspended_at = Some(SystemTime::now().into());
    let teams = Arc::new(FakeTeams {
        environments: [
            environment("a", "team-a", "kube-1"),
            environment("elsewhere", "team-a", "kube-2"),
            archived,
            suspended,
        ]
        .into(),
        ..Default::default()
    });
    let directory = directory(teams.clone()).await;

    assert_eq!(
        directory.lookup("a").await.unwrap(),
        Some(EnvironmentRecord {
            id: "a".into(),
            team_id: "team-a".into()
        })
    );
    for absent in ["archived", "suspended", "elsewhere", "missing", "denied"] {
        assert_eq!(directory.lookup(absent).await.unwrap(), None, "{absent}");
    }
    assert!(directory.lookup("flaky").await.is_err());

    // Answers are cached; failures are not.
    let calls = teams.calls.load(Ordering::SeqCst);
    assert!(directory.lookup("a").await.unwrap().is_some());
    assert_eq!(directory.lookup("missing").await.unwrap(), None);
    assert_eq!(teams.calls.load(Ordering::SeqCst), calls);
    assert!(directory.lookup("flaky").await.is_err());
    assert_eq!(teams.calls.load(Ordering::SeqCst), calls + 1);
}

#[tokio::test]
async fn concurrent_lookups_share_one_metadata_call() {
    let teams = Arc::new(FakeTeams {
        environments: [environment("a", "team-a", "kube-1")].into(),
        ..Default::default()
    });
    let directory = directory(teams.clone()).await;
    let results = futures::future::join_all((0..16).map(|_| directory.lookup("a"))).await;
    assert!(results.iter().all(|result| matches!(result, Ok(Some(_)))));
    assert_eq!(teams.calls.load(Ordering::SeqCst), 1);
}
