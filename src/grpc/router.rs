use axum::body::Body;
use axum::Router;
use axum::{extract::Request, middleware::Next, response::Response};
use base64::Engine;
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use pin_project_lite::pin_project;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;
use tonic::service::Routes;
use tower::ServiceBuilder;
use tracing::Instrument;

use crate::AppState;

use super::api_keys::ApiKeyServer;
use super::background_responses::BackgroundResponseServer;
use super::debug::DebugServer;
use super::fallback::FallbackPolicyServer;
use super::judge_policy::JudgePolicyServer;
use super::model_routes::ModelRouteServer;
use super::pools::UsagePoolServer;
use super::proto;
use super::provider_connections::ProviderConnectionServer;
use super::rate_limits::RateLimitServer;
use super::sampling_policy::SamplingPolicyServer;
use super::standard_health::{GrpcHealthService, HealthState};
use super::telemetry_settings::TelemetrySettingsServer;
use super::usage::UsageServer;
use super::usage_budgets::UsageBudgetServer;

/// Service names registered in [`build_grpc_router`]. Tonic exposes its route
/// dispatcher as an Axum fallback, so use this list to prevent that fallback
/// from claiming ordinary unmatched HTTP paths.
const GRPC_SERVICES: &[&str] = &[
    "chalk.router.v1.ApiKeyService",
    "chalk.router.v1.BackgroundResponseService",
    "chalk.router.v1.UsagePoolService",
    "chalk.router.v1.UsageService",
    "chalk.router.v1.UsageBudgetService",
    "chalk.router.v1.RateLimitService",
    "chalk.router.v1.FallbackPolicyService",
    "chalk.router.v1.JudgePolicyService",
    "chalk.router.v1.SamplingPolicyService",
    "chalk.router.v1.ProviderConnectionService",
    "chalk.router.v1.ModelRouteService",
    "chalk.router.v1.DebugService",
    "chalk.router.v1.TelemetrySettingsService",
    "grpc.health.v1.Health",
];

/// Build an `axum::Router` that serves all gRPC services.
///
/// Each tonic service is wrapped with [`tonic_web::GrpcWebLayer`] so browser
/// clients using `@connectrpc/connect-web`'s `createGrpcWebTransport` can
/// reach them. The returned router is meant to be `.merge()`-d into the main
/// axum router so HTTP and gRPC share a port.
pub fn build_grpc_router(state: AppState, health_state: HealthState) -> Router {
    use super::health_proto::health_server::HealthServer as GrpcHealthServer;
    use proto::api_key_service_server::ApiKeyServiceServer;
    use proto::background_response_service_server::BackgroundResponseServiceServer;
    use proto::debug_service_server::DebugServiceServer;
    use proto::fallback_policy_service_server::FallbackPolicyServiceServer;
    use proto::judge_policy_service_server::JudgePolicyServiceServer;
    use proto::model_route_service_server::ModelRouteServiceServer;
    use proto::provider_connection_service_server::ProviderConnectionServiceServer;
    use proto::rate_limit_service_server::RateLimitServiceServer;
    use proto::sampling_policy_service_server::SamplingPolicyServiceServer;
    use proto::telemetry_settings_service_server::TelemetrySettingsServiceServer;
    use proto::usage_budget_service_server::UsageBudgetServiceServer;
    use proto::usage_pool_service_server::UsagePoolServiceServer;
    use proto::usage_service_server::UsageServiceServer;

    Routes::new(grpc_web(ApiKeyServiceServer::new(ApiKeyServer {
        state: state.clone(),
    })))
    .add_service(grpc_web(BackgroundResponseServiceServer::new(
        BackgroundResponseServer {
            state: state.clone(),
        },
    )))
    .add_service(grpc_web(UsagePoolServiceServer::new(UsagePoolServer {
        state: state.clone(),
    })))
    .add_service(grpc_web(UsageServiceServer::new(UsageServer {
        state: state.clone(),
    })))
    .add_service(grpc_web(UsageBudgetServiceServer::new(UsageBudgetServer {
        state: state.clone(),
    })))
    .add_service(grpc_web(RateLimitServiceServer::new(RateLimitServer {
        state: state.clone(),
    })))
    .add_service(grpc_web(FallbackPolicyServiceServer::new(
        FallbackPolicyServer,
    )))
    .add_service(grpc_web(JudgePolicyServiceServer::new(JudgePolicyServer)))
    .add_service(grpc_web(SamplingPolicyServiceServer::new(
        SamplingPolicyServer {
            state: state.clone(),
        },
    )))
    .add_service(grpc_web(ProviderConnectionServiceServer::new(
        ProviderConnectionServer {
            state: state.clone(),
        },
    )))
    .add_service(grpc_web(ModelRouteServiceServer::new(ModelRouteServer {
        state: state.clone(),
    })))
    .add_service(grpc_web(DebugServiceServer::new(DebugServer {
        state: state.clone(),
    })))
    .add_service(grpc_web(TelemetrySettingsServiceServer::new(
        TelemetrySettingsServer {
            state: state.clone(),
        },
    )))
    .add_service(grpc_web(GrpcHealthServer::new(GrpcHealthService::new(
        health_state,
    ))))
    .into_axum_router()
    .layer(axum::middleware::from_fn(observe_grpc))
    .layer(axum::middleware::from_fn(reject_unknown_router_path))
}

/// Tonic's `into_axum_router` installs its service dispatcher as a fallback.
/// Without this guard, it receives every HTTP path that did not match the
/// OpenAI-compatible router and reports it as an empty gRPC response. Keep
/// genuine gRPC service paths on Tonic's dispatcher, including its native
/// `UNIMPLEMENTED` response for an unknown method on a known service.
async fn reject_unknown_router_path(req: Request, next: Next) -> Response {
    if is_grpc_service_path(req.uri().path()) {
        next.run(req).await
    } else {
        crate::handlers::not_found_response()
    }
}

fn is_grpc_service_path(path: &str) -> bool {
    let Some((service, method)) = path.trim_start_matches('/').split_once('/') else {
        return false;
    };

    !method.is_empty() && !method.contains('/') && GRPC_SERVICES.contains(&service)
}

/// Attach a method-level span to every gRPC/gRPC-Web request. The outer HTTP
/// trace records transport latency, while this span adds stable RPC dimensions
/// and observes response frames through the final trailers.
async fn observe_grpc(req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let (service, method) = path
        .trim_start_matches('/')
        .split_once('/')
        .unwrap_or((path, ""));
    let request_bytes = req
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let started = Instant::now();
    let span = tracing::info_span!(
        "grpc_request",
        rpc.system = "grpc",
        rpc.service = service,
        rpc.method = method,
        chalk.rpc.request_bytes = request_bytes,
        chalk.rpc.response_bytes = tracing::field::Empty,
        rpc.grpc.status_code = tracing::field::Empty,
        chalk.rpc.duration_ms = tracing::field::Empty,
    );

    let response = next.run(req).instrument(span.clone()).await;
    let header_status = response
        .headers()
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    let grpc_web_parser = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(GrpcWebResponseParser::for_content_type);
    let (parts, body) = response.into_parts();
    let body_is_end_stream = body.is_end_stream();
    let observation = Arc::new(RpcObservation {
        span,
        started,
        response_bytes: AtomicU64::new(0),
        header_status,
        finished: AtomicBool::new(false),
    });
    finish_if_end_stream(body_is_end_stream, &observation);
    Response::from_parts(
        parts,
        Body::new(ObservedGrpcBody {
            inner: body,
            observation,
            grpc_web_parser,
        }),
    )
}

fn finish_if_end_stream(is_end_stream: bool, observation: &RpcObservation) {
    if is_end_stream {
        observation.finish(None);
    }
}

pin_project! {
    struct ObservedGrpcBody {
        #[pin]
        inner: Body,
        observation: Arc<RpcObservation>,
        grpc_web_parser: Option<GrpcWebResponseParser>,
    }
}

struct RpcObservation {
    span: tracing::Span,
    started: Instant,
    response_bytes: AtomicU64,
    header_status: Option<i64>,
    finished: AtomicBool,
}

impl RpcObservation {
    fn add_response_bytes(&self, bytes: usize) {
        self.response_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn finish(&self, grpc_status: Option<i64>) {
        if self
            .finished
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.span.record(
            "chalk.rpc.duration_ms",
            self.started.elapsed().as_secs_f64() * 1_000.0,
        );
        self.span.record(
            "chalk.rpc.response_bytes",
            self.response_bytes.load(Ordering::Relaxed),
        );
        if let Some(status) = grpc_status.or(self.header_status) {
            self.span.record("rpc.grpc.status_code", status);
        }
    }
}

impl Drop for RpcObservation {
    fn drop(&mut self) {
        // Covers client disconnects and bodies that a transport drops without
        // polling through an end-of-stream frame.
        self.finish(None);
    }
}

impl HttpBody for ObservedGrpcBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        match this.inner.as_mut().poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.observation.add_response_bytes(data.len());
                    if let Some(status) = this
                        .grpc_web_parser
                        .as_mut()
                        .and_then(|parser| parser.feed(data))
                    {
                        this.observation.finish(Some(status));
                    }
                }
                if let Some(trailers) = frame.trailers_ref() {
                    let trailer_status = trailers
                        .get("grpc-status")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<i64>().ok());
                    this.observation.finish(trailer_status);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.observation.finish(None);
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.observation.finish(None);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Incremental parser for the binary gRPC-Web envelope. Unlike native gRPC,
/// gRPC-Web serializes its final HTTP/2-style trailers into a data frame whose
/// high bit is set, so ordinary `Body::trailers` observation cannot see status.
#[derive(Default)]
struct GrpcWebFrameParser {
    header: [u8; 5],
    header_len: usize,
    remaining: usize,
    is_trailer: bool,
    trailer: Vec<u8>,
}

impl GrpcWebFrameParser {
    fn feed(&mut self, mut bytes: &[u8]) -> Option<i64> {
        while !bytes.is_empty() {
            if self.remaining == 0 {
                let needed = 5 - self.header_len;
                let take = needed.min(bytes.len());
                self.header[self.header_len..self.header_len + take]
                    .copy_from_slice(&bytes[..take]);
                self.header_len += take;
                bytes = &bytes[take..];
                if self.header_len < 5 {
                    break;
                }
                self.is_trailer = self.header[0] & 0x80 != 0;
                self.remaining = u32::from_be_bytes([
                    self.header[1],
                    self.header[2],
                    self.header[3],
                    self.header[4],
                ]) as usize;
                self.header_len = 0;
                self.trailer.clear();
                if self.remaining == 0 {
                    continue;
                }
            }

            let take = self.remaining.min(bytes.len());
            if self.is_trailer {
                self.trailer.extend_from_slice(&bytes[..take]);
            }
            self.remaining -= take;
            bytes = &bytes[take..];
            if self.remaining == 0 && self.is_trailer {
                let status = std::str::from_utf8(&self.trailer)
                    .ok()
                    .and_then(|trailers| {
                        trailers.split("\r\n").find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("grpc-status")
                                .then(|| value.trim().parse::<i64>().ok())
                                .flatten()
                        })
                    });
                if status.is_some() {
                    return status;
                }
            }
        }
        None
    }
}

enum GrpcWebResponseParser {
    Binary(GrpcWebFrameParser),
    Text(GrpcWebTextFrameParser),
}

impl GrpcWebResponseParser {
    fn for_content_type(content_type: &str) -> Option<Self> {
        let mime = content_type.split(';').next()?.trim();
        if mime.eq_ignore_ascii_case("application/grpc-web+proto") {
            Some(Self::Binary(GrpcWebFrameParser::default()))
        } else if mime.eq_ignore_ascii_case("application/grpc-web-text+proto") {
            Some(Self::Text(GrpcWebTextFrameParser::default()))
        } else {
            None
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Option<i64> {
        match self {
            Self::Binary(parser) => parser.feed(bytes),
            Self::Text(parser) => parser.feed(bytes),
        }
    }
}

#[derive(Default)]
struct GrpcWebTextFrameParser {
    encoded: Vec<u8>,
    decoded: Vec<u8>,
    binary: GrpcWebFrameParser,
}

impl GrpcWebTextFrameParser {
    fn feed(&mut self, bytes: &[u8]) -> Option<i64> {
        self.encoded.extend(
            bytes
                .iter()
                .copied()
                .filter(|byte| !byte.is_ascii_whitespace()),
        );
        let complete_len = self.encoded.len() / 4 * 4;
        if complete_len == 0 {
            return None;
        }

        let mut status = None;
        let mut start = 0;
        while start < complete_len {
            // gRPC-Web text may base64-encode each frame independently, leaving
            // padding inside the concatenated response. Decode in bulk between
            // padded quartet boundaries rather than treating it as one stream.
            let end = self.encoded[start..complete_len]
                .iter()
                .position(|byte| *byte == b'=')
                .map_or(complete_len, |padding| start + (padding / 4 + 1) * 4);
            let decoded_capacity = (end - start) / 4 * 3;
            self.decoded.resize(decoded_capacity, 0);
            let decoded_len = match base64::engine::general_purpose::STANDARD
                .decode_slice(&self.encoded[start..end], &mut self.decoded)
            {
                Ok(decoded_len) => decoded_len,
                Err(_) => {
                    self.encoded.clear();
                    self.decoded.clear();
                    return None;
                }
            };
            status = self.binary.feed(&self.decoded[..decoded_len]).or(status);
            start = end;
        }
        self.encoded.drain(..complete_len);
        status
    }
}

/// Wrap a tonic-generated service with [`tonic_web::GrpcWebLayer`].
///
/// [`tonic_web::GrpcWebService`] preserves [`tonic::server::NamedService`]
/// and inherits its inner service's `Error = Infallible`, so the wrapped
/// service still satisfies [`Routes::add_service`]'s bounds.
fn grpc_web<S>(svc: S) -> tonic_web::GrpcWebService<S>
where
    S: tower::Service<
            http::Request<tonic::body::BoxBody>,
            Response = http::Response<tonic::body::BoxBody>,
        > + tonic::server::NamedService
        + Send
        + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
{
    ServiceBuilder::new()
        .layer(tonic_web::GrpcWebLayer::new())
        .service(svc)
}

#[cfg(test)]
mod observation_tests {
    use super::{
        finish_if_end_stream, is_grpc_service_path, reject_unknown_router_path, GrpcWebFrameParser,
        GrpcWebResponseParser, RpcObservation,
    };
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        middleware,
        routing::get,
        Router,
    };
    use base64::Engine;
    use http_body_util::BodyExt;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Instant;
    use tower::ServiceExt;

    fn trailer_frame() -> Vec<u8> {
        let trailers = b"grpc-status: 7\r\ngrpc-message: denied\r\n";
        let mut frame = vec![0x80];
        frame.extend_from_slice(&(trailers.len() as u32).to_be_bytes());
        frame.extend_from_slice(trailers);
        frame
    }

    #[test]
    fn parses_grpc_web_status_across_body_chunks() {
        let frame = trailer_frame();
        let mut parser = GrpcWebFrameParser::default();
        assert_eq!(parser.feed(&frame[..3]), None);
        assert_eq!(parser.feed(&frame[3..11]), None);
        assert_eq!(parser.feed(&frame[11..]), Some(7));
    }

    #[test]
    fn parses_grpc_web_text_status_across_body_chunks() {
        let data_frame = [0, 0, 0, 0, 2, 1, 2];
        let encoded = format!(
            "{}{}",
            base64::engine::general_purpose::STANDARD.encode(data_frame),
            base64::engine::general_purpose::STANDARD.encode(trailer_frame()),
        );
        let mut parser = GrpcWebResponseParser::for_content_type(
            "application/grpc-web-text+proto; charset=utf-8",
        )
        .unwrap();
        assert_eq!(parser.feed(&encoded.as_bytes()[..3]), None);
        assert_eq!(parser.feed(&encoded.as_bytes()[3..11]), None);
        assert_eq!(parser.feed(&encoded.as_bytes()[11..]), Some(7));
    }

    #[test]
    fn finalizes_trailers_only_response_eagerly() {
        let observation = RpcObservation {
            span: tracing::Span::none(),
            started: Instant::now(),
            response_bytes: AtomicU64::new(0),
            header_status: Some(13),
            finished: AtomicBool::new(false),
        };
        finish_if_end_stream(true, &observation);
        assert!(observation.finished.load(Ordering::Acquire));
    }

    #[test]
    fn recognizes_binary_and_text_grpc_web_content_types() {
        assert!(GrpcWebResponseParser::for_content_type(
            "application/grpc-web+proto; charset=utf-8"
        )
        .is_some());
        assert!(
            GrpcWebResponseParser::for_content_type("application/grpc-web-text+proto").is_some()
        );
        assert!(GrpcWebResponseParser::for_content_type("application/grpc").is_none());
    }

    #[test]
    fn recognizes_only_registered_grpc_service_paths() {
        assert!(is_grpc_service_path(
            "/chalk.router.v1.ApiKeyService/CreateKey"
        ));
        assert!(is_grpc_service_path("/grpc.health.v1.Health/Check"));
        assert!(!is_grpc_service_path("/v1/not-a-real-endpoint"));
        assert!(!is_grpc_service_path(
            "/chalk.router.v1.UnknownService/Call"
        ));
        assert!(!is_grpc_service_path(
            "/chalk.router.v1.ApiKeyService/CreateKey/extra"
        ));
    }

    #[tokio::test]
    async fn unknown_router_paths_return_json_not_found() {
        let app = Router::new()
            .route("/known", get(|| async { StatusCode::OK }))
            .layer(middleware::from_fn(reject_unknown_router_path));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/not-a-real-endpoint")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("infallible router");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        let body = response
            .into_body()
            .collect()
            .await
            .expect("response body")
            .to_bytes();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).expect("JSON error body"),
            serde_json::json!({
                "error": {
                    "message": "Unknown router path",
                    "type": "invalid_request_error",
                    "param": null,
                    "code": null,
                }
            })
        );
    }
}
