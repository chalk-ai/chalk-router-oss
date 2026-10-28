//! gRPC service implementations.
//!
//! Each `*Server` in this module wraps an `AppState` and implements the
//! tonic-generated trait for a single service. They are thin adapters:
//! parse the proto request, dispatch to the service layer, translate
//! domain types back into proto responses.

pub mod proto {
    tonic::include_proto!("chalk.router.v1");
}

/// Canonical protobuf types for the standard gRPC health checking protocol.
#[allow(clippy::result_large_err)] // generated HealthClient returns tonic::Status
pub mod health_proto {
    tonic::include_proto!("grpc.health.v1");
}

mod api_keys;
mod background_responses;
mod conv;
mod debug;
mod fallback;
mod field_mask;
mod judge_policy;
mod model_routes;
mod pools;
mod provider_connections;
mod rate_limits;
mod router;
mod sampling_policy;
mod standard_health;
mod telemetry_settings;
mod usage;
mod usage_budgets;

pub use router::build_grpc_router;
pub use standard_health::HealthState;
