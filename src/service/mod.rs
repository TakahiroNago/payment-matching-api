//! Use cases: each public function is one transaction.

mod auto;
pub mod import;
pub mod matching;
pub mod rematch;

use serde::Serialize;
use sqlx::PgConnection;
use time::OffsetDateTime;

use crate::domain::{MatchId, PaymentId, Yen};
use crate::repo::matches::Allocation;
use crate::repo::payments::Payment;
use crate::repo::{matches, payments};

/// A payment with everything needed to explain its state: the automatic decision (in `payment.decision`) and
/// every match it ever had, cancelled ones included.
#[derive(Debug, Serialize)]
pub struct PaymentDetail {
    #[serde(flatten)]
    pub payment: Payment,
    pub matches: Vec<MatchDetail>,
}

#[derive(Debug, Serialize)]
pub struct MatchDetail {
    pub id: MatchId,
    pub rule: String,
    pub state: String,
    pub fee_amount: Yen,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub confirmed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub cancelled_at: Option<OffsetDateTime>,
    /// Why the match was cancelled: `dismissed`, `reversed`, `replaced`, `invoice_cleared` or
    /// `invoice_voided`.
    pub cancel_reason: Option<String>,
    pub allocations: Vec<Allocation>,
}

pub async fn payment_detail(
    conn: &mut PgConnection,
    id: PaymentId,
) -> sqlx::Result<Option<PaymentDetail>> {
    let Some(payment) = payments::find(conn, id).await? else {
        return Ok(None);
    };
    let mut details = Vec::new();
    for m in matches::of_payment(conn, id).await? {
        let allocations = matches::allocations_of(conn, m.id).await?;
        details.push(MatchDetail {
            id: m.id,
            rule: m.rule,
            state: m.state,
            fee_amount: m.fee_amount,
            created_at: m.created_at,
            confirmed_at: m.confirmed_at,
            cancelled_at: m.cancelled_at,
            cancel_reason: m.cancel_reason,
            allocations,
        });
    }
    Ok(Some(PaymentDetail {
        payment,
        matches: details,
    }))
}
