//! The error type of the API and its rendering as RFC 9457 problem details.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldError {
    /// Where the problem is, as a path into the request: `amount`, `payments[3].payer_name`.
    pub field: String,
    /// A stable, machine-readable reason: `required`, `out_of_range`, `not_found`, …
    pub code: &'static str,
}

impl FieldError {
    pub fn new(field: impl Into<String>, code: &'static str) -> Self {
        Self {
            field: field.into(),
            code,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The request was understood but its content is not acceptable.
    #[error("validation failed")]
    Validation(Vec<FieldError>),
    /// The request could not be read at all (malformed JSON, missing header).
    #[error("{0}")]
    BadRequest(String),
    /// The request is well-formed JSON (or a query string) of the wrong shape.
    #[error("{0}")]
    Unprocessable(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("{0} not found")]
    NotFound(&'static str),
    /// The request is valid but does not fit the current state.
    #[error("{0}")]
    Conflict(String),
    #[error("payload too large")]
    PayloadTooLarge,
    #[error("unsupported media type")]
    UnsupportedMediaType,
    #[error("database unavailable")]
    Unavailable,
    /// The server could not take the request now: the matching lock or a pooled connection was not free in
    /// time. Nothing was written; the same request can be sent again.
    #[error("busy")]
    Busy,
    /// The request ran longer than the server allows and was abandoned. Its transaction is rolled back unless
    /// the commit was already on its way, so the caller has to look before repeating a write (an import is
    /// safe to repeat under the same idempotency key).
    #[error("timed out")]
    Timeout,
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A broken invariant. Never the caller's fault.
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn invalid(field: impl Into<String>, code: &'static str) -> Self {
        Self::Validation(vec![FieldError::new(field, code)])
    }

    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::Conflict(detail.into())
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::Validation(_) | Self::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::Unavailable | Self::Busy | Self::Timeout => StatusCode::SERVICE_UNAVAILABLE,
            Self::Database(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::Validation(_) => "validation_failed",
            Self::BadRequest(_) => "bad_request",
            Self::Unprocessable(_) => "invalid_request",
            Self::Unauthorized => "unauthorized",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::PayloadTooLarge => "payload_too_large",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::Unavailable => "unavailable",
            Self::Busy => "busy",
            Self::Timeout => "timeout",
            Self::Database(_) | Self::Internal(_) => "internal_error",
        }
    }
}

/// A problem details body (RFC 9457). `type` is always `about:blank`, so `title` is the status phrase; `code`
/// is the member clients branch on.
pub fn problem(
    status: StatusCode,
    code: &str,
    detail: Option<String>,
    extra: serde_json::Value,
) -> Response {
    let mut body = json!({
        "type": "about:blank",
        "title": status.canonical_reason().unwrap_or("Error"),
        "status": status.as_u16(),
        "code": code,
    });
    if let Some(members) = body.as_object_mut() {
        if let Some(detail) = detail {
            members.insert("detail".to_owned(), detail.into());
        }
        if let serde_json::Value::Object(extra) = extra {
            members.extend(extra);
        }
    }
    let mut response = (status, body.to_string()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response
}

/// Whether the database turned the request away for lack of capacity, not because of the request: no pooled
/// connection became free in time, or a lock was not granted within `lock_timeout`.
fn is_overload(error: &sqlx::Error) -> bool {
    const LOCK_NOT_AVAILABLE: &str = "55P03";
    matches!(error, sqlx::Error::PoolTimedOut)
        || error
            .as_database_error()
            .is_some_and(|e| e.code().as_deref() == Some(LOCK_NOT_AVAILABLE))
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let this = match self {
            Self::Database(error) if is_overload(&error) => {
                tracing::warn!(%error, "request turned away");
                Self::Busy
            }
            other => other,
        };
        this.render()
    }
}

impl AppError {
    fn render(self) -> Response {
        let status = self.status();
        let code = self.code();
        match self {
            Self::Validation(errors) => problem(
                status,
                code,
                Some("The request content is not acceptable.".to_owned()),
                json!({ "errors": errors }),
            ),
            Self::Unauthorized => {
                let mut response = problem(status, code, None, json!({}));
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                response
            }
            // What went wrong inside stays in the log. The request-context middleware adds the request id to
            // the body, which is all the caller needs to report it.
            Self::Database(_) | Self::Internal(_) => {
                tracing::error!(error = %self, "request failed");
                problem(status, code, None, json!({}))
            }
            Self::Busy | Self::Timeout => {
                let detail = if matches!(self, Self::Busy) {
                    "The server is busy. Nothing was written; send the request again shortly."
                } else {
                    "The request took too long and was abandoned. Check the state before repeating a write."
                };
                let mut response = problem(status, code, Some(detail.to_owned()), json!({}));
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
                response
            }
            Self::Unavailable | Self::PayloadTooLarge | Self::UnsupportedMediaType => {
                problem(status, code, None, json!({}))
            }
            Self::BadRequest(detail) | Self::Unprocessable(detail) | Self::Conflict(detail) => {
                problem(status, code, Some(detail), json!({}))
            }
            Self::NotFound(what) => {
                problem(status, code, Some(format!("{what} not found")), json!({}))
            }
        }
    }
}

/// Whether `error` is a violation of the named unique constraint or index.
#[must_use]
pub fn is_unique_violation(error: &sqlx::Error, constraint: &str) -> bool {
    error
        .as_database_error()
        .is_some_and(|e| e.is_unique_violation() && e.constraint() == Some(constraint))
}

/// Whether `error` is a violation of the named foreign key.
#[must_use]
pub fn is_foreign_key_violation(error: &sqlx::Error, constraint: &str) -> bool {
    error
        .as_database_error()
        .is_some_and(|e| e.is_foreign_key_violation() && e.constraint() == Some(constraint))
}
