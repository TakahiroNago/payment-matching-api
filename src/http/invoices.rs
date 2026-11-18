use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use time::Date;

use super::AppState;
use super::extract::{ApiJson, ApiPath, ApiQuery};
use super::pagination::{Page, page, page_size};
use crate::domain::{CustomerId, InvoiceId, Yen};
use crate::error::{AppError, FieldError, is_foreign_key_violation, is_unique_violation};
use crate::repo::invoices::{self, Invoice, ListFilter, NewInvoice};
use crate::repo::matches::{self, ClearedBy};
use crate::service::matching;

const MAX_NUMBER_CHARS: usize = 50;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInvoice {
    customer_id: CustomerId,
    invoice_number: String,
    amount: Yen,
    #[serde(with = "crate::domain::iso_date")]
    due_date: Date,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    status: Option<String>,
    customer_id: Option<CustomerId>,
    cursor: Option<InvoiceId>,
    limit: Option<u32>,
}

pub async fn create(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateInvoice>,
) -> Result<(StatusCode, Json<Invoice>), AppError> {
    let invoice_number = body.invoice_number.trim();
    let mut errors = Vec::new();
    if invoice_number.is_empty() {
        errors.push(FieldError::new("invoice_number", "required"));
    } else if invoice_number.chars().count() > MAX_NUMBER_CHARS {
        errors.push(FieldError::new("invoice_number", "too_long"));
    } else if invoice_number.chars().any(char::is_control) {
        errors.push(FieldError::new("invoice_number", "invalid_characters"));
    }
    if !body.amount.is_valid_amount() {
        errors.push(FieldError::new("amount", "out_of_range"));
    }
    if !errors.is_empty() {
        return Err(AppError::Validation(errors));
    }

    let mut conn = state.pool.acquire().await?;
    let new = NewInvoice {
        customer_id: body.customer_id,
        invoice_number,
        amount: body.amount,
        due_date: body.due_date,
    };
    let invoice = invoices::insert(&mut conn, &new).await.map_err(|error| {
        if is_unique_violation(&error, invoices::NUMBER_UNIQUE_CONSTRAINT) {
            AppError::conflict("the invoice number is already registered")
        } else if is_foreign_key_violation(&error, invoices::CUSTOMER_FOREIGN_KEY) {
            AppError::invalid("customer_id", "not_found")
        } else {
            error.into()
        }
    })?;
    Ok((StatusCode::CREATED, Json(invoice)))
}

/// An invoice with the match that cleared it: the answer to "which payment paid this?".
#[derive(Serialize)]
pub struct InvoiceDetail {
    #[serde(flatten)]
    invoice: Invoice,
    /// `null` unless the invoice is cleared.
    cleared_by: Option<ClearedBy>,
}

pub async fn get(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<InvoiceId>,
) -> Result<Json<InvoiceDetail>, AppError> {
    // One transaction, so that the status and the match are read from the same snapshot.
    let mut tx = state.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let invoice = invoices::find(&mut tx, id)
        .await?
        .ok_or(AppError::NotFound("invoice"))?;
    let cleared_by = matches::confirmed_for_invoice(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(InvoiceDetail {
        invoice,
        cleared_by,
    }))
}

pub async fn void(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<InvoiceId>,
) -> Result<Json<Invoice>, AppError> {
    let invoice = matching::void_invoice(&state.pool, id).await?;
    Ok(Json(invoice))
}

pub async fn list(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Page<Invoice>>, AppError> {
    let size = page_size(query.limit)?;
    if let Some(status) = query.status.as_deref()
        && status != invoices::STATUS_OPEN
        && status != invoices::STATUS_CLEARED
        && status != invoices::STATUS_VOID
    {
        return Err(AppError::invalid("status", "unknown_value"));
    }

    let mut conn = state.pool.acquire().await?;
    let filter = ListFilter {
        status: query.status.as_deref(),
        customer_id: query.customer_id,
        after: query.cursor,
        limit: i64::from(size) + 1,
    };
    let rows = invoices::list(&mut conn, &filter).await?;
    Ok(Json(page(rows, size, |invoice| invoice.id.0)))
}
