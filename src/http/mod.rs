//! The HTTP interface: routing, authentication, request validation and the error format.

mod customers;
mod extract;
mod health;
mod invoices;
mod matches;
mod middleware;
mod pagination;
mod payments;

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post};
use sqlx::PgPool;
use tower_http::catch_panic::CatchPanicLayer;

use crate::config::Config;
use crate::error::AppError;

/// The largest request body accepted (1 MiB). A batch of 1,000 payments is well below it.
const MAX_BODY_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
}

pub fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/customers", post(customers::create))
        .route("/customers/{id}", get(customers::get))
        .route("/customers/{id}/aliases", post(customers::add_alias))
        .route(
            "/customers/{id}/aliases/{alias_id}",
            delete(customers::remove_alias),
        )
        .route("/invoices", post(invoices::create).get(invoices::list))
        .route("/invoices/{id}", get(invoices::get))
        .route("/invoices/{id}/void", post(invoices::void))
        .route("/payment-batches", post(payments::import))
        .route("/payments", get(payments::list))
        .route("/payments/rematch", post(payments::rematch))
        .route("/payments/{id}", get(payments::get))
        .route("/payments/{id}/matches", post(matches::create_manual))
        .route("/matches/{id}/confirm", post(matches::confirm))
        .route("/matches/{id}/cancel", post(matches::cancel))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::require_bearer,
        ));

    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .nest("/v1", v1)
        .fallback(|| async { AppError::NotFound("resource") })
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(CatchPanicLayer::custom(middleware::panic_response))
        .layer(axum::middleware::from_fn(middleware::request_context))
        .with_state(state)
}
