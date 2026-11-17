use std::any::Any;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::Instrument;
use uuid::Uuid;

use super::AppState;
use crate::error::{AppError, problem};

static REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// How long a request may run. The largest import takes a fraction of this; a request that is still running
/// is stuck on something (the database, mostly) and is dropped, which rolls its transaction back.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Gives every request an id, logs its outcome and returns the id in the `x-request-id` header. A 500
/// response gets a body that carries the id and nothing else about the failure. A request that runs longer
/// than [`REQUEST_TIMEOUT`] is answered with 503.
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
    let handled = tokio::time::timeout(REQUEST_TIMEOUT, next.run(request).instrument(span.clone()));
    let mut response = match handled.await {
        Ok(response) => response,
        Err(_) => AppError::Timeout.into_response(),
    };

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

/// The response to a handler that panicked: a 500 like any other (the request-context middleware gives it its
/// body), with the panic message in the log. Without this the connection would just close.
pub fn panic_response(panic: Box<dyn Any + Send + 'static>) -> Response {
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("a panic without a message");
    tracing::error!(panic = message, "request handler panicked");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
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
    #![allow(clippy::unwrap_used)]

    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use tower_http::catch_panic::CatchPanicLayer;

    use super::{panic_response, request_context, tokens_match};

    async fn get_from(app: Router, path: &str) -> (StatusCode, serde_json::Value) {
        let request = Request::builder().uri(path).body(Body::empty()).unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// The layers of the real router around handlers that misbehave.
    fn misbehaving() -> Router {
        Router::new()
            .route(
                "/panic",
                get(|| async {
                    panic!("boom");
                    #[allow(unreachable_code)]
                    ()
                }),
            )
            .route(
                "/slow",
                get(|| async { tokio::time::sleep(Duration::from_secs(3600)).await }),
            )
            .layer(CatchPanicLayer::custom(panic_response))
            .layer(axum::middleware::from_fn(request_context))
    }

    #[tokio::test]
    async fn a_panic_is_an_internal_error_with_a_request_id() {
        let (status, body) = get_from(misbehaving(), "/panic").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["code"], "internal_error");
        assert!(body["request_id"].is_string());
        assert!(!body.to_string().contains("boom"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_that_runs_too_long_is_dropped() {
        let (status, body) = get_from(misbehaving(), "/slow").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "timeout");
    }

    #[test]
    fn tokens_match_only_when_equal() {
        assert!(tokens_match("0123456789abcdef", "0123456789abcdef"));
        assert!(!tokens_match("0123456789abcdeF", "0123456789abcdef"));
        assert!(!tokens_match("0123456789abcde", "0123456789abcdef"));
        assert!(!tokens_match("", "0123456789abcdef"));
    }
}
