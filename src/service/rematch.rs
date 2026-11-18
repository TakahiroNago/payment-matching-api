//! Deciding again about payments that are unmatched.
//!
//! The automatic decision is made when a payment is imported, against the invoices and payer names known
//! then. Later an alias is registered, an invoice is entered late, or a suggestion goes away because another
//! payment took its invoice: the same payment could now be matched, and this is how it gets a second look.

use sqlx::PgPool;

use super::auto::AutoMatcher;
use crate::domain::PaymentId;
use crate::domain::matching::MatchingConfig;
use crate::domain::normalize::normalize_payer_name;
use crate::error::AppError;
use crate::repo::payments;
use crate::repo::payments::{Decided, Payment};

pub struct RematchOutcome {
    /// The unmatched payments that were looked at, as they are now, oldest first.
    pub examined: Vec<Payment>,
    /// The last payment looked at, when more unmatched payments follow it.
    pub next: Option<PaymentId>,
}

/// Decides again about up to `limit` unmatched payments after `after`, oldest first, in one transaction.
///
/// Each payment goes through the same rules as at import and sees the invoices cleared by the payments before
/// it. Its `decision` is replaced by the new one, whatever the outcome. Only unmatched payments are looked
/// at: a suggestion is left for a person to confirm or dismiss, and a dismissed one comes up here again.
pub async fn rematch(
    pool: &PgPool,
    config: &MatchingConfig,
    after: Option<PaymentId>,
    limit: u32,
) -> Result<RematchOutcome, AppError> {
    let mut tx = pool.begin().await?;
    payments::lock_matching(&mut tx).await?;

    // One more than asked for: the extra row only tells whether more follow.
    let mut unmatched = payments::unmatched_after(&mut tx, after, i64::from(limit) + 1).await?;
    let has_more = unmatched.len() > limit as usize;
    unmatched.truncate(limit as usize);
    let next = unmatched.last().map(|p| p.id).filter(|_| has_more);

    // Normalized again rather than read back: the stored key may come from older normalization rules.
    let normalized: Vec<Option<String>> = unmatched
        .iter()
        .map(|payment| normalize_payer_name(&payment.payer_name))
        .collect();
    let mut matcher = AutoMatcher::new(config);
    matcher
        .preload(&mut tx, normalized.iter().flatten().map(String::as_str))
        .await?;

    let mut decided = Vec::with_capacity(unmatched.len());
    for (payment, normalized) in unmatched.iter().zip(normalized) {
        let judgement = matcher
            .decide(&mut tx, payment.amount, normalized.as_deref())
            .await?;
        matcher.record(&mut tx, payment.id, &judgement).await?;
        decided.push(Decided {
            id: payment.id,
            payer_name_normalized: normalized.unwrap_or_default(),
            decision: judgement.decision_json()?,
        });
    }
    payments::set_decisions(&mut tx, &decided).await?;

    let ids: Vec<PaymentId> = unmatched.iter().map(|payment| payment.id).collect();
    let examined = payments::by_ids(&mut tx, &ids).await?;
    tx.commit().await?;
    Ok(RematchOutcome { examined, next })
}
