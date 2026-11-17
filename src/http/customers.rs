use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::extract::{ApiJson, ApiPath};
use crate::domain::CustomerId;
use crate::domain::normalize::normalize_payer_name;
use crate::error::{AppError, FieldError, is_unique_violation};
use crate::repo::customers::{self, ALIAS_UNIQUE_CONSTRAINT, Alias, AliasSource, Customer};

const MAX_NAME_CHARS: usize = 200;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCustomer {
    name: String,
    /// The reading of the name in kana, as the customer writes it on bank transfers.
    name_kana: String,
    /// Whether the customer pays the invoice amount less the transfer fee (enables rule R3).
    #[serde(default)]
    fee_deducted: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAlias {
    alias: String,
}

#[derive(Serialize)]
pub struct CustomerBody {
    #[serde(flatten)]
    customer: Customer,
    aliases: Vec<Alias>,
}

pub async fn create(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateCustomer>,
) -> Result<(StatusCode, Json<CustomerBody>), AppError> {
    let name = body.name.trim();
    let name_kana = body.name_kana.trim();
    let mut errors = Vec::new();
    if let Some(code) = text_problem(name) {
        errors.push(FieldError::new("name", code));
    }
    if let Some(code) = text_problem(name_kana) {
        errors.push(FieldError::new("name_kana", code));
    }
    let normalized = normalize_payer_name(name_kana);
    if errors.is_empty() && normalized.is_none() {
        errors.push(FieldError::new("name_kana", "not_normalizable"));
    }
    let Some(normalized) = normalized.filter(|_| errors.is_empty()) else {
        return Err(AppError::Validation(errors));
    };

    let mut tx = state.pool.begin().await?;
    let customer = customers::insert(&mut tx, name, name_kana, body.fee_deducted).await?;
    let alias = customers::insert_alias(
        &mut tx,
        customer.id,
        name_kana,
        &normalized,
        AliasSource::Kana,
    )
    .await
    .map_err(alias_conflict)?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(CustomerBody {
            customer,
            aliases: vec![alias],
        }),
    ))
}

pub async fn get(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<CustomerId>,
) -> Result<Json<CustomerBody>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let customer = customers::find(&mut conn, id)
        .await?
        .ok_or(AppError::NotFound("customer"))?;
    let aliases = customers::aliases_of(&mut conn, id).await?;
    Ok(Json(CustomerBody { customer, aliases }))
}

pub async fn add_alias(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<CustomerId>,
    ApiJson(body): ApiJson<CreateAlias>,
) -> Result<(StatusCode, Json<Alias>), AppError> {
    let raw = body.alias.trim();
    if let Some(code) = text_problem(raw) {
        return Err(AppError::invalid("alias", code));
    }
    let normalized =
        normalize_payer_name(raw).ok_or_else(|| AppError::invalid("alias", "not_normalizable"))?;

    let mut conn = state.pool.acquire().await?;
    customers::find(&mut conn, id)
        .await?
        .ok_or(AppError::NotFound("customer"))?;
    let alias = customers::insert_alias(&mut conn, id, raw, &normalized, AliasSource::Manual)
        .await
        .map_err(alias_conflict)?;
    Ok((StatusCode::CREATED, Json(alias)))
}

fn text_problem(value: &str) -> Option<&'static str> {
    if value.is_empty() {
        Some("required")
    } else if value.chars().count() > MAX_NAME_CHARS {
        Some("too_long")
    } else if value.chars().any(char::is_control) {
        Some("invalid_characters")
    } else {
        None
    }
}

fn alias_conflict(error: sqlx::Error) -> AppError {
    if is_unique_violation(&error, ALIAS_UNIQUE_CONSTRAINT) {
        AppError::conflict("the normalized name already identifies a customer")
    } else {
        error.into()
    }
}
