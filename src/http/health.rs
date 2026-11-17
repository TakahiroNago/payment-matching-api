use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use super::AppState;
use crate::error::AppError;

/// The process is running.
pub async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// The process can reach the database.
pub async fn readyz(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    match sqlx::query!("SELECT 1 AS one").fetch_one(&state.pool).await {
        Ok(_) => Ok(Json(json!({ "status": "ok" }))),
        Err(error) => {
            tracing::warn!(%error, "readiness check failed");
            Err(AppError::Unavailable)
        }
    }
}
