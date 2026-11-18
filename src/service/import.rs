//! Importing a batch of payments and matching each line against the open invoices.

use sqlx::PgPool;
use time::Date;

use super::auto::AutoMatcher;
use crate::domain::matching::MatchingConfig;
use crate::domain::normalize::normalize_payer_name;
use crate::domain::{BatchId, PaymentId, Yen};
use crate::error::AppError;
use crate::repo::payments;
use crate::repo::payments::{Decided, NewPayment, Payment};

pub struct ImportLine {
    pub paid_on: Date,
    pub amount: Yen,
    pub payer_name: String,
}

pub struct ImportOutcome {
    pub batch_id: BatchId,
    /// The idempotency key had been used with the same request: nothing was written, and `payments` is the
    /// batch as it is now.
    pub replayed: bool,
    pub payments: Vec<Payment>,
}

/// Imports the lines and matches them, all in one transaction: a failure leaves nothing behind.
///
/// A request repeated with the same key and the same content writes nothing. The same key with other content
/// is a conflict.
///
/// The same content under another key is a conflict too, unless `allow_duplicate` is set: a statement sent
/// twice by a client that makes up a key per request would otherwise be imported twice, and its second copy
/// matched to whatever invoices of the same amounts are still open (next month's, typically).
pub async fn import(
    pool: &PgPool,
    config: &MatchingConfig,
    idempotency_key: &str,
    request_hash: &[u8],
    allow_duplicate: bool,
    lines: &[ImportLine],
) -> Result<ImportOutcome, AppError> {
    let mut tx = pool.begin().await?;
    payments::lock_matching(&mut tx).await?;

    let batch_id = BatchId::generate();
    if !payments::insert_batch(&mut tx, batch_id, idempotency_key, request_hash).await? {
        let existing = payments::find_batch_by_key(&mut tx, idempotency_key)
            .await?
            .ok_or_else(|| AppError::Internal("a conflicting batch disappeared".to_owned()))?;
        if existing.request_hash != request_hash {
            return Err(AppError::conflict(
                "Idempotency-Key was already used with a different request",
            ));
        }
        let payments = payments::of_batch(&mut tx, existing.id).await?;
        tx.commit().await?;
        return Ok(ImportOutcome {
            batch_id: existing.id,
            replayed: true,
            payments,
        });
    }

    if !allow_duplicate
        && let Some(earlier) = payments::find_batch_by_hash(&mut tx, request_hash, batch_id).await?
    {
        return Err(AppError::conflict(format!(
            "the same payments were already imported as batch {earlier}; \
             set allow_duplicate to import them again"
        )));
    }

    let normalized: Vec<Option<String>> = lines
        .iter()
        .map(|line| normalize_payer_name(&line.payer_name))
        .collect();

    // Three statements for the whole batch (the payments, what they are matched against, the decisions) and
    // one or two for each line that gets a match: the lock is held for all of it.
    let mut new_payments = Vec::with_capacity(lines.len());
    for (index, (line, normalized)) in lines.iter().zip(&normalized).enumerate() {
        new_payments.push(NewPayment {
            id: PaymentId::generate(),
            line_no: i32::try_from(index + 1)
                .map_err(|_| AppError::Internal("too many lines in a batch".to_owned()))?,
            paid_on: line.paid_on,
            amount: line.amount,
            payer_name_raw: &line.payer_name,
            payer_name_normalized: normalized.as_deref().unwrap_or(""),
        });
    }
    payments::insert_many(&mut tx, batch_id, &new_payments).await?;

    let mut matcher = AutoMatcher::new(config);
    matcher
        .preload(&mut tx, normalized.iter().flatten().map(String::as_str))
        .await?;

    // Lines are matched one after another, so a line sees the invoices cleared by the lines before it.
    let mut decided = Vec::with_capacity(lines.len());
    for (payment, normalized) in new_payments.iter().zip(&normalized) {
        let judgement = matcher
            .decide(&mut tx, payment.amount, normalized.as_deref())
            .await?;
        matcher.record(&mut tx, payment.id, &judgement).await?;
        decided.push(Decided {
            id: payment.id,
            payer_name_normalized: payment.payer_name_normalized.to_owned(),
            decision: judgement.decision_json()?,
        });
    }
    payments::set_decisions(&mut tx, &decided).await?;

    let payments = payments::of_batch(&mut tx, batch_id).await?;
    tx.commit().await?;
    Ok(ImportOutcome {
        batch_id,
        replayed: false,
        payments,
    })
}
