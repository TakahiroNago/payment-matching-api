use serde::Serialize;
use sqlx::PgConnection;
use time::OffsetDateTime;

use crate::domain::matching::Rule;
use crate::domain::{CustomerId, InvoiceId, MatchId, PaymentId, Yen};

#[derive(Debug)]
pub struct Match {
    pub id: MatchId,
    pub payment_id: PaymentId,
    pub rule: String,
    pub state: String,
    pub fee_amount: Yen,
    pub created_at: OffsetDateTime,
    pub confirmed_at: Option<OffsetDateTime>,
    pub cancelled_at: Option<OffsetDateTime>,
    /// Why the match was cancelled; see [`CancelReason`].
    pub cancel_reason: Option<String>,
}

/// One invoice of a match, with what the response and the checks need from the invoice.
#[derive(Debug, Serialize)]
pub struct Allocation {
    pub invoice_id: InvoiceId,
    pub invoice_number: String,
    #[serde(skip)]
    pub customer_id: CustomerId,
    /// The amount of the invoice.
    pub invoice_amount: Yen,
    /// The cash of the payment applied to it.
    pub amount: Yen,
}

/// The confirmed match an invoice was cleared by.
#[derive(Debug, Serialize)]
pub struct ClearedBy {
    pub match_id: MatchId,
    pub payment_id: PaymentId,
    pub rule: String,
    /// The transfer fee of the whole match.
    pub fee_amount: Yen,
    /// The cash of the payment applied to this invoice.
    pub amount: Yen,
    #[serde(with = "time::serde::rfc3339::option")]
    pub confirmed_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchState {
    Suggested,
    Confirmed,
    Cancelled,
}

impl MatchState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Suggested => "suggested",
            Self::Confirmed => "confirmed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Why a match was cancelled (`matches.cancel_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// A person rejected a suggestion.
    Dismissed,
    /// A person cancelled a confirmed match.
    Reversed,
    /// A person matched the payment by hand instead of confirming the suggestion.
    Replaced,
    /// Another payment cleared an invoice of the suggestion.
    InvoiceCleared,
}

impl CancelReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dismissed => "dismissed",
            Self::Reversed => "reversed",
            Self::Replaced => "replaced",
            Self::InvoiceCleared => "invoice_cleared",
        }
    }
}

/// Inserts a match in the `suggested` or `confirmed` state with its allocations, in one statement.
///
/// Fails on the unique index `matches_one_active_per_payment_idx` when the payment already has a match that
/// is not cancelled (invariant I1).
pub async fn insert(
    conn: &mut PgConnection,
    payment_id: PaymentId,
    rule: Rule,
    state: MatchState,
    fee_amount: Yen,
    allocations: &[(InvoiceId, Yen)],
) -> sqlx::Result<MatchId> {
    let id = MatchId::generate();
    let invoice_ids: Vec<uuid::Uuid> = allocations.iter().map(|(id, _)| id.0).collect();
    let amounts: Vec<i64> = allocations.iter().map(|(_, amount)| amount.0).collect();
    sqlx::query!(
        "WITH new_match AS (
             INSERT INTO matches (id, payment_id, rule, state, fee_amount, confirmed_at)
             VALUES ($1, $2, $3, $4, $5, CASE WHEN $4 = 'confirmed' THEN now() END)
         )
         INSERT INTO match_allocations (match_id, invoice_id, amount)
         SELECT $1, invoice_id, amount FROM UNNEST($6::uuid[], $7::bigint[]) AS part (invoice_id, amount)",
        id.0,
        payment_id.0,
        rule.as_str(),
        state.as_str(),
        fee_amount.0,
        &invoice_ids,
        &amounts,
    )
    .execute(conn)
    .await?;
    Ok(id)
}

pub async fn find(conn: &mut PgConnection, id: MatchId) -> sqlx::Result<Option<Match>> {
    sqlx::query_as!(
        Match,
        r#"SELECT id AS "id: MatchId", payment_id AS "payment_id: PaymentId", rule, state,
                  fee_amount AS "fee_amount: Yen", created_at, confirmed_at, cancelled_at, cancel_reason
           FROM matches WHERE id = $1"#,
        id.0,
    )
    .fetch_optional(conn)
    .await
}

/// Every match of the payment, oldest first, cancelled ones included.
pub async fn of_payment(
    conn: &mut PgConnection,
    payment_id: PaymentId,
) -> sqlx::Result<Vec<Match>> {
    sqlx::query_as!(
        Match,
        r#"SELECT id AS "id: MatchId", payment_id AS "payment_id: PaymentId", rule, state,
                  fee_amount AS "fee_amount: Yen", created_at, confirmed_at, cancelled_at, cancel_reason
           FROM matches WHERE payment_id = $1 ORDER BY id"#,
        payment_id.0,
    )
    .fetch_all(conn)
    .await
}

/// The invoices of the match, oldest first: the order in which the fee was taken off.
pub async fn allocations_of(
    conn: &mut PgConnection,
    match_id: MatchId,
) -> sqlx::Result<Vec<Allocation>> {
    sqlx::query_as!(
        Allocation,
        r#"SELECT a.invoice_id AS "invoice_id: InvoiceId", i.invoice_number,
                  i.customer_id AS "customer_id: CustomerId", i.amount AS "invoice_amount: Yen",
                  a.amount AS "amount: Yen"
           FROM match_allocations a JOIN invoices i ON i.id = a.invoice_id
           WHERE a.match_id = $1
           ORDER BY i.due_date, i.id"#,
        match_id.0,
    )
    .fetch_all(conn)
    .await
}

/// The confirmed match that names the invoice. There is at most one (invariant I2).
pub async fn confirmed_for_invoice(
    conn: &mut PgConnection,
    invoice_id: InvoiceId,
) -> sqlx::Result<Option<ClearedBy>> {
    sqlx::query_as!(
        ClearedBy,
        r#"SELECT m.id AS "match_id: MatchId", m.payment_id AS "payment_id: PaymentId", m.rule,
                  m.fee_amount AS "fee_amount: Yen", a.amount AS "amount: Yen", m.confirmed_at
           FROM match_allocations a JOIN matches m ON m.id = a.match_id
           WHERE a.invoice_id = $1 AND m.state = 'confirmed'"#,
        invoice_id.0,
    )
    .fetch_optional(conn)
    .await
}

pub async fn set_confirmed(conn: &mut PgConnection, id: MatchId) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE matches SET state = 'confirmed', confirmed_at = now() WHERE id = $1",
        id.0
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Marks the invoices cleared and returns how many were open. The caller compares the count with the number
/// of ids: a smaller count means an invoice was cleared by someone else (invariant I2).
///
/// The same statement cancels every suggestion that names one of the invoices and leaves its payment
/// unmatched: such a suggestion could never be confirmed any more (invariant I5). The match that clears the
/// invoices must be `confirmed` already, or it would be cancelled with them.
pub async fn clear_invoices(
    conn: &mut PgConnection,
    invoice_ids: &[InvoiceId],
) -> sqlx::Result<u64> {
    let ids: Vec<uuid::Uuid> = invoice_ids.iter().map(|id| id.0).collect();
    let cleared = sqlx::query_scalar!(
        r#"WITH cleared AS (
               UPDATE invoices SET status = 'cleared' WHERE id = ANY($1) AND status = 'open'
               RETURNING id
           ),
           stale AS (
               UPDATE matches
               SET state = 'cancelled', cancelled_at = now(), cancel_reason = 'invoice_cleared'
               WHERE state = 'suggested'
                 AND id IN (SELECT match_id FROM match_allocations WHERE invoice_id = ANY($1))
               RETURNING payment_id
           ),
           left_unmatched AS (
               UPDATE payments SET status = 'unmatched' WHERE id IN (SELECT payment_id FROM stale)
           )
           SELECT count(*) AS "cleared!" FROM cleared"#,
        &ids,
    )
    .fetch_one(conn)
    .await?;
    Ok(u64::try_from(cleared).unwrap_or(0))
}

pub async fn set_cancelled(
    conn: &mut PgConnection,
    id: MatchId,
    reason: CancelReason,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE matches SET state = 'cancelled', cancelled_at = now(), cancel_reason = $2 WHERE id = $1",
        id.0,
        reason.as_str(),
    )
    .execute(conn)
    .await?;
    Ok(())
}
