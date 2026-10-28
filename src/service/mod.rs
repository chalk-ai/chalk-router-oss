//! Service layer.
//!
//! The HTTP and gRPC layers both parse their own wire formats, validate, then
//! dispatch to the structs in this module. The service layer holds the
//! business logic and is wire-format agnostic — it neither knows nor cares
//! whether a request arrived as JSON, protobuf, or anything else.

use std::sync::LazyLock;

/// Coordinates route writes and connection deletes for in-memory stores, whose
/// separate maps cannot share PostgreSQL's row locks.
pub(crate) static CONNECTION_ROUTE_MUTATION_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

pub mod api_keys;
pub mod background_responses;
pub mod debug;
pub mod error;
pub mod fallback;
pub mod judge_policy;
pub mod model_routes;
pub mod pagination;
pub mod provider_connections;
pub mod rate_limits;
pub mod sampling_policy;
pub mod usage;
pub mod usage_budgets;
pub mod usage_pools;

pub use api_keys::ApiKeyService;
pub use background_responses::BackgroundResponseService;
pub use debug::DebugService;
pub use error::ServiceError;
pub use fallback::FallbackPolicyService;
pub use judge_policy::JudgePolicyService;
pub use model_routes::ModelRouteService;
pub use provider_connections::ProviderConnectionService;
pub use rate_limits::RateLimitService;
pub use sampling_policy::SamplingPolicyService;
pub use usage::UsageService;
pub use usage_budgets::UsageBudgetService;
pub use usage_pools::UsagePoolService;
