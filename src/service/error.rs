use crate::types::{Error, ErrorResponse};
use axum::http::StatusCode;
use axum::Json;

/// A wire-format-agnostic error produced by the service layer.
///
/// Each variant carries enough information for both HTTP and gRPC layers to
/// produce an appropriate response. HTTP picks a status code via
/// [`ServiceError::http_status`]; gRPC picks a [`tonic::Code`] via
/// [`ServiceError::grpc_code`].
#[derive(Debug, Clone)]
pub enum ServiceError {
    /// A resource identity is already occupied. Maps to 409 / ALREADY_EXISTS.
    AlreadyExists { message: String },
    /// Resource does not exist. Maps to 404 / NOT_FOUND.
    NotFound { what: String },
    /// Client supplied an invalid argument. Maps to 400 / INVALID_ARGUMENT.
    InvalidArgument {
        field: Option<String>,
        message: String,
    },
    /// Server is configured in a way that makes the request impossible
    /// (e.g. a disabled backend). Maps to 403 / FAILED_PRECONDITION.
    FailedPrecondition { message: String },
    /// Unexpected server-side error. Maps to 500 / INTERNAL.
    Internal { message: String },
}

impl ServiceError {
    pub fn already_exists(message: impl Into<String>) -> Self {
        Self::AlreadyExists {
            message: message.into(),
        }
    }

    pub fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound { what: what.into() }
    }

    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            field: None,
            message: message.into(),
        }
    }

    pub fn invalid_field(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            field: Some(field.into()),
            message: message.into(),
        }
    }

    pub fn failed_precondition(message: impl Into<String>) -> Self {
        Self::FailedPrecondition {
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }

    pub fn message(&self) -> String {
        match self {
            ServiceError::AlreadyExists { message } => message.clone(),
            ServiceError::NotFound { what } => format!("{what} not found"),
            ServiceError::InvalidArgument { message, .. } => message.clone(),
            ServiceError::FailedPrecondition { message } => message.clone(),
            ServiceError::Internal { message } => message.clone(),
        }
    }

    pub fn http_status(&self) -> StatusCode {
        match self {
            ServiceError::AlreadyExists { .. } => StatusCode::CONFLICT,
            ServiceError::NotFound { .. } => StatusCode::NOT_FOUND,
            ServiceError::InvalidArgument { .. } => StatusCode::BAD_REQUEST,
            ServiceError::FailedPrecondition { .. } => StatusCode::FORBIDDEN,
            ServiceError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn error_type(&self) -> &'static str {
        match self {
            ServiceError::AlreadyExists { .. } => "already_exists",
            ServiceError::NotFound { .. } => "not_found",
            ServiceError::InvalidArgument { .. } => "invalid_request",
            ServiceError::FailedPrecondition { .. } => "configuration_error",
            ServiceError::Internal { .. } => "internal_error",
        }
    }

    pub fn grpc_code(&self) -> tonic::Code {
        match self {
            ServiceError::AlreadyExists { .. } => tonic::Code::AlreadyExists,
            ServiceError::NotFound { .. } => tonic::Code::NotFound,
            ServiceError::InvalidArgument { .. } => tonic::Code::InvalidArgument,
            ServiceError::FailedPrecondition { .. } => tonic::Code::FailedPrecondition,
            ServiceError::Internal { .. } => tonic::Code::Internal,
        }
    }

    /// Render as the JSON error envelope used by the HTTP layer.
    pub fn into_http(self) -> (StatusCode, Json<ErrorResponse>) {
        let status = self.http_status();
        let error_type = self.error_type().to_string();
        let (message, param) = match self {
            ServiceError::AlreadyExists { message } => (message, None),
            ServiceError::InvalidArgument { field, message } => (message, field),
            ServiceError::NotFound { what } => (format!("{what} not found"), None),
            ServiceError::FailedPrecondition { message } => (message, None),
            ServiceError::Internal { message } => (message, None),
        };
        (
            status,
            Json(ErrorResponse {
                error: Error {
                    message,
                    error_type,
                    param,
                    code: None,
                },
            }),
        )
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ServiceError {}

impl From<ServiceError> for tonic::Status {
    fn from(err: ServiceError) -> tonic::Status {
        tonic::Status::new(err.grpc_code(), err.message())
    }
}

impl From<crate::api_keys::ApiKeyStoreError> for ServiceError {
    fn from(e: crate::api_keys::ApiKeyStoreError) -> Self {
        use crate::api_keys::ApiKeyStoreError as E;
        match e {
            // A bad usage_pool_id is a client error (400), not a server fault.
            E::InvalidUsagePool => {
                ServiceError::invalid_field("usage_pool_id", "usage pool does not exist")
            }
            E::InvalidRateLimitConnection => {
                ServiceError::invalid_field("rate_limits", "connection does not exist")
            }
            E::DuplicateRateLimit => ServiceError::invalid_field(
                "rate_limits",
                crate::ratelimit::RATE_LIMIT_IDENTITY_CONFLICT,
            ),
            E::Internal(msg) => {
                tracing::error!(error = %msg, "api key store persistence error");
                ServiceError::internal("database error")
            }
        }
    }
}

impl From<crate::usage_pools::UsagePoolStoreError> for ServiceError {
    fn from(e: crate::usage_pools::UsagePoolStoreError) -> Self {
        use crate::usage_pools::UsagePoolStoreError as E;
        match e {
            E::DuplicateName => {
                ServiceError::already_exists("A usage pool with this name already exists.")
            }
            E::InvalidRateLimitConnection => {
                ServiceError::invalid_field("rate_limits", "connection does not exist")
            }
            E::Internal(msg) => {
                tracing::error!(error = %msg, "usage pool store persistence error");
                ServiceError::internal("database error")
            }
        }
    }
}
