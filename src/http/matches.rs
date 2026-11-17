use std::collections::HashSet;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;

use super::AppState;
use super::extract::{ApiJson, ApiPath};
use crate::domain::{InvoiceId, MatchId, PaymentId, Yen};
use crate::error::AppError;
use crate::service::PaymentDetail;
use crate::service::matching::{self, ManualMatch};

/// A person picks invoices one by one; more than this is not a realistic request.
const MAX_MANUAL_INVOICES: usize = 50;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmRequest {
    /// Also register the payer name of the payment as an alias of the customer.
    #[serde(default)]
    register_alias: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualMatchRequest {
    invoice_ids: Vec<InvoiceId>,
    #[serde(default = "zero")]
    fee_amount: Yen,
    #[serde(default)]
    register_alias: bool,
}

fn zero() -> Yen {
    Yen::ZERO
}

/// The body is optional: an empty request confirms without registering an alias.
pub async fn confirm(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<MatchId>,
    body: Bytes,
) -> Result<Json<PaymentDetail>, AppError> {
    let request = if body.is_empty() {
        ConfirmRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            if e.is_data() {
                AppError::Unprocessable(e.to_string())
            } else {
                AppError::BadRequest(e.to_string())
            }
        })?
    };
    let detail = matching::confirm(&state.pool, id, request.register_alias).await?;
    Ok(Json(detail))
}

pub async fn create_manual(
    State(state): State<AppState>,
    ApiPath(payment_id): ApiPath<PaymentId>,
    ApiJson(body): ApiJson<ManualMatchRequest>,
) -> Result<(StatusCode, Json<PaymentDetail>), AppError> {
    if body.invoice_ids.is_empty() || body.invoice_ids.len() > MAX_MANUAL_INVOICES {
        return Err(AppError::invalid("invoice_ids", "out_of_range"));
    }
    if body.invoice_ids.iter().collect::<HashSet<_>>().len() != body.invoice_ids.len() {
        return Err(AppError::invalid("invoice_ids", "duplicate"));
    }
    if body.fee_amount < Yen::ZERO || body.fee_amount > Yen::MAX {
        return Err(AppError::invalid("fee_amount", "out_of_range"));
    }

    let input = ManualMatch {
        invoice_ids: body.invoice_ids,
        fee_amount: body.fee_amount,
        register_alias: body.register_alias,
    };
    let detail = matching::match_manually(&state.pool, payment_id, &input).await?;
    Ok((StatusCode::CREATED, Json(detail)))
}

pub async fn cancel(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<MatchId>,
) -> Result<Json<PaymentDetail>, AppError> {
    let detail = matching::cancel(&state.pool, id).await?;
    Ok(Json(detail))
}
