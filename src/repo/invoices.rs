use serde::Serialize;
use sqlx::PgConnection;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::domain::matching::OpenInvoice;
use crate::domain::{CustomerId, InvoiceId, Yen};

#[derive(Debug, Serialize)]
pub struct Invoice {
    pub id: InvoiceId,
    pub customer_id: CustomerId,
    pub invoice_number: String,
    pub amount: Yen,
    #[serde(with = "crate::domain::iso_date")]
    pub due_date: Date,
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

pub const STATUS_OPEN: &str = "open";
pub const STATUS_CLEARED: &str = "cleared";
pub const NUMBER_UNIQUE_CONSTRAINT: &str = "invoices_number_key";
pub const CUSTOMER_FOREIGN_KEY: &str = "invoices_customer_fkey";

pub struct NewInvoice<'a> {
    pub customer_id: CustomerId,
    pub invoice_number: &'a str,
    pub amount: Yen,
    pub due_date: Date,
}

/// Fails with a violation of [`NUMBER_UNIQUE_CONSTRAINT`] or [`CUSTOMER_FOREIGN_KEY`].
pub async fn insert(conn: &mut PgConnection, new: &NewInvoice<'_>) -> sqlx::Result<Invoice> {
    sqlx::query_as!(
        Invoice,
        r#"INSERT INTO invoices (id, customer_id, invoice_number, amount, due_date)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id AS "id: InvoiceId", customer_id AS "customer_id: CustomerId", invoice_number,
                     amount AS "amount: Yen", due_date, status, created_at"#,
        InvoiceId::generate().0,
        new.customer_id.0,
        new.invoice_number,
        new.amount.0,
        new.due_date,
    )
    .fetch_one(conn)
    .await
}

pub async fn find(conn: &mut PgConnection, id: InvoiceId) -> sqlx::Result<Option<Invoice>> {
    sqlx::query_as!(
        Invoice,
        r#"SELECT id AS "id: InvoiceId", customer_id AS "customer_id: CustomerId", invoice_number,
                  amount AS "amount: Yen", due_date, status, created_at
           FROM invoices WHERE id = $1"#,
        id.0,
    )
    .fetch_optional(conn)
    .await
}

pub struct ListFilter<'a> {
    pub status: Option<&'a str>,
    pub customer_id: Option<CustomerId>,
    /// Return the invoices after this one.
    pub after: Option<InvoiceId>,
    pub limit: i64,
}

pub async fn list(conn: &mut PgConnection, filter: &ListFilter<'_>) -> sqlx::Result<Vec<Invoice>> {
    sqlx::query_as!(
        Invoice,
        r#"SELECT id AS "id: InvoiceId", customer_id AS "customer_id: CustomerId", invoice_number,
                  amount AS "amount: Yen", due_date, status, created_at
           FROM invoices
           WHERE ($1::text IS NULL OR status = $1)
             AND ($2::uuid IS NULL OR customer_id = $2)
             AND ($3::uuid IS NULL OR id > $3)
           ORDER BY id
           LIMIT $4"#,
        filter.status,
        filter.customer_id.map(|id| id.0),
        filter.after.map(|id| id.0),
        filter.limit,
    )
    .fetch_all(conn)
    .await
}

pub async fn open_of_customer(
    conn: &mut PgConnection,
    customer_id: CustomerId,
) -> sqlx::Result<Vec<OpenInvoice>> {
    sqlx::query_as!(
        OpenInvoice,
        r#"SELECT id AS "id: InvoiceId", amount AS "amount: Yen", due_date
           FROM invoices WHERE customer_id = $1 AND status = 'open'
           ORDER BY due_date, id"#,
        customer_id.0,
    )
    .fetch_all(conn)
    .await
}

/// The open invoices of these customers, each with its customer, oldest first.
pub async fn open_of_customers(
    conn: &mut PgConnection,
    customer_ids: &[CustomerId],
) -> sqlx::Result<Vec<(CustomerId, OpenInvoice)>> {
    let ids: Vec<Uuid> = customer_ids.iter().map(|id| id.0).collect();
    let rows = sqlx::query!(
        r#"SELECT customer_id AS "customer_id: CustomerId", id AS "id: InvoiceId",
                  amount AS "amount: Yen", due_date
           FROM invoices WHERE customer_id = ANY($1) AND status = 'open'
           ORDER BY due_date, id"#,
        &ids,
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let invoice = OpenInvoice {
                id: row.id,
                amount: row.amount,
                due_date: row.due_date,
            };
            (row.customer_id, invoice)
        })
        .collect())
}

/// The oldest open invoices of any customer with exactly this amount.
pub async fn open_with_amount(
    conn: &mut PgConnection,
    amount: Yen,
    limit: i64,
) -> sqlx::Result<Vec<OpenInvoice>> {
    sqlx::query_as!(
        OpenInvoice,
        r#"SELECT id AS "id: InvoiceId", amount AS "amount: Yen", due_date
           FROM invoices WHERE amount = $1 AND status = 'open'
           ORDER BY due_date, id
           LIMIT $2"#,
        amount.0,
        limit,
    )
    .fetch_all(conn)
    .await
}

/// The invoices with these ids, oldest first, locked until the end of the transaction. Ids that do not exist
/// are simply missing from the result.
pub async fn lock_many(conn: &mut PgConnection, ids: &[InvoiceId]) -> sqlx::Result<Vec<Invoice>> {
    sqlx::query_as!(
        Invoice,
        r#"SELECT id AS "id: InvoiceId", customer_id AS "customer_id: CustomerId", invoice_number,
                  amount AS "amount: Yen", due_date, status, created_at
           FROM invoices WHERE id = ANY($1)
           ORDER BY due_date, id
           FOR UPDATE"#,
        &raw(ids),
    )
    .fetch_all(conn)
    .await
}

/// Marks the invoices open again and returns how many were cleared.
pub async fn reopen(conn: &mut PgConnection, ids: &[InvoiceId]) -> sqlx::Result<u64> {
    let done = sqlx::query!(
        "UPDATE invoices SET status = 'open' WHERE id = ANY($1) AND status = 'cleared'",
        &raw(ids),
    )
    .execute(conn)
    .await?;
    Ok(done.rows_affected())
}

fn raw(ids: &[InvoiceId]) -> Vec<Uuid> {
    ids.iter().map(|id| id.0).collect()
}
