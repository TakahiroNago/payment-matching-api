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
    /// What the automatic matching decided, and why: at import, or when the payment was last decided again.
    pub decision: serde_json::Value,
    /// When `decision` was made.
    #[serde(with = "time::serde::rfc3339")]
    pub decided_at: OffsetDateTime,
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
    pub line_no: i32,
    pub paid_on: Date,
    pub amount: Yen,
    pub payer_name_raw: &'a str,
    pub payer_name_normalized: &'a str,
}

/// Inserts the payments of a batch in one statement, as unmatched and without a decision. The caller decides
/// about each of them and calls [`set_decisions`] before the transaction ends.
pub async fn insert_many(
    conn: &mut PgConnection,
    batch_id: BatchId,
    new: &[NewPayment<'_>],
) -> sqlx::Result<()> {
    let ids: Vec<uuid::Uuid> = new.iter().map(|p| p.id.0).collect();
    let line_nos: Vec<i32> = new.iter().map(|p| p.line_no).collect();
    let paid_on: Vec<Date> = new.iter().map(|p| p.paid_on).collect();
    let amounts: Vec<i64> = new.iter().map(|p| p.amount.0).collect();
    let raw: Vec<String> = new.iter().map(|p| p.payer_name_raw.to_owned()).collect();
    let normalized: Vec<String> = new
        .iter()
        .map(|p| p.payer_name_normalized.to_owned())
        .collect();
    sqlx::query!(
        "INSERT INTO payments
            (id, batch_id, line_no, paid_on, amount, payer_name_raw, payer_name_normalized, status, decision)
         SELECT id, $1, line_no, paid_on, amount, raw, normalized, 'unmatched', 'null'::jsonb
         FROM UNNEST($2::uuid[], $3::int4[], $4::date[], $5::bigint[], $6::text[], $7::text[])
              AS line (id, line_no, paid_on, amount, raw, normalized)",
        batch_id.0,
        &ids,
        &line_nos,
        &paid_on,
        &amounts,
        &raw,
        &normalized,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The automatic decision about a payment.
pub struct Decided {
    pub id: PaymentId,
    /// Empty when the payer name could not be normalized.
    pub payer_name_normalized: String,
    pub decision: serde_json::Value,
}

/// Stores the automatic decisions about these payments, in one statement.
///
/// The status is not taken from the decision but from the match the payment has now: a suggestion recorded
/// for one payment may have been cancelled since, by a later payment that cleared its invoice.
pub async fn set_decisions(conn: &mut PgConnection, decided: &[Decided]) -> sqlx::Result<()> {
    let ids: Vec<uuid::Uuid> = decided.iter().map(|d| d.id.0).collect();
    let normalized: Vec<String> = decided
        .iter()
        .map(|d| d.payer_name_normalized.clone())
        .collect();
    let decisions: Vec<serde_json::Value> = decided.iter().map(|d| d.decision.clone()).collect();
    sqlx::query!(
        "UPDATE payments p
         SET payer_name_normalized = d.normalized,
             decision = d.decision,
             decided_at = now(),
             status = COALESCE(
                 (SELECT CASE m.state WHEN 'confirmed' THEN 'matched' ELSE 'suggested' END
                  FROM matches m
                  WHERE m.payment_id = p.id AND m.state <> 'cancelled'),
                 'unmatched')
         FROM UNNEST($1::uuid[], $2::text[], $3::jsonb[]) AS d (id, normalized, decision)
         WHERE p.id = d.id",
        &ids,
        &normalized,
        &decisions,
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

/// Unmatched payments after `after`, oldest first.
pub async fn unmatched_after(
    conn: &mut PgConnection,
    after: Option<PaymentId>,
    limit: i64,
) -> sqlx::Result<Vec<Payment>> {
    sqlx::query_as!(
        Payment,
        r#"SELECT id AS "id: PaymentId", batch_id AS "batch_id: BatchId", line_no, paid_on,
                  amount AS "amount: Yen", payer_name_raw AS payer_name, payer_name_normalized, status,
                  decision, decided_at, created_at
           FROM payments
           WHERE status = 'unmatched' AND ($1::uuid IS NULL OR id > $1)
           ORDER BY id
           LIMIT $2"#,
        after.map(|id| id.0),
        limit,
    )
    .fetch_all(conn)
    .await
}

/// The payments with these ids, oldest first.
pub async fn by_ids(conn: &mut PgConnection, ids: &[PaymentId]) -> sqlx::Result<Vec<Payment>> {
    let ids: Vec<uuid::Uuid> = ids.iter().map(|id| id.0).collect();
    sqlx::query_as!(
        Payment,
        r#"SELECT id AS "id: PaymentId", batch_id AS "batch_id: BatchId", line_no, paid_on,
                  amount AS "amount: Yen", payer_name_raw AS payer_name, payer_name_normalized, status,
                  decision, decided_at, created_at
           FROM payments WHERE id = ANY($1) ORDER BY id"#,
        &ids,
    )
    .fetch_all(conn)
    .await
}

pub async fn find(conn: &mut PgConnection, id: PaymentId) -> sqlx::Result<Option<Payment>> {
    sqlx::query_as!(
        Payment,
        r#"SELECT id AS "id: PaymentId", batch_id AS "batch_id: BatchId", line_no, paid_on,
                  amount AS "amount: Yen", payer_name_raw AS payer_name, payer_name_normalized, status,
                  decision, decided_at, created_at
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
                  decision, decided_at, created_at
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
                  decision, decided_at, created_at
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
