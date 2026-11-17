use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::Instrument;
use uuid::Uuid;

use super::AppState;
use crate::error::{AppError, problem};

static REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// Gives every request an id, logs its outcome and returns the id in the `x-request-id` header. A 500
/// response gets a body that carries the id and nothing else about the failure.
///
/// Only the method, the path, the status and the duration are logged: no query string, no body, no payer
/// names or amounts.
pub async fn request_context(request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get(&REQUEST_ID)
        .and_then(|value| value.to_str().ok())
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 64
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        .map_or_else(|| Uuid::now_v7().to_string(), str::to_owned);

    let span = tracing::info_span!(
        "request",
        request_id = %request_id,
        method = %request.method(),
        path = %request.uri().path(),
    );
    let started = Instant::now();
    let mut response = next.run(request).instrument(span.clone()).await;

    if response.status() == StatusCode::INTERNAL_SERVER_ERROR {
        response = problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            Some(
                "An unexpected error occurred. Quote the request id when reporting it.".to_owned(),
            ),
            json!({ "request_id": request_id }),
        );
    }
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(REQUEST_ID.clone(), value);
    }

    span.in_scope(|| {
        tracing::info!(
            status = response.status().as_u16(),
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "request completed"
        );
    });
    response
}

/// Requires `Authorization: Bearer <API_TOKEN>`.
pub async fn require_bearer(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(AppError::Unauthorized)?;
    if !tokens_match(presented, &state.config.api_token) {
        return Err(AppError::Unauthorized);
    }
    Ok(next.run(request).await)
}

/// Compares the tokens without revealing, through timing, how much of the presented token is right. Hashing
/// first gives both sides the same length, so the length is not revealed either.
fn tokens_match(presented: &str, expected: &str) -> bool {
    let presented = Sha256::digest(presented.as_bytes());
    let expected = Sha256::digest(expected.as_bytes());
    presented.as_slice().ct_eq(expected.as_slice()).into()
}

#[cfg(test)]
mod tests {
    use super::tokens_match;

    #[test]
    fn tokens_match_only_when_equal() {
        assert!(tokens_match("0123456789abcdef", "0123456789abcdef"));
        assert!(!tokens_match("0123456789abcdeF", "0123456789abcdef"));
        assert!(!tokens_match("0123456789abcde", "0123456789abcdef"));
        assert!(!tokens_match("", "0123456789abcdef"));
    }
}
