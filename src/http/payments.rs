use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::Date;

use super::AppState;
use super::extract::{ApiJson, ApiPath, ApiQuery};
use super::pagination::{Page, page, page_size};
use crate::domain::{BatchId, PaymentId, Yen};
use crate::error::{AppError, FieldError};
use crate::repo::payments::{self, ListFilter, Payment, PaymentStatus};
use crate::service::import::{ImportLine, import as import_batch};
use crate::service::rematch::rematch as rematch_unmatched;
use crate::service::{PaymentDetail, payment_detail};

const MAX_LINES: usize = 1_000;
const MAX_PAYER_NAME_CHARS: usize = 200;
const MAX_KEY_LENGTH: usize = 100;
/// A request with many bad lines reports the first of them.
const MAX_REPORTED_ERRORS: usize = 50;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    payments: Vec<PaymentLine>,
    /// Import the payments although the same payments were already imported under another idempotency key.
    /// Not part of the request hash: it does not change what the request imports.
    #[serde(default, skip_serializing)]
    allow_duplicate: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentLine {
    #[serde(with = "crate::domain::iso_date")]
    paid_on: Date,
    amount: Yen,
    /// The payer as printed on the statement. May be empty; such a line can only be matched by amount.
    payer_name: String,
}

#[derive(Serialize)]
pub struct ImportResponse {
    batch_id: BatchId,
    /// `true` when this idempotency key had already been used with the same request and nothing was written.
    replayed: bool,
    summary: Summary,
    /// The payments of the batch as they are now, in line order.
    payments: Vec<Payment>,
}

#[derive(Default, Serialize)]
pub struct Summary {
    total: usize,
    matched: usize,
    suggested: usize,
    unmatched: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RematchQuery {
    cursor: Option<PaymentId>,
    limit: Option<u32>,
}

#[derive(Serialize)]
pub struct RematchResponse {
    /// `total` is the number of unmatched payments looked at; the rest is what became of them.
    summary: Summary,
    /// The payments that are no longer unmatched, as they are now, oldest first.
    items: Vec<Payment>,
    /// Pass as `cursor` to look at the unmatched payments that follow; `null` when there are none.
    next_cursor: Option<PaymentId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    status: Option<String>,
    cursor: Option<PaymentId>,
    limit: Option<u32>,
}

pub async fn import(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<ImportRequest>,
) -> Result<(StatusCode, Json<ImportResponse>), AppError> {
    let key = idempotency_key(&headers)?;
    validate(&body)?;

    // The hash is taken over the parsed request written back out, so that the same payments in another
    // layout (whitespace, key order) count as the same request.
    let allow_duplicate = body.allow_duplicate;
    let canonical = serde_json::to_vec(&body)
        .map_err(|e| AppError::Internal(format!("request is not serializable: {e}")))?;
    let request_hash = Sha256::digest(&canonical);

    let lines: Vec<ImportLine> = body
        .payments
        .into_iter()
        .map(|line| ImportLine {
            paid_on: line.paid_on,
            amount: line.amount,
            payer_name: line.payer_name,
        })
        .collect();
    let outcome = import_batch(
        &state.pool,
        &state.config.matching,
        key,
        request_hash.as_slice(),
        allow_duplicate,
        &lines,
    )
    .await?;

    let summary = summarize(&outcome.payments);
    let status = if outcome.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((
        status,
        Json(ImportResponse {
            batch_id: outcome.batch_id,
            replayed: outcome.replayed,
            summary,
            payments: outcome.payments,
        }),
    ))
}

fn summarize(payments: &[Payment]) -> Summary {
    let mut summary = Summary {
        total: payments.len(),
        ..Summary::default()
    };
    for payment in payments {
        match payment.status.as_str() {
            "matched" => summary.matched += 1,
            "suggested" => summary.suggested += 1,
            _ => summary.unmatched += 1,
        }
    }
    summary
}

fn idempotency_key(headers: &HeaderMap) -> Result<&str, AppError> {
    let value = headers
        .get("idempotency-key")
        .ok_or_else(|| AppError::BadRequest("the Idempotency-Key header is required".to_owned()))?;
    value
        .to_str()
        .ok()
        .filter(|key| {
            !key.is_empty()
                && key.len() <= MAX_KEY_LENGTH
                && key.bytes().all(|b| b.is_ascii_graphic())
        })
        .ok_or_else(|| AppError::invalid("Idempotency-Key", "invalid"))
}

fn validate(body: &ImportRequest) -> Result<(), AppError> {
    if body.payments.is_empty() || body.payments.len() > MAX_LINES {
        return Err(AppError::invalid("payments", "out_of_range"));
    }
    let mut errors = Vec::new();
    for (index, line) in body.payments.iter().enumerate() {
        if !line.amount.is_valid_amount() {
            errors.push(FieldError::new(
                format!("payments[{index}].amount"),
                "out_of_range",
            ));
        }
        if line.payer_name.chars().count() > MAX_PAYER_NAME_CHARS {
            errors.push(FieldError::new(
                format!("payments[{index}].payer_name"),
                "too_long",
            ));
        } else if line.payer_name.chars().any(char::is_control) {
            errors.push(FieldError::new(
                format!("payments[{index}].payer_name"),
                "invalid_characters",
            ));
        }
        if errors.len() >= MAX_REPORTED_ERRORS {
            break;
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(AppError::Validation(errors))
    }
}

/// Decides again about unmatched payments, a page at a time.
pub async fn rematch(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<RematchQuery>,
) -> Result<Json<RematchResponse>, AppError> {
    let size = page_size(query.limit)?;
    let outcome =
        rematch_unmatched(&state.pool, &state.config.matching, query.cursor, size).await?;

    let summary = summarize(&outcome.examined);
    let unmatched = PaymentStatus::Unmatched.as_str();
    Ok(Json(RematchResponse {
        summary,
        items: outcome
            .examined
            .into_iter()
            .filter(|payment| payment.status != unmatched)
            .collect(),
        next_cursor: outcome.next,
    }))
}

pub async fn list(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Page<Payment>>, AppError> {
    let size = page_size(query.limit)?;
    if let Some(status) = query.status.as_deref() {
        let known = [
            PaymentStatus::Unmatched,
            PaymentStatus::Suggested,
            PaymentStatus::Matched,
        ];
        if !known.iter().any(|s| s.as_str() == status) {
            return Err(AppError::invalid("status", "unknown_value"));
        }
    }

    let mut conn = state.pool.acquire().await?;
    let filter = ListFilter {
        status: query.status.as_deref(),
        after: query.cursor,
        limit: i64::from(size) + 1,
    };
    let rows = payments::list(&mut conn, &filter).await?;
    Ok(Json(page(rows, size, |payment| payment.id.0)))
}

pub async fn get(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<PaymentId>,
) -> Result<Json<PaymentDetail>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let detail = payment_detail(&mut conn, id)
        .await?
        .ok_or(AppError::NotFound("payment"))?;
    Ok(Json(detail))
}
