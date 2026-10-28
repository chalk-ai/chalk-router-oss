use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::Stream;
use tonic::{Request, Response, Status};

use super::health_proto::health_check_response::ServingStatus;
use super::health_proto::health_server::Health;
use super::health_proto::{HealthCheckRequest, HealthCheckResponse};

const STARTUP_SERVICE: &str = "startup";
const READINESS_SERVICE: &str = "readiness";

/// Process lifecycle state exposed through the standard gRPC health protocol.
///
/// Startup is a monotonic latch: once the fully initialized router is accepting
/// RPCs it remains complete for the life of the process. Readiness may move in
/// either direction, including becoming false before graceful shutdown.
#[derive(Clone, Debug, Default)]
pub struct HealthState {
    inner: Arc<HealthStateInner>,
}

#[derive(Debug, Default)]
struct HealthStateInner {
    startup_complete: AtomicBool,
    ready: AtomicBool,
}

impl HealthState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark initialization complete after the h2c listener has been bound.
    /// There is deliberately no inverse operation: startup is monotonic.
    pub fn mark_startup_complete(&self) {
        self.inner.startup_complete.store(true, Ordering::Release);
    }

    /// Update whether the router can serve normal routing traffic and its
    /// required local dependencies are initialized.
    pub fn set_ready(&self, ready: bool) {
        self.inner.ready.store(ready, Ordering::Release);
    }

    fn serving_status(&self, service: &str) -> Option<ServingStatus> {
        match service {
            STARTUP_SERVICE => Some(if self.inner.startup_complete.load(Ordering::Acquire) {
                ServingStatus::Serving
            } else {
                ServingStatus::NotServing
            }),
            READINESS_SERVICE => Some(
                if self.inner.startup_complete.load(Ordering::Acquire)
                    && self.inner.ready.load(Ordering::Acquire)
                {
                    ServingStatus::Serving
                } else {
                    ServingStatus::NotServing
                },
            ),
            _ => None,
        }
    }
}

/// Implements `grpc.health.v1.Health` without dispatching into the router's
/// business-service layer or contacting any provider.
#[derive(Clone, Debug)]
pub struct GrpcHealthService {
    state: HealthState,
}

impl GrpcHealthService {
    pub fn new(state: HealthState) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl Health for GrpcHealthService {
    async fn check(
        &self,
        request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        let status = self
            .state
            .serving_status(&request.get_ref().service)
            .ok_or_else(|| Status::not_found("service not registered"))?;

        Ok(Response::new(HealthCheckResponse {
            status: status as i32,
        }))
    }

    type WatchStream =
        Pin<Box<dyn Stream<Item = Result<HealthCheckResponse, Status>> + Send + 'static>>;

    async fn watch(
        &self,
        _request: Request<HealthCheckRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        Err(Status::unimplemented("health watch is not supported"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_is_monotonic_and_readiness_is_reversible() {
        let state = HealthState::new();

        assert_eq!(
            state.serving_status(STARTUP_SERVICE),
            Some(ServingStatus::NotServing)
        );
        assert_eq!(
            state.serving_status(READINESS_SERVICE),
            Some(ServingStatus::NotServing)
        );

        // Readiness cannot become serving before startup completes.
        state.set_ready(true);
        assert_eq!(
            state.serving_status(READINESS_SERVICE),
            Some(ServingStatus::NotServing)
        );

        state.mark_startup_complete();
        assert_eq!(
            state.serving_status(STARTUP_SERVICE),
            Some(ServingStatus::Serving)
        );
        assert_eq!(
            state.serving_status(READINESS_SERVICE),
            Some(ServingStatus::Serving)
        );

        state.set_ready(false);
        assert_eq!(
            state.serving_status(STARTUP_SERVICE),
            Some(ServingStatus::Serving)
        );
        assert_eq!(
            state.serving_status(READINESS_SERVICE),
            Some(ServingStatus::NotServing)
        );

        state.set_ready(true);
        assert_eq!(
            state.serving_status(READINESS_SERVICE),
            Some(ServingStatus::Serving)
        );
    }

    #[test]
    fn unknown_services_are_not_registered() {
        let state = HealthState::new();
        assert_eq!(state.serving_status("other"), None);
        assert_eq!(state.serving_status(""), None);
    }
}
