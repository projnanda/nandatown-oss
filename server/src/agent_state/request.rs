//! Extractors that check a request's inputs on the way in, so a handler only ever sees names and
//! bodies that are already acceptable, and a malformed one is a JSON 4xx like every other error.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use super::error::StateError;
use super::validation::{MAX_KEY_BYTES, MAX_TASK_ID_BYTES, check_name};

/// A task id taken from the path and validated.
pub(super) struct TaskId(String);

impl TaskId {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn into_string(self) -> String {
        self.0
    }
}

impl<S: Send + Sync> FromRequestParts<S> for TaskId {
    type Rejection = StateError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, StateError> {
        let raw = path_segment(parts, state).await?;
        check_name("task_id", &raw, MAX_TASK_ID_BYTES)?;
        Ok(Self(raw))
    }
}

/// A key-value key taken from the path and validated.
pub(super) struct KvKey(String);

impl KvKey {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn into_string(self) -> String {
        self.0
    }
}

impl<S: Send + Sync> FromRequestParts<S> for KvKey {
    type Rejection = StateError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, StateError> {
        let raw = path_segment(parts, state).await?;
        check_name("key", &raw, MAX_KEY_BYTES)?;
        Ok(Self(raw))
    }
}

/// The route's single path parameter, percent-decoded. A key containing `/` therefore has to be
/// sent as `%2F`: an unescaped slash starts a new path segment and matches no route.
async fn path_segment<S: Send + Sync>(parts: &mut Parts, state: &S) -> Result<String, StateError> {
    Path::<String>::from_request_parts(parts, state)
        .await
        .map(|Path(segment)| segment)
        .map_err(|rejection| StateError::invalid(format!("bad path: {}", rejection.body_text())))
}

/// `axum::Json` for a request body, with its failures reported in this API's error shape: 413
/// when the body is over the transport limit, 400 for everything else wrong with it (not JSON,
/// wrong content type, unknown or missing fields).
pub(super) struct StateJson<T>(pub(super) T);

impl<T, S> FromRequest<S> for StateJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = StateError;

    async fn from_request(request: Request, state: &S) -> Result<Self, StateError> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(body_error(&rejection)),
        }
    }
}

fn body_error(rejection: &JsonRejection) -> StateError {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return StateError::PayloadTooLarge("request body is over the size limit".into());
    }
    let message = match rejection {
        JsonRejection::MissingJsonContentType(_) => {
            "Content-Type must be application/json".to_string()
        }
        other => format!("invalid request body: {}", other.body_text()),
    };
    StateError::invalid(message)
}

/// `axum::extract::Query`, with its failures reported in this API's error shape.
pub(super) struct StateQuery<T>(pub(super) T);

impl<T, S> FromRequestParts<S> for StateQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = StateError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, StateError> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|rejection| {
                StateError::invalid(format!("invalid query string: {}", rejection.body_text()))
            })
    }
}
