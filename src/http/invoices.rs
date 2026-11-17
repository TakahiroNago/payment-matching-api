use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use time::Date;

use super::AppState;
use super::extract::{ApiJson, ApiQuery};
use super::pagination::{Page, page, page_size};
use crate::domain::{CustomerId, InvoiceId, Yen};
use crate::error::{AppError, FieldError, is_foreign_key_violation, is_unique_violation};
use crate::repo::invoices::{self, Invoice, ListFilter, NewInvoice};

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

pub async fn list(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Page<Invoice>>, AppError> {
    let size = page_size(query.limit)?;
    if let Some(status) = query.status.as_deref()
        && status != invoices::STATUS_OPEN
        && status != invoices::STATUS_CLEARED
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
