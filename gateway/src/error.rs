use axum::{
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("invalid request: {0}")]
    BadRequest(&'static str),
    #[error("authentication failed")]
    Unauthorized,
    #[error("not found")]
    NotFound,
    #[error("reserved object is not present in storage")]
    ObjectNotPresent { retry_after_seconds: u64 },
    #[error("rate limited")]
    RateLimited { retry_after_seconds: u64 },
    #[error("service capacity is temporarily exhausted")]
    ServiceUnavailable { retry_after_seconds: u64 },
    #[error("conflict: {0}")]
    Conflict(&'static str),
    #[error("object storage verification failed: {0}")]
    Upstream(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn internal(error: impl std::fmt::Display) -> Self {
        Self::Internal(error.to_string())
    }
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            Self::BadRequest(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The request is invalid.",
            ),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Authentication failed.",
            ),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "The requested resource was not found.",
            ),
            Self::ObjectNotPresent { .. } => (
                StatusCode::CONFLICT,
                "object_not_present",
                "The reserved object is not present yet.",
            ),
            Self::RateLimited { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "Too many requests. Retry later.",
            ),
            Self::ServiceUnavailable { .. } => (
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "The gateway is temporarily at capacity.",
            ),
            Self::Conflict(_) => (
                StatusCode::CONFLICT,
                "conflict",
                "The request conflicts with current state.",
            ),
            Self::Upstream(_) => (
                StatusCode::BAD_GATEWAY,
                "storage_verification_failed",
                "The uploaded object could not be verified.",
            ),
            Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "The gateway could not complete the request.",
            ),
        };

        match &self {
            Self::Internal(detail) => tracing::error!(error = %detail, "gateway internal error"),
            Self::Upstream(detail) => tracing::warn!(error = %detail, "B2 verification failed"),
            _ => tracing::debug!(error = %self, "request rejected"),
        }

        let mut response = (
            status,
            Json(ErrorEnvelope {
                error: ErrorBody { code, message },
            }),
        )
            .into_response();
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store, max-age=0"),
        );

        if matches!(self, Self::Unauthorized) {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"pusula-backup\""),
            );
        }
        let retry_after_seconds = match self {
            Self::RateLimited {
                retry_after_seconds,
            }
            | Self::ServiceUnavailable {
                retry_after_seconds,
            }
            | Self::ObjectNotPresent {
                retry_after_seconds,
            } => Some(retry_after_seconds),
            _ => None,
        };
        if let Some(retry_after_seconds) = retry_after_seconds {
            if let Ok(value) = HeaderValue::from_str(&retry_after_seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }

        response
    }
}

pub type Result<T> = std::result::Result<T, AppError>;
