use serde::Serialize;
use sqlx::PgConnection;
use time::{Date, OffsetDateTime};

use crate::domain::{BatchId, PaymentId, Yen};

#[derive(Debug, Serialize)]
pub struct Payment {
    pub id: PaymentId,
    pub batch_id: BatchId,
    pub line_no: i32,
    #[serde(with = "crate::domain::iso_date")]
    pub paid_on: Date,
    pub amount: Yen,
    pub payer_name: String,
    pub payer_name_normalized: String,
    pub status: String,
    /// What the automatic matching decided at import time, and why.
    pub decision: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentStatus {
    Unmatched,
    Suggested,
    Matched,
}

impl PaymentStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unmatched => "unmatched",
            Self::Suggested => "suggested",
            Self::Matched => "matched",
        }
    }
}

pub struct ExistingBatch {
    pub id: BatchId,
    pub request_hash: Vec<u8>,
}

/// Serializes everything that changes which invoice is matched to which payment: imports, confirmations,
/// manual matches and cancellations. Held until the transaction ends.
///
/// One lock for the whole application is coarse, but it removes every ordering question between a batch that
/// reads a customer's open invoices and a confirmation that clears some of them. A lock per customer would be
/// the next step if the volume called for it.
///
/// The wait is bounded by the `lock_timeout` of the connection (set in `main`). A request that waits longer
/// fails with `lock_not_available`, which the API answers with 503 instead of holding a pooled connection for
/// as long as the queue lasts.
pub async fn lock_matching(conn: &mut PgConnection) -> sqlx::Result<()> {
    // The key is arbitrary; it only has to be the same everywhere.
    const MATCHING_LOCK_KEY: i64 = 0x7061_796d_6174_6368; // "paymatch"
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", MATCHING_LOCK_KEY)
        .fetch_one(conn)
        .await?;
    Ok(())
}

/// Records the batch unless its idempotency key was already used. Returns whether it was recorded.
pub async fn insert_batch(
    conn: &mut PgConnection,
    id: BatchId,
    idempotency_key: &str,
    request_hash: &[u8],
) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "INSERT INTO payment_batches (id, idempotency_key, request_hash) VALUES ($1, $2, $3)
         ON CONFLICT (idempotency_key) DO NOTHING",
        id.0,
        idempotency_key,
        request_hash,
    )
    .execute(conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn find_batch_by_key(
    conn: &mut PgConnection,
    idempotency_key: &str,
) -> sqlx::Result<Option<ExistingBatch>> {
    sqlx::query_as!(
        ExistingBatch,
        r#"SELECT id AS "id: BatchId", request_hash FROM payment_batches WHERE idempotency_key = $1"#,
        idempotency_key,
    )
    .fetch_optional(conn)
    .await
}

/// The oldest other batch with the same request content, if any.
pub async fn find_batch_by_hash(
    conn: &mut PgConnection,
    request_hash: &[u8],
    except: BatchId,
) -> sqlx::Result<Option<BatchId>> {
    sqlx::query_scalar!(
        r#"SELECT id AS "id: BatchId" FROM payment_batches
           WHERE request_hash = $1 AND id <> $2
           ORDER BY id
           LIMIT 1"#,
        request_hash,
        except.0,
    )
    .fetch_optional(conn)
    .await
}

pub struct NewPayment<'a> {
    pub id: PaymentId,
    pub batch_id: BatchId,
    pub line_no: i32,
    pub paid_on: Date,
    pub amount: Yen,
    pub payer_name_raw: &'a str,
    pub payer_name_normalized: &'a str,
    pub status: PaymentStatus,
    pub decision: serde_json::Value,
}

pub async fn insert(conn: &mut PgConnection, new: &NewPayment<'_>) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO payments
            (id, batch_id, line_no, paid_on, amount, payer_name_raw, payer_name_normalized, status, decision)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        new.id.0,
        new.batch_id.0,
        new.line_no,
        new.paid_on,
        new.amount.0,
        new.payer_name_raw,
        new.payer_name_normalized,
        new.status.as_str(),
        new.decision,
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn set_status(
    conn: &mut PgConnection,
    id: PaymentId,
    status: PaymentStatus,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE payments SET status = $2 WHERE id = $1",
        id.0,
        status.as_str()
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn find(conn: &mut PgConnection, id: PaymentId) -> sqlx::Result<Option<Payment>> {
    sqlx::query_as!(
        Payment,
        r#"SELECT id AS "id: PaymentId", batch_id AS "batch_id: BatchId", line_no, paid_on,
                  amount AS "amount: Yen", payer_name_raw AS payer_name, payer_name_normalized, status,
                  decision, created_at
           FROM payments WHERE id = $1"#,
        id.0,
    )
    .fetch_optional(conn)
    .await
}

pub async fn of_batch(conn: &mut PgConnection, batch_id: BatchId) -> sqlx::Result<Vec<Payment>> {
    sqlx::query_as!(
        Payment,
        r#"SELECT id AS "id: PaymentId", batch_id AS "batch_id: BatchId", line_no, paid_on,
                  amount AS "amount: Yen", payer_name_raw AS payer_name, payer_name_normalized, status,
                  decision, created_at
           FROM payments WHERE batch_id = $1 ORDER BY line_no"#,
        batch_id.0,
    )
    .fetch_all(conn)
    .await
}

pub struct ListFilter<'a> {
    pub status: Option<&'a str>,
    /// Return the payments after this one.
    pub after: Option<PaymentId>,
    pub limit: i64,
}

pub async fn list(conn: &mut PgConnection, filter: &ListFilter<'_>) -> sqlx::Result<Vec<Payment>> {
    sqlx::query_as!(
        Payment,
        r#"SELECT id AS "id: PaymentId", batch_id AS "batch_id: BatchId", line_no, paid_on,
                  amount AS "amount: Yen", payer_name_raw AS payer_name, payer_name_normalized, status,
                  decision, created_at
           FROM payments
           WHERE ($1::text IS NULL OR status = $1)
             AND ($2::uuid IS NULL OR id > $2)
           ORDER BY id
           LIMIT $3"#,
        filter.status,
        filter.after.map(|id| id.0),
        filter.limit,
    )
    .fetch_all(conn)
    .await
}
