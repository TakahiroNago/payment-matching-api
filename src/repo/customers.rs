use serde::Serialize;
use sqlx::PgConnection;
use time::OffsetDateTime;

use crate::domain::{AliasId, CustomerId};

#[derive(Debug, Serialize)]
pub struct Customer {
    pub id: CustomerId,
    pub name: String,
    pub name_kana: String,
    pub fee_deducted: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
pub struct Alias {
    pub id: AliasId,
    pub alias: String,
    pub normalized: String,
    pub source: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Where an alias comes from (`customer_aliases.source`).
#[derive(Debug, Clone, Copy)]
pub enum AliasSource {
    /// The customer's own kana name.
    Kana,
    Manual,
    /// Registered when a match was confirmed.
    Learned,
}

impl AliasSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Kana => "kana",
            Self::Manual => "manual",
            Self::Learned => "learned",
        }
    }
}

/// The customer a normalized payer name belongs to.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedPayer {
    pub customer_id: CustomerId,
    pub fee_deducted: bool,
}

pub const ALIAS_UNIQUE_CONSTRAINT: &str = "customer_aliases_normalized_key";

pub async fn insert(
    conn: &mut PgConnection,
    name: &str,
    name_kana: &str,
    fee_deducted: bool,
) -> sqlx::Result<Customer> {
    sqlx::query_as!(
        Customer,
        r#"INSERT INTO customers (id, name, name_kana, fee_deducted)
           VALUES ($1, $2, $3, $4)
           RETURNING id AS "id: CustomerId", name, name_kana, fee_deducted, created_at"#,
        CustomerId::generate().0,
        name,
        name_kana,
        fee_deducted,
    )
    .fetch_one(conn)
    .await
}

pub async fn find(conn: &mut PgConnection, id: CustomerId) -> sqlx::Result<Option<Customer>> {
    sqlx::query_as!(
        Customer,
        r#"SELECT id AS "id: CustomerId", name, name_kana, fee_deducted, created_at
           FROM customers WHERE id = $1"#,
        id.0,
    )
    .fetch_optional(conn)
    .await
}

/// Fails with a violation of [`ALIAS_UNIQUE_CONSTRAINT`] when the normalized name is already registered.
pub async fn insert_alias(
    conn: &mut PgConnection,
    customer_id: CustomerId,
    raw: &str,
    normalized: &str,
    source: AliasSource,
) -> sqlx::Result<Alias> {
    sqlx::query_as!(
        Alias,
        r#"INSERT INTO customer_aliases (id, customer_id, alias_raw, alias_normalized, source)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id AS "id: AliasId", alias_raw AS alias, alias_normalized AS normalized, source, created_at"#,
        AliasId::generate().0,
        customer_id.0,
        raw,
        normalized,
        source.as_str(),
    )
    .fetch_one(conn)
    .await
}

pub async fn aliases_of(
    conn: &mut PgConnection,
    customer_id: CustomerId,
) -> sqlx::Result<Vec<Alias>> {
    sqlx::query_as!(
        Alias,
        r#"SELECT id AS "id: AliasId", alias_raw AS alias, alias_normalized AS normalized, source, created_at
           FROM customer_aliases WHERE customer_id = $1 ORDER BY id"#,
        customer_id.0,
    )
    .fetch_all(conn)
    .await
}

pub async fn resolve_payer(
    conn: &mut PgConnection,
    normalized: &str,
) -> sqlx::Result<Option<ResolvedPayer>> {
    sqlx::query_as!(
        ResolvedPayer,
        r#"SELECT c.id AS "customer_id: CustomerId", c.fee_deducted
           FROM customer_aliases a JOIN customers c ON c.id = a.customer_id
           WHERE a.alias_normalized = $1"#,
        normalized,
    )
    .fetch_optional(conn)
    .await
}

/// A normalized payer name and the customer it belongs to.
pub struct ResolvedName {
    pub normalized: String,
    pub payer: ResolvedPayer,
}

/// The customers behind these normalized payer names. Names that belong to no customer are missing from the
/// result.
pub async fn resolve_payers(
    conn: &mut PgConnection,
    normalized: &[String],
) -> sqlx::Result<Vec<ResolvedName>> {
    let rows = sqlx::query!(
        r#"SELECT a.alias_normalized, c.id AS "customer_id: CustomerId", c.fee_deducted
           FROM customer_aliases a JOIN customers c ON c.id = a.customer_id
           WHERE a.alias_normalized = ANY($1)"#,
        normalized,
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ResolvedName {
            normalized: row.alias_normalized,
            payer: ResolvedPayer {
                customer_id: row.customer_id,
                fee_deducted: row.fee_deducted,
            },
        })
        .collect())
}
