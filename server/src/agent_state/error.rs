//! The one error type every agent state handler returns.

use std::time::Duration;

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// What went wrong with a request, named for what the caller can do about it.
///
/// Bodies follow `API_CONVENTIONS.md` section 2, `{"error": <message>, "code": <slug>}`: the slug
/// is the stable part clients switch on, the message may be reworded. 5xx bodies never carry
/// internal detail; it goes to the log.
#[derive(Debug)]
pub(super) enum StateError {
    /// No credential, or one that is malformed, unknown or revoked.
    Unauthorized,
    /// The agent has spent its request budget for the current window.
    RateLimited { retry_after_secs: u64 },
    /// No live row under that name for this agent. Another agent's row is indistinguishable
    /// from a missing one on purpose.
    NotFound { what: &'static str },
    /// The route exists but does not take this method.
    MethodNotAllowed,
    /// The request is well-formed HTTP but unacceptable: bad body, name, query or header.
    BadRequest { code: &'static str, message: String },
    /// The value as sent, or the request carrying it, is over the configured cap.
    PayloadTooLarge(String),
    /// The `If-Match` precondition did not hold. `current_version` is 0 when the row is absent.
    VersionConflict { current_version: i64 },
    /// Storage or crypto failed. The text is for the log only.
    Internal(String),
}

impl StateError {
    pub(super) fn invalid(message: impl Into<String>) -> Self {
        Self::BadRequest {
            code: "invalid_request",
            message: message.into(),
        }
    }

    /// Over a request budget, with `wait` left until it refills. `Retry-After` is whole seconds,
    /// so the wait is rounded up, and never 0, which would invite an immediate retry that fails.
    pub(super) fn rate_limited(wait: Duration) -> Self {
        let whole_secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
        Self::RateLimited {
            retry_after_secs: whole_secs.max(1),
        }
    }

    pub(super) fn invalid_cursor() -> Self {
        Self::BadRequest {
            code: "invalid_cursor",
            message: "cursor is not one this API issued".into(),
        }
    }
}

impl From<sqlx::Error> for StateError {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

impl IntoResponse for StateError {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthorized => error_response(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing, unknown or revoked agent token",
            ),
            Self::RateLimited { retry_after_secs } => {
                let mut response = error_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limited",
                    "rate limit exceeded, try again shortly",
                );
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_secs));
                response
            }
            Self::NotFound { what } => error_response(
                StatusCode::NOT_FOUND,
                "not_found",
                &format!("{what} not found"),
            ),
            Self::MethodNotAllowed => error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "method not allowed on this route",
            ),
            Self::BadRequest { code, message } => {
                error_response(StatusCode::BAD_REQUEST, code, &message)
            }
            Self::PayloadTooLarge(message) => {
                error_response(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large", &message)
            }
            // The agent SDK reads `error` and `current_version` (the contract's shape); `code`
            // is added so this body also satisfies the shared error convention.
            Self::VersionConflict { current_version } => (
                StatusCode::PRECONDITION_FAILED,
                Json(json!({
                    "error": "version_conflict",
                    "code": "version_conflict",
                    "current_version": current_version,
                })),
            )
                .into_response(),
            Self::Internal(detail) => {
                tracing::error!(error = %detail, "agent state request failed");
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "internal error",
                )
            }
        }
    }
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({ "error": message, "code": code }))).into_response()
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;

    use super::*;

    async fn parts_of(error: StateError) -> (StatusCode, HeaderMap, serde_json::Value) {
        let (parts, body) = error.into_response().into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        (
            parts.status,
            parts.headers,
            serde_json::from_slice(&bytes).unwrap(),
        )
    }

    #[tokio::test]
    async fn version_conflict_carries_the_contract_fields() {
        let (status, _, body) = parts_of(StateError::VersionConflict { current_version: 7 }).await;
        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        assert_eq!(body["error"], "version_conflict");
        assert_eq!(body["code"], "version_conflict");
        assert_eq!(body["current_version"], 7);
    }

    #[tokio::test]
    async fn rate_limited_names_when_to_retry() {
        let (status, headers, body) = parts_of(StateError::RateLimited {
            retry_after_secs: 60,
        })
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(headers[header::RETRY_AFTER], "60");
        assert_eq!(body["code"], "rate_limited");
    }

    #[test]
    fn retry_after_is_the_wait_rounded_up_to_a_whole_second_and_never_zero() {
        let secs = |wait: Duration| match StateError::rate_limited(wait) {
            StateError::RateLimited { retry_after_secs } => retry_after_secs,
            other => panic!("expected a rate limit, got {other:?}"),
        };
        assert_eq!(secs(Duration::from_secs(60)), 60);
        assert_eq!(secs(Duration::from_millis(59_001)), 60);
        assert_eq!(secs(Duration::from_millis(1_500)), 2);
        assert_eq!(secs(Duration::from_millis(1)), 1);
        assert_eq!(secs(Duration::ZERO), 1);
    }

    #[tokio::test]
    async fn internal_errors_never_leak_their_detail() {
        let (status, _, body) =
            parts_of(StateError::Internal("password=hunter2 at db:5432".into())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["code"], "internal_error");
        assert!(!body.to_string().contains("hunter2"));
    }

    #[tokio::test]
    async fn client_errors_use_the_shared_envelope() {
        let cases = [
            (
                StateError::Unauthorized,
                StatusCode::UNAUTHORIZED,
                "unauthorized",
            ),
            (
                StateError::NotFound { what: "task" },
                StatusCode::NOT_FOUND,
                "not_found",
            ),
            (
                StateError::MethodNotAllowed,
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
            ),
            (
                StateError::invalid("bad"),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                StateError::invalid_cursor(),
                StatusCode::BAD_REQUEST,
                "invalid_cursor",
            ),
            (
                StateError::PayloadTooLarge("big".into()),
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
            ),
        ];
        for (error, expected_status, expected_code) in cases {
            let (status, _, body) = parts_of(error).await;
            assert_eq!(status, expected_status);
            assert_eq!(body["code"], expected_code);
            assert!(body["error"].is_string());
        }
    }
}
