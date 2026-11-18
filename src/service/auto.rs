//! Automatic matching of payments, shared by the import and by the re-evaluation of unmatched payments.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use sqlx::PgConnection;

use super::matching::record_match;
use crate::domain::matching::{Decision, MatchingConfig, OpenInvoice, Payer, decide};
use crate::domain::{CustomerId, InvoiceId, PaymentId, Yen};
use crate::error::AppError;
use crate::repo::customers::ResolvedPayer;
use crate::repo::matches::MatchState;
use crate::repo::{customers, invoices};

/// Matches payments one after another inside a transaction that holds the matching lock.
///
/// What it reads about a payer (the customer and the customer's open invoices) is kept for the next payment
/// of the same payer and kept current as matches are confirmed. Nothing else can clear an invoice meanwhile:
/// that is what the lock is for. [`AutoMatcher::preload`] reads all of it in two queries, so that matching a
/// batch does not cost two round trips per line while every other writer waits.
pub(crate) struct AutoMatcher<'a> {
    config: &'a MatchingConfig,
    /// By normalized payer name; `None` for a name that belongs to no customer.
    payers: HashMap<String, Option<ResolvedPayer>>,
    /// The open invoices of a customer, oldest first.
    open_invoices: HashMap<CustomerId, Vec<OpenInvoice>>,
}

/// The decision about one payment, with what is needed to record it.
pub(crate) struct Judgement {
    pub decision: Decision,
    /// The invoices the decision names, with their amounts, in the order of the decision.
    chosen: Vec<(InvoiceId, Yen)>,
    /// The customer of the payer, when the payer is known.
    customer_id: Option<CustomerId>,
}

impl Judgement {
    /// The decision as stored in `payments.decision`.
    pub fn decision_json(&self) -> Result<serde_json::Value, AppError> {
        serde_json::to_value(&self.decision)
            .map_err(|e| AppError::Internal(format!("decision is not serializable: {e}")))
    }
}

impl<'a> AutoMatcher<'a> {
    pub fn new(config: &'a MatchingConfig) -> Self {
        Self {
            config,
            payers: HashMap::new(),
            open_invoices: HashMap::new(),
        }
    }

    /// Reads the customers behind these normalized payer names and their open invoices, in two queries.
    pub async fn preload<'n>(
        &mut self,
        conn: &mut PgConnection,
        names: impl Iterator<Item = &'n str>,
    ) -> Result<(), AppError> {
        let mut unseen: Vec<String> = names
            .filter(|name| !self.payers.contains_key(*name))
            .map(str::to_owned)
            .collect();
        unseen.sort_unstable();
        unseen.dedup();
        if unseen.is_empty() {
            return Ok(());
        }

        let mut customer_ids = Vec::new();
        for found in customers::resolve_payers(conn, &unseen).await? {
            if !self.open_invoices.contains_key(&found.payer.customer_id) {
                customer_ids.push(found.payer.customer_id);
            }
            self.payers.insert(found.normalized, Some(found.payer));
        }
        for name in unseen {
            self.payers.entry(name).or_insert(None);
        }

        customer_ids.sort_unstable();
        customer_ids.dedup();
        for customer_id in &customer_ids {
            self.open_invoices.insert(*customer_id, Vec::new());
        }
        // Oldest first, so each customer's list ends up in that order too.
        for (customer_id, invoice) in invoices::open_of_customers(conn, &customer_ids).await? {
            self.open_invoices
                .entry(customer_id)
                .or_default()
                .push(invoice);
        }
        Ok(())
    }

    /// Decides how a payment relates to the invoices that are open now. Writes nothing.
    ///
    /// `normalized` is the normalized payer name, `None` when the name could not be normalized.
    pub async fn decide(
        &mut self,
        conn: &mut PgConnection,
        amount: Yen,
        normalized: Option<&str>,
    ) -> Result<Judgement, AppError> {
        let resolved = match normalized {
            Some(name) => self.resolve(conn, name).await?,
            None => None,
        };
        let Some(payer) = resolved else {
            // The decision reports at most this many candidates, so more need not be read.
            let limit = MatchingConfig::MAX_R4_CANDIDATES as i64;
            let candidates = invoices::open_with_amount(conn, amount, limit).await?;
            let decision = decide(
                amount,
                Payer::Unknown {
                    same_amount_invoices: &candidates,
                },
                self.config,
            );
            let chosen = with_amounts(&decision, &candidates)?;
            return Ok(Judgement {
                decision,
                chosen,
                customer_id: None,
            });
        };

        let open: &[OpenInvoice] = match self.open_invoices.entry(payer.customer_id) {
            Entry::Occupied(known) => known.into_mut(),
            Entry::Vacant(unknown) => {
                unknown.insert(invoices::open_of_customer(conn, payer.customer_id).await?)
            }
        };
        let decision = decide(
            amount,
            Payer::Known {
                fee_deducted: payer.fee_deducted,
                open_invoices: open,
            },
            self.config,
        );
        let chosen = with_amounts(&decision, open)?;
        Ok(Judgement {
            decision,
            chosen,
            customer_id: Some(payer.customer_id),
        })
    }

    /// Records the match the judgement calls for, if any, for a payment that has no match. A confirmed match
    /// clears its invoices, so that the next payment does not see them.
    pub async fn record(
        &mut self,
        conn: &mut PgConnection,
        payment_id: PaymentId,
        judgement: &Judgement,
    ) -> Result<(), AppError> {
        let (rule, fee, state) = match &judgement.decision {
            Decision::AutoConfirmed {
                rule, fee_amount, ..
            } => (*rule, *fee_amount, MatchState::Confirmed),
            Decision::Suggested {
                rule, fee_amount, ..
            } => (*rule, *fee_amount, MatchState::Suggested),
            Decision::Unmatched { .. } => return Ok(()),
        };
        record_match(conn, payment_id, rule, state, &judgement.chosen, fee).await?;

        if state == MatchState::Confirmed
            && let Some(open) = judgement
                .customer_id
                .and_then(|id| self.open_invoices.get_mut(&id))
        {
            open.retain(|invoice| !judgement.chosen.iter().any(|(id, _)| *id == invoice.id));
        }
        Ok(())
    }

    async fn resolve(
        &mut self,
        conn: &mut PgConnection,
        name: &str,
    ) -> Result<Option<ResolvedPayer>, AppError> {
        if let Some(known) = self.payers.get(name) {
            return Ok(*known);
        }
        let resolved = customers::resolve_payer(conn, name).await?;
        self.payers.insert(name.to_owned(), resolved);
        Ok(resolved)
    }
}

/// The invoices a decision names, with the amounts they have among the invoices the decision was given.
fn with_amounts(
    decision: &Decision,
    candidates: &[OpenInvoice],
) -> Result<Vec<(InvoiceId, Yen)>, AppError> {
    let ids = match decision {
        Decision::AutoConfirmed { invoice_ids, .. } | Decision::Suggested { invoice_ids, .. } => {
            invoice_ids.as_slice()
        }
        Decision::Unmatched { .. } => &[],
    };
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
