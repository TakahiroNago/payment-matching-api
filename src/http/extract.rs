//! Extractors that answer in the API's error format instead of axum's plain-text rejections.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::error::AppError;

/// A JSON request body.
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(json_error(&rejection)),
        }
    }
}

fn json_error(rejection: &JsonRejection) -> AppError {
    match rejection.status() {
        StatusCode::PAYLOAD_TOO_LARGE => AppError::PayloadTooLarge,
        StatusCode::UNSUPPORTED_MEDIA_TYPE => AppError::UnsupportedMediaType,
        // Well-formed JSON of the wrong shape: a missing or unknown field, a string where a number belongs.
        StatusCode::UNPROCESSABLE_ENTITY => AppError::Unprocessable(rejection.body_text()),
        _ => AppError::BadRequest(rejection.body_text()),
    }
}

/// Path parameters. A path that does not parse (an id that is not a UUID) names nothing: 404.
pub struct ApiPath<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiPath<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Path::<T>::from_request_parts(parts, state).await {
            Ok(Path(value)) => Ok(Self(value)),
            Err(_) => Err(AppError::NotFound("resource")),
        }
    }
}

/// Query parameters.
pub struct ApiQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(AppError::Unprocessable(rejection.body_text())),
        }
    }
}
