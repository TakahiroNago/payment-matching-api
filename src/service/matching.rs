//! Confirming, creating and cancelling matches, and voiding invoices.
//!
//! Invariants, and where each is enforced:
//!
//! - I1: a payment has at most one match that is not cancelled. A partial unique index.
//! - I2: an invoice belongs to at most one confirmed match. [`clear_invoices`] flips `open` to `cleared` with a
//!   conditional `UPDATE` and compares the row count; whoever comes second gets a conflict.
//! - I3: for a confirmed match, the allocations add up to the payment, and allocations + fee add up to the
//!   invoices. Checked here before anything is confirmed.
//! - I5: a suggestion names open invoices only. [`clear_invoices`] cancels the suggestions of other payments
//!   that name an invoice it clears, and [`void_invoice`] those that name the invoice it voids.

use sqlx::{PgConnection, PgPool};

use super::{PaymentDetail, payment_detail};
use crate::domain::matching::{Rule, allocate};
use crate::domain::{CustomerId, InvoiceId, MatchId, PaymentId, Yen};
use crate::error::{AppError, FieldError};
use crate::repo::customers::AliasSource;
use crate::repo::invoices::Invoice;
use crate::repo::matches::{CancelReason, MatchState};
use crate::repo::payments::{Payment, PaymentStatus};
use crate::repo::{customers, invoices, matches, payments};

pub struct ManualMatch {
    pub invoice_ids: Vec<InvoiceId>,
    pub fee_amount: Yen,
    pub register_alias: bool,
}

/// Inserts a match with its allocations and, when it is confirmed, clears its invoices.
///
/// `chosen` is oldest first; the fee is taken off the first invoice.
pub(crate) async fn record_match(
    conn: &mut PgConnection,
    payment_id: PaymentId,
    rule: Rule,
    state: MatchState,
    chosen: &[(InvoiceId, Yen)],
    fee: Yen,
) -> Result<MatchId, AppError> {
    let parts = allocate(chosen, fee).ok_or_else(|| {
        AppError::Internal("the fee does not fit the first invoice of the match".to_owned())
    })?;
    let match_id = matches::insert(conn, payment_id, rule, state, fee, &parts).await?;
    if state == MatchState::Confirmed {
        let ids: Vec<InvoiceId> = chosen.iter().map(|(id, _)| *id).collect();
        clear_invoices(conn, &ids).await?;
    }
    Ok(match_id)
}

/// Clears the invoices of a match that is already recorded as confirmed.
///
/// Suggestions of other payments that name one of the invoices are cancelled and their payments go back to
/// `unmatched`: they could never be confirmed, and a payment listed as `suggested` should have a suggestion
/// that a person can act on.
async fn clear_invoices(conn: &mut PgConnection, ids: &[InvoiceId]) -> Result<(), AppError> {
    let cleared = matches::clear_invoices(conn, ids).await?;
    if cleared != ids.len() as u64 {
        return Err(AppError::conflict(
            "an invoice of the match is no longer open",
        ));
    }
    Ok(())
}

/// Confirms a suggested match.
pub async fn confirm(
    pool: &PgPool,
    match_id: MatchId,
    register_alias: bool,
) -> Result<PaymentDetail, AppError> {
    let mut tx = pool.begin().await?;
    payments::lock_matching(&mut tx).await?;

    let found = matches::find(&mut tx, match_id)
        .await?
        .ok_or(AppError::NotFound("match"))?;
    if found.state != MatchState::Suggested.as_str() {
        return Err(AppError::conflict(format!(
            "the match is {}, not suggested",
            found.state
        )));
    }
    let payment = find_payment(&mut tx, found.payment_id).await?;
    let allocations = matches::allocations_of(&mut tx, match_id).await?;

    let allocated: Yen = allocations.iter().map(|a| a.amount).sum();
    let billed: Yen = allocations.iter().map(|a| a.invoice_amount).sum();
    if allocated != payment.amount || allocated + found.fee_amount != billed {
        return Err(AppError::Internal(format!(
            "match {match_id} does not add up"
        )));
    }

    let ids: Vec<InvoiceId> = allocations.iter().map(|a| a.invoice_id).collect();
    // Confirmed first: clearing cancels whatever is still a suggestion on these invoices.
    matches::set_confirmed(&mut tx, match_id).await?;
    clear_invoices(&mut tx, &ids).await?;
    payments::set_status(&mut tx, payment.id, PaymentStatus::Matched).await?;

    if register_alias {
        let customer_id = allocations
            .first()
            .map(|a| a.customer_id)
            .ok_or_else(|| AppError::Internal(format!("match {match_id} has no invoices")))?;
        learn_alias(&mut tx, &payment, customer_id).await?;
    }

    let detail = detail_of(&mut tx, payment.id).await?;
    tx.commit().await?;
    Ok(detail)
}

/// Matches a payment to invoices chosen by a person. A suggestion the payment may have is cancelled.
pub async fn match_manually(
    pool: &PgPool,
    payment_id: PaymentId,
    input: &ManualMatch,
) -> Result<PaymentDetail, AppError> {
    let mut tx = pool.begin().await?;
    payments::lock_matching(&mut tx).await?;

    let payment = payments::find(&mut tx, payment_id)
        .await?
        .ok_or(AppError::NotFound("payment"))?;
    if payment.status == PaymentStatus::Matched.as_str() {
        return Err(AppError::conflict("the payment is already matched"));
    }

    let chosen = invoices::lock_many(&mut tx, &input.invoice_ids).await?;
    if chosen.len() != input.invoice_ids.len() {
        return Err(AppError::invalid("invoice_ids", "not_found"));
    }
    if chosen
        .iter()
        .any(|invoice| invoice.status != invoices::STATUS_OPEN)
    {
        return Err(AppError::conflict("an invoice is not open"));
    }
    let customer_id = single_customer(chosen.iter().map(|invoice| invoice.customer_id))
        .ok_or_else(|| AppError::invalid("invoice_ids", "different_customers"))?;

    let pairs: Vec<(InvoiceId, Yen)> = chosen
        .iter()
        .map(|invoice| (invoice.id, invoice.amount))
        .collect();
    if allocate(&pairs, input.fee_amount).is_none() {
        return Err(AppError::invalid("fee_amount", "out_of_range"));
    }
    let billed: Yen = pairs.iter().map(|(_, amount)| *amount).sum();
    if billed - input.fee_amount != payment.amount {
        return Err(AppError::Validation(vec![FieldError::new(
            "invoice_ids",
            "amount_mismatch",
        )]));
    }

    for existing in matches::of_payment(&mut tx, payment.id).await? {
        if existing.state == MatchState::Suggested.as_str() {
            matches::set_cancelled(&mut tx, existing.id, CancelReason::Replaced).await?;
        }
    }
    record_match(
        &mut tx,
        payment.id,
        Rule::Manual,
        MatchState::Confirmed,
        &pairs,
        input.fee_amount,
    )
    .await?;
    payments::set_status(&mut tx, payment.id, PaymentStatus::Matched).await?;

    if input.register_alias {
        learn_alias(&mut tx, &payment, customer_id).await?;
    }

    let detail = detail_of(&mut tx, payment.id).await?;
    tx.commit().await?;
    Ok(detail)
}

/// Cancels a match. A confirmed match gives its invoices back; a suggestion is simply dismissed. Either way
/// the payment is unmatched afterwards and the match stays as history.
pub async fn cancel(pool: &PgPool, match_id: MatchId) -> Result<PaymentDetail, AppError> {
    let mut tx = pool.begin().await?;
    payments::lock_matching(&mut tx).await?;

    let found = matches::find(&mut tx, match_id)
        .await?
        .ok_or(AppError::NotFound("match"))?;
    if found.state == MatchState::Cancelled.as_str() {
        return Err(AppError::conflict("the match is already cancelled"));
    }
    if found.state == MatchState::Confirmed.as_str() {
        let ids: Vec<InvoiceId> = matches::allocations_of(&mut tx, match_id)
            .await?
            .iter()
            .map(|a| a.invoice_id)
            .collect();
        let reopened = invoices::reopen(&mut tx, &ids).await?;
        if reopened != ids.len() as u64 {
            return Err(AppError::Internal(format!(
                "match {match_id} was confirmed but an invoice was open"
            )));
        }
    }
    let reason = if found.state == MatchState::Confirmed.as_str() {
        CancelReason::Reversed
    } else {
        CancelReason::Dismissed
    };
    matches::set_cancelled(&mut tx, match_id, reason).await?;
    payments::set_status(&mut tx, found.payment_id, PaymentStatus::Unmatched).await?;

    let detail = detail_of(&mut tx, found.payment_id).await?;
    tx.commit().await?;
    Ok(detail)
}

/// Takes an invoice that was registered by mistake out of matching. Only an open invoice can be voided: a
/// cleared one has a payment matched to it, and that match has to be cancelled first. Suggestions that name
/// the invoice are cancelled and their payments go back to `unmatched`.
pub async fn void_invoice(pool: &PgPool, id: InvoiceId) -> Result<Invoice, AppError> {
    let mut tx = pool.begin().await?;
    payments::lock_matching(&mut tx).await?;

    let invoice = invoices::find(&mut tx, id)
        .await?
        .ok_or(AppError::NotFound("invoice"))?;
    if invoice.status == invoices::STATUS_CLEARED {
        return Err(AppError::conflict(
            "the invoice is cleared; cancel the match that cleared it first",
        ));
    }
    if invoice.status != invoices::STATUS_OPEN {
        return Err(AppError::conflict("the invoice is already void"));
    }
    if !matches::void_invoice(&mut tx, id).await? {
        return Err(AppError::Internal(format!(
            "invoice {id} was open but could not be voided"
        )));
    }
    let voided = invoices::find(&mut tx, id)
        .await?
        .ok_or_else(|| AppError::Internal(format!("invoice {id} disappeared")))?;
    tx.commit().await?;
    Ok(voided)
}

/// Registers the payer name of the payment as an alias of the customer, so that the next payment under this
/// name is recognised.
async fn learn_alias(
    conn: &mut PgConnection,
    payment: &Payment,
    customer_id: CustomerId,
) -> Result<(), AppError> {
    if payment.payer_name_normalized.is_empty() {
        return Err(AppError::invalid(
            "register_alias",
            "payer_name_not_normalizable",
        ));
    }
    match customers::resolve_payer(conn, &payment.payer_name_normalized).await? {
        Some(owner) if owner.customer_id == customer_id => Ok(()),
        Some(_) => Err(AppError::conflict(
            "the payer name already belongs to another customer",
        )),
        None => {
            customers::insert_alias(
                conn,
                customer_id,
                &payment.payer_name,
                &payment.payer_name_normalized,
                AliasSource::Learned,
            )
            .await?;
            Ok(())
        }
    }
}

fn single_customer(mut ids: impl Iterator<Item = CustomerId>) -> Option<CustomerId> {
    let first = ids.next()?;
    ids.all(|id| id == first).then_some(first)
}

async fn find_payment(conn: &mut PgConnection, id: PaymentId) -> Result<Payment, AppError> {
    payments::find(conn, id)
        .await?
        .ok_or_else(|| AppError::Internal(format!("payment {id} of a match is missing")))
}

async fn detail_of(conn: &mut PgConnection, id: PaymentId) -> Result<PaymentDetail, AppError> {
    payment_detail(conn, id)
        .await?
        .ok_or_else(|| AppError::Internal(format!("payment {id} disappeared")))
}
