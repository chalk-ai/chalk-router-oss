//! End-to-end smoke test that boots the full gRPC stack on a local
//! listener and exercises one RPC per service. Asserts both wire-format
//! interop (gRPC-Web framing matches what tonic-web produces) and that
//! the service layer is reachable through the gRPC adapters.

use std::sync::Arc;

use openai_server::{
    grpc::{
        build_grpc_router,
        health_proto::{
            health_check_response::ServingStatus, health_client::HealthClient, HealthCheckRequest,
        },
        HealthState,
    },
    middleware::auth_middleware,
    AppState,
};

use crate::support::app::AppStateBuilder;

async fn test_state() -> AppState {
    use openai_server::backend::RouterBackend;
    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        Arc::new(openai_server::provider_connections::InMemoryProviderConnectionStore::new()),
        Arc::new(openai_server::model_routes::InMemoryModelRouteStore::new()),
    ));
    // No test here sends an Authorization header, and `boot_server` doesn't
    // even layer `auth_middleware`, so the default seeded TEST_API_KEY is
    // inert — no need to override the api-key store.
    AppStateBuilder::new(backend).build().await
}

async fn boot_server() -> u16 {
    let state = test_state().await;
    let app = build_grpc_router(state, HealthState::new());
    let base = crate::support::mock::serve(app).await;
    base.rsplit(':').next().unwrap().parse().unwrap()
}

async fn boot_standard_health_server() -> (u16, HealthState) {
    let state = test_state().await;
    let health_state = HealthState::new();
    let app = build_grpc_router(state.clone(), health_state.clone())
        .layer(axum::middleware::from_fn_with_state(state, auth_middleware));
    let base = crate::support::mock::serve(app).await;
    let port = base.rsplit(':').next().unwrap().parse().unwrap();
    (port, health_state)
}

async fn standard_health_status(
    client: &mut HealthClient<tonic::transport::Channel>,
    service: &str,
) -> ServingStatus {
    let response = client
        .check(HealthCheckRequest {
            service: service.to_string(),
        })
        .await
        .expect("health check")
        .into_inner();
    ServingStatus::try_from(response.status).expect("known serving status")
}

/// Frame an already-encoded protobuf message in the gRPC-Web envelope:
/// 1-byte flags (0 for data, no compression), 4-byte big-endian length,
/// then the payload.
fn grpc_web_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(0);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[tokio::test]
async fn standard_health_startup_transitions_over_native_h2c() {
    let (port, health_state) = boot_standard_health_server().await;
    let mut client = HealthClient::connect(format!("http://127.0.0.1:{port}"))
        .await
        .expect("connect health client");

    assert_eq!(
        standard_health_status(&mut client, "startup").await,
        ServingStatus::NotServing
    );

    health_state.mark_startup_complete();
    assert_eq!(
        standard_health_status(&mut client, "startup").await,
        ServingStatus::Serving
    );

    // Losing readiness during drain must not regress the startup latch.
    health_state.set_ready(false);
    assert_eq!(
        standard_health_status(&mut client, "startup").await,
        ServingStatus::Serving
    );
}

#[tokio::test]
async fn standard_health_readiness_transitions_over_native_h2c() {
    let (port, health_state) = boot_standard_health_server().await;
    let mut client = HealthClient::connect(format!("http://127.0.0.1:{port}"))
        .await
        .expect("connect health client");

    assert_eq!(
        standard_health_status(&mut client, "readiness").await,
        ServingStatus::NotServing
    );

    // Required dependencies cannot make the router ready before startup.
    health_state.set_ready(true);
    assert_eq!(
        standard_health_status(&mut client, "readiness").await,
        ServingStatus::NotServing
    );

    health_state.mark_startup_complete();
    assert_eq!(
        standard_health_status(&mut client, "readiness").await,
        ServingStatus::Serving
    );

    health_state.set_ready(false);
    assert_eq!(
        standard_health_status(&mut client, "readiness").await,
        ServingStatus::NotServing
    );

    health_state.set_ready(true);
    assert_eq!(
        standard_health_status(&mut client, "readiness").await,
        ServingStatus::Serving
    );
}

#[tokio::test]
async fn standard_health_rejects_unknown_services_over_native_h2c() {
    let (port, _health_state) = boot_standard_health_server().await;
    let mut client = HealthClient::connect(format!("http://127.0.0.1:{port}"))
        .await
        .expect("connect health client");

    let error = client
        .check(HealthCheckRequest {
            service: "unknown".to_string(),
        })
        .await
        .expect_err("unknown service should fail");
    assert_eq!(error.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn provider_connections_list_over_grpc_web() {
    let port = boot_server().await;
    let url = format!(
        "http://127.0.0.1:{port}/chalk.router.v1.ProviderConnectionService/ListProviderConnections"
    );

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .body(grpc_web_frame(&[]))
        .send()
        .await
        .expect("send");

    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("grpc-status:0"), "body: {body_str:?}");
}

#[tokio::test]
async fn route_judge_defaults_over_grpc_web() {
    use openai_server::{grpc::proto::GetRouteJudgeDefaultsResponse, route_grid::Classification};
    use prost::Message;

    let port = boot_server().await;
    let response = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/chalk.router.v1.ModelRouteService/GetRouteJudgeDefaults"
        ))
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .body(grpc_web_frame(&[]))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.bytes().await.unwrap();
    assert_eq!(body[0], 0);
    let length = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
    let defaults = GetRouteJudgeDefaultsResponse::decode(&body[5..5 + length]).unwrap();
    for (entries, classification) in [
        (defaults.difficulty, Classification::Difficulty),
        (defaults.task, Classification::Task),
    ] {
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.label.as_str(), entry.description.as_str()))
                .collect::<Vec<_>>(),
            classification.default_outcomes()
        );
    }
}

#[tokio::test]
async fn background_response_listing_requires_admin_auth() {
    use openai_server::grpc::proto;
    use prost::Message;
    let mut state = test_state().await;
    state.admin_key = Some("admin-operations-test".to_string());
    let app = build_grpc_router(state.clone(), HealthState::new())
        .layer(axum::middleware::from_fn_with_state(state, auth_middleware));
    let base = crate::support::mock::serve(app).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/chalk.router.v1.BackgroundResponseService/ListBackgroundResponses");
    for (token, expected) in [
        (None, 401),
        (Some("ck-not-an-admin"), 403),
        (Some("admin-operations-test"), 200),
    ] {
        let mut request = client
            .post(&url)
            .header("content-type", "application/grpc-web+proto")
            .header("x-grpc-web", "1")
            .body(grpc_web_frame(&[]));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status().as_u16(), expected);
        if expected == 200 {
            let body = response.bytes().await.unwrap();
            assert!(String::from_utf8_lossy(&body).contains("grpc-status:0"));
            let length = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
            let list =
                proto::ListBackgroundResponsesResponse::decode(&body[5..5 + length]).unwrap();
            assert!(list.jobs.is_empty());
        }
    }
    for request in [
        proto::ListBackgroundResponsesRequest {
            phase: Some("invalid".to_string()),
            ..Default::default()
        },
        proto::ListBackgroundResponsesRequest {
            limit: Some(0),
            ..Default::default()
        },
    ] {
        let response = client
            .post(&url)
            .bearer_auth("admin-operations-test")
            .header("content-type", "application/grpc-web+proto")
            .header("x-grpc-web", "1")
            .body(grpc_web_frame(&request.encode_to_vec()))
            .send()
            .await
            .unwrap();
        assert_eq!(
            grpc_status(&response),
            Some(tonic::Code::InvalidArgument as i32)
        );
    }
}

/// A destructive RPC called with an empty body (no `id`) must be rejected
/// as a malformed request, not silently reported as a 200 success — a
/// delete that "works" on a missing id is a poor primitive to automate
/// against and useless as an existence probe. This is distinct from
/// deleting an unknown-but-well-formed id, which stays a no-op success.
#[tokio::test]
async fn delete_provider_connection_rejects_empty_id() {
    let port = boot_server().await;
    let url = format!(
        "http://127.0.0.1:{port}/chalk.router.v1.ProviderConnectionService/DeleteProviderConnection"
    );

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .body(grpc_web_frame(&[]))
        .send()
        .await
        .expect("send");

    let status = grpc_status(&resp);
    let body = resp.bytes().await.unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        Some(tonic::Code::InvalidArgument as i32),
        "headers/body: {body_str:?}"
    );
}

/// Same as above for RevokeKey — an empty `key` must be rejected rather
/// than treated as an idempotent no-op.
#[tokio::test]
async fn revoke_key_rejects_empty_key() {
    let port = boot_server().await;
    let url = format!("http://127.0.0.1:{port}/chalk.router.v1.ApiKeyService/RevokeKey");

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .body(grpc_web_frame(&[]))
        .send()
        .await
        .expect("send");

    let status = grpc_status(&resp);
    let body = resp.bytes().await.unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        Some(tonic::Code::InvalidArgument as i32),
        "headers/body: {body_str:?}"
    );
}

/// A read RPC called with an empty body (no `id`) must be rejected as a
/// malformed request rather than treated as a lookup for the empty string.
#[tokio::test]
async fn get_usage_pool_rejects_empty_id() {
    let port = boot_server().await;
    let url = format!("http://127.0.0.1:{port}/chalk.router.v1.UsagePoolService/GetUsagePool");

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .body(grpc_web_frame(&[]))
        .send()
        .await
        .expect("send");

    let status = grpc_status(&resp);
    let body = resp.bytes().await.unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        Some(tonic::Code::InvalidArgument as i32),
        "headers/body: {body_str:?}"
    );
}

/// A well-formed but unknown id must come back as NotFound, not a generic
/// error or an empty success response.
#[tokio::test]
async fn get_usage_pool_unknown_is_not_found() {
    use openai_server::grpc::proto::GetUsagePoolRequest;
    use prost::Message;

    let port = boot_server().await;
    let url = format!("http://127.0.0.1:{port}/chalk.router.v1.UsagePoolService/GetUsagePool");

    let payload = GetUsagePoolRequest {
        lookup: Some(
            openai_server::grpc::proto::get_usage_pool_request::Lookup::Id(
                "some-unknown-id".into(),
            ),
        ),
    }
    .encode_to_vec();

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .body(grpc_web_frame(&payload))
        .send()
        .await
        .expect("send");

    let status = grpc_status(&resp);
    let body = resp.bytes().await.unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        Some(tonic::Code::NotFound as i32),
        "headers/body: {body_str:?}"
    );
}

#[tokio::test]
async fn usage_pool_name_conflicts_are_already_exists_over_grpc() {
    use openai_server::grpc::proto::{
        CreateUsagePoolRequest, CreateUsagePoolResponse, UpdateUsagePoolOperation,
        UpdateUsagePoolRequest,
    };
    use prost::Message;

    async fn call(
        client: &reqwest::Client,
        base: &str,
        method: &str,
        payload: Vec<u8>,
    ) -> reqwest::Response {
        client
            .post(format!("{base}/chalk.router.v1.UsagePoolService/{method}"))
            .header("content-type", "application/grpc-web+proto")
            .header("x-grpc-web", "1")
            .body(grpc_web_frame(&payload))
            .send()
            .await
            .unwrap()
    }
    let base = format!("http://127.0.0.1:{}", boot_server().await);
    let client = reqwest::Client::new();
    let response = call(
        &client,
        &base,
        "CreateUsagePool",
        CreateUsagePoolRequest {
            name: "Production".into(),
            ..Default::default()
        }
        .encode_to_vec(),
    )
    .await;
    let body = response.bytes().await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("grpc-status:0"));

    let response = call(
        &client,
        &base,
        "CreateUsagePool",
        CreateUsagePoolRequest {
            name: " Production ".into(),
            ..Default::default()
        }
        .encode_to_vec(),
    )
    .await;
    assert_eq!(
        grpc_status(&response),
        Some(tonic::Code::AlreadyExists as i32)
    );

    let response = call(
        &client,
        &base,
        "CreateUsagePool",
        CreateUsagePoolRequest {
            name: "Other".into(),
            ..Default::default()
        }
        .encode_to_vec(),
    )
    .await;
    let body = response.bytes().await.unwrap();
    let length = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
    let other = CreateUsagePoolResponse::decode(&body[5..5 + length])
        .unwrap()
        .pool
        .unwrap();
    let response = call(
        &client,
        &base,
        "UpdateUsagePool",
        UpdateUsagePoolRequest {
            id: other.id,
            update: Some(UpdateUsagePoolOperation {
                name: Some("Production".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
        .encode_to_vec(),
    )
    .await;
    assert_eq!(
        grpc_status(&response),
        Some(tonic::Code::AlreadyExists as i32)
    );
}

/// Read the grpc-status out of response headers (trailers-only responses).
fn grpc_status(resp: &reqwest::Response) -> Option<i32> {
    resp.headers()
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

#[tokio::test]
async fn usage_pool_lookup_supports_names_and_legacy_id_requests() {
    use openai_server::grpc::proto::{
        get_usage_pool_request::Lookup, CreateUsagePoolRequest, CreateUsagePoolResponse,
        GetUsagePoolRequest, GetUsagePoolResponse,
    };
    use prost::Message;

    // This is the pre-oneof message shape, independent of generated bindings.
    #[derive(Clone, PartialEq, Message)]
    struct LegacyGetUsagePoolRequest {
        #[prost(string, tag = "1")]
        id: String,
    }

    async fn call(
        client: &reqwest::Client,
        base: &str,
        method: &str,
        payload: Vec<u8>,
    ) -> reqwest::Response {
        client
            .post(format!("{base}/chalk.router.v1.UsagePoolService/{method}"))
            .header("content-type", "application/grpc-web+proto")
            .header("x-grpc-web", "1")
            .body(grpc_web_frame(&payload))
            .send()
            .await
            .unwrap()
    }

    let base = format!("http://127.0.0.1:{}", boot_server().await);
    let client = reqwest::Client::new();
    let mut pools = Vec::new();
    for name in ["Production", "production"] {
        let response = call(
            &client,
            &base,
            "CreateUsagePool",
            CreateUsagePoolRequest {
                name: name.into(),
                ..Default::default()
            }
            .encode_to_vec(),
        )
        .await;
        let body = response.bytes().await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("grpc-status:0"));
        let length = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
        pools.push(
            CreateUsagePoolResponse::decode(&body[5..5 + length])
                .unwrap()
                .pool
                .unwrap(),
        );
    }
    let legacy_payload = LegacyGetUsagePoolRequest {
        id: pools[0].id.clone(),
    }
    .encode_to_vec();
    assert_eq!(
        legacy_payload,
        GetUsagePoolRequest {
            lookup: Some(Lookup::Id(pools[0].id.clone()))
        }
        .encode_to_vec()
    );
    for (payload, expected) in [
        (legacy_payload, &pools[0]),
        (
            GetUsagePoolRequest {
                lookup: Some(Lookup::Name("Production".into())),
            }
            .encode_to_vec(),
            &pools[0],
        ),
        (
            GetUsagePoolRequest {
                lookup: Some(Lookup::Name("\tproduction\u{3000}".into())),
            }
            .encode_to_vec(),
            &pools[1],
        ),
    ] {
        let response = call(&client, &base, "GetUsagePool", payload).await;
        let body = response.bytes().await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("grpc-status:0"));
        let length = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
        let pool = GetUsagePoolResponse::decode(&body[5..5 + length])
            .unwrap()
            .pool
            .unwrap();
        assert_eq!(pool.id, expected.id);
        assert_eq!(pool.name, expected.name);
    }
    for (lookup, expected_code) in [
        (None, tonic::Code::InvalidArgument),
        (
            Some(Lookup::Id(String::new())),
            tonic::Code::InvalidArgument,
        ),
        (
            Some(Lookup::Name(String::new())),
            tonic::Code::InvalidArgument,
        ),
        (
            Some(Lookup::Name(" \t".into())),
            tonic::Code::InvalidArgument,
        ),
        (
            Some(Lookup::Name("PRODUCTION".into())),
            tonic::Code::NotFound,
        ),
        // An ID selector must not fall back to a name, or vice versa.
        (Some(Lookup::Id("Production".into())), tonic::Code::NotFound),
        (
            Some(Lookup::Name(pools[0].id.clone())),
            tonic::Code::NotFound,
        ),
    ] {
        let response = call(
            &client,
            &base,
            "GetUsagePool",
            GetUsagePoolRequest { lookup }.encode_to_vec(),
        )
        .await;
        assert_eq!(grpc_status(&response), Some(expected_code as i32));
    }
}
