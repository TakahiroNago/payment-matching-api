//! Importing a batch of payments and matching each line against the open invoices.

use sqlx::PgPool;
use time::Date;

use super::matching::record_match;
use crate::domain::matching::{Decision, MatchingConfig, OpenInvoice, Payer, decide};
use crate::domain::normalize::normalize_payer_name;
use crate::domain::{BatchId, InvoiceId, PaymentId, Yen};
use crate::error::AppError;
use crate::repo::matches::MatchState;
use crate::repo::payments::{NewPayment, Payment, PaymentStatus};
use crate::repo::{customers, invoices, payments};

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

    for (index, line) in lines.iter().enumerate() {
        let line_no = i32::try_from(index + 1)
            .map_err(|_| AppError::Internal("too many lines in a batch".to_owned()))?;
        let normalized = normalize_payer_name(&line.payer_name);
        let resolved = match &normalized {
            Some(name) => customers::resolve_payer(&mut tx, name).await?,
            None => None,
        };

        // Lines are matched one after another inside the transaction, so a line sees the invoices cleared by
        // the lines before it.
        let candidates: Vec<OpenInvoice> = match &resolved {
            Some(payer) => invoices::open_of_customer(&mut tx, payer.customer_id).await?,
            None => {
                // The decision reports at most this many candidates, so more need not be read.
                let limit = MatchingConfig::MAX_R4_CANDIDATES as i64;
                invoices::open_with_amount(&mut tx, line.amount, limit).await?
            }
        };
        let payer = match &resolved {
            Some(payer) => Payer::Known {
                fee_deducted: payer.fee_deducted,
                open_invoices: &candidates,
            },
            None => Payer::Unknown {
                same_amount_invoices: &candidates,
            },
        };
        let decision = decide(line.amount, payer, config);

        let payment_id = PaymentId::generate();
        let status = match &decision {
            Decision::AutoConfirmed { .. } => PaymentStatus::Matched,
            Decision::Suggested { .. } => PaymentStatus::Suggested,
            Decision::Unmatched { .. } => PaymentStatus::Unmatched,
        };
        payments::insert(
            &mut tx,
            &NewPayment {
                id: payment_id,
                batch_id,
                line_no,
                paid_on: line.paid_on,
                amount: line.amount,
                payer_name_raw: &line.payer_name,
                payer_name_normalized: normalized.as_deref().unwrap_or(""),
                status,
                decision: serde_json::to_value(&decision).map_err(|e| {
                    AppError::Internal(format!("decision is not serializable: {e}"))
                })?,
            },
        )
        .await?;

        let (rule, invoice_ids, fee, state) = match &decision {
            Decision::AutoConfirmed {
                rule,
                invoice_ids,
                fee_amount,
            } => (*rule, invoice_ids, *fee_amount, MatchState::Confirmed),
            Decision::Suggested {
                rule,
                invoice_ids,
                fee_amount,
                ..
            } => (*rule, invoice_ids, *fee_amount, MatchState::Suggested),
            Decision::Unmatched { .. } => continue,
        };
        let chosen = with_amounts(invoice_ids, &candidates)?;
        record_match(&mut tx, payment_id, rule, state, &chosen, fee).await?;
    }

    let payments = payments::of_batch(&mut tx, batch_id).await?;
    tx.commit().await?;
    Ok(ImportOutcome {
        batch_id,
        replayed: false,
        payments,
    })
}

fn with_amounts(
    ids: &[InvoiceId],
    candidates: &[OpenInvoice],
) -> Result<Vec<(InvoiceId, Yen)>, AppError> {
    ids.iter()
        .map(|id| {
            candidates
                .iter()
                .find(|invoice| invoice.id == *id)
                .map(|invoice| (invoice.id, invoice.amount))
                .ok_or_else(|| {
                    AppError::Internal("the decision names an invoice it was not given".to_owned())
                })
        })
        .collect()
}
