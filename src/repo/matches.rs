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

/// Inserts a match in the `suggested` or `confirmed` state.
///
/// Fails on the unique index `matches_one_active_per_payment_idx` when the payment already has a match that
/// is not cancelled (invariant I1).
pub async fn insert(
    conn: &mut PgConnection,
    payment_id: PaymentId,
    rule: Rule,
    state: MatchState,
    fee_amount: Yen,
) -> sqlx::Result<MatchId> {
    let id = MatchId::generate();
    sqlx::query!(
        "INSERT INTO matches (id, payment_id, rule, state, fee_amount, confirmed_at)
         VALUES ($1, $2, $3, $4, $5, CASE WHEN $4 = 'confirmed' THEN now() END)",
        id.0,
        payment_id.0,
        rule.as_str(),
        state.as_str(),
        fee_amount.0,
    )
    .execute(conn)
    .await?;
    Ok(id)
}

pub async fn insert_allocation(
    conn: &mut PgConnection,
    match_id: MatchId,
    invoice_id: InvoiceId,
    amount: Yen,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO match_allocations (match_id, invoice_id, amount) VALUES ($1, $2, $3)",
        match_id.0,
        invoice_id.0,
        amount.0,
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn find(conn: &mut PgConnection, id: MatchId) -> sqlx::Result<Option<Match>> {
    sqlx::query_as!(
        Match,
        r#"SELECT id AS "id: MatchId", payment_id AS "payment_id: PaymentId", rule, state,
                  fee_amount AS "fee_amount: Yen", created_at, confirmed_at, cancelled_at
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
                  fee_amount AS "fee_amount: Yen", created_at, confirmed_at, cancelled_at
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

/// Cancels every suggestion that names one of the invoices and leaves its payment unmatched. Called when
/// the invoices are cleared: such a suggestion could never be confirmed any more. Returns how many payments
/// lost their suggestion.
pub async fn cancel_suggestions_naming(
    conn: &mut PgConnection,
    invoice_ids: &[InvoiceId],
) -> sqlx::Result<u64> {
    let ids: Vec<uuid::Uuid> = invoice_ids.iter().map(|id| id.0).collect();
    let done = sqlx::query!(
        "WITH stale AS (
             UPDATE matches SET state = 'cancelled', cancelled_at = now()
             WHERE state = 'suggested'
               AND id IN (SELECT match_id FROM match_allocations WHERE invoice_id = ANY($1))
             RETURNING payment_id
         )
         UPDATE payments SET status = 'unmatched' WHERE id IN (SELECT payment_id FROM stale)",
        &ids,
    )
    .execute(conn)
    .await?;
    Ok(done.rows_affected())
}

pub async fn set_cancelled(conn: &mut PgConnection, id: MatchId) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE matches SET state = 'cancelled', cancelled_at = now() WHERE id = $1",
        id.0
    )
    .execute(conn)
    .await?;
    Ok(())
}
