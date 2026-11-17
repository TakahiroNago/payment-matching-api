//! The matching rules, as a pure function from a payment and the open invoices to a decision.
//!
//! | Rule | Condition | Unique | Several |
//! |---|---|---|---|
//! | R1 | payer known; one open invoice of the customer has the paid amount | confirmed | suggested |
//! | R2 | payer known; two or more open invoices add up to the paid amount | confirmed | suggested |
//! | R3 | payer known and deducts the transfer fee; R1 or R2 holds for amount + fee | suggested | suggested |
//! | R4 | payer unknown; an open invoice of any customer has the paid amount | suggested | suggested |
//!
//! The rules are tried in this order and the first that holds wins. A match is confirmed automatically only
//! when it is the single explanation; whenever there are several, the one made of the oldest invoices is
//! suggested and a person decides.

use serde::Serialize;
use time::Date;

use super::{InvoiceId, Yen};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Rule {
    R1,
    R2,
    R3,
    R4,
    #[serde(rename = "manual")]
    Manual,
}

impl Rule {
    /// The value stored in `matches.rule`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Rule::R1 => "R1",
            Rule::R2 => "R2",
            Rule::R3 => "R3",
            Rule::R4 => "R4",
            Rule::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenInvoice {
    pub id: InvoiceId,
    pub amount: Yen,
    pub due_date: Date,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchingConfig {
    /// The transfer fees R3 tries, ascending and without duplicates.
    pub fee_amounts: Vec<Yen>,
    /// R2 and the combinations of R3 are skipped when the customer has more open invoices than this: the
    /// search visits up to 2^n subsets.
    pub max_subset_invoices: usize,
}

impl MatchingConfig {
    pub const DEFAULT_FEES: [i64; 8] = [110, 220, 330, 440, 550, 660, 770, 880];
    pub const DEFAULT_MAX_SUBSET_INVOICES: usize = 20;
    /// How many same-amount invoices R4 reports as candidates.
    pub const MAX_R4_CANDIDATES: usize = 5;
}

impl Default for MatchingConfig {
    fn default() -> Self {
        Self {
            fee_amounts: Self::DEFAULT_FEES.into_iter().map(Yen).collect(),
            max_subset_invoices: Self::DEFAULT_MAX_SUBSET_INVOICES,
        }
    }
}

/// What is known about the payer of a payment.
#[derive(Debug, Clone, Copy)]
pub enum Payer<'a> {
    /// The normalized payer name belongs to a customer.
    Known {
        fee_deducted: bool,
        /// All open invoices of the customer.
        open_invoices: &'a [OpenInvoice],
    },
    /// The name belongs to no customer (or could not be normalized).
    Unknown {
        /// Open invoices of any customer whose amount equals the paid amount.
        same_amount_invoices: &'a [OpenInvoice],
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Decision {
    /// The single explanation of the payment: recorded as a confirmed match.
    AutoConfirmed {
        rule: Rule,
        invoice_ids: Vec<InvoiceId>,
        fee_amount: Yen,
    },
    /// A plausible explanation that a person has to confirm.
    Suggested {
        rule: Rule,
        invoice_ids: Vec<InvoiceId>,
        fee_amount: Yen,
        reason: SuggestReason,
    },
    Unmatched {
        reason: UnmatchedReason,
        subset_search_skipped: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SuggestReason {
    /// R1: several open invoices have the paid amount; the oldest is suggested.
    SameAmountInvoices { count: usize },
    /// R2 or R3: several combinations explain the payment; the one made of the oldest invoices is suggested.
    SeveralCombinations,
    /// R3: the payment is short of the invoices by a transfer fee.
    FeeDeducted,
    /// R4: only the amount agrees.
    PayerUnknown {
        candidate_invoice_ids: Vec<InvoiceId>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnmatchedReason {
    /// The payer is unknown and no open invoice has the paid amount.
    PayerUnknown,
    /// The payer is known but no rule explains the amount.
    NoCandidate,
}

/// Decides how a payment of `amount` relates to the open invoices.
#[must_use]
pub fn decide(amount: Yen, payer: Payer<'_>, config: &MatchingConfig) -> Decision {
    match payer {
        Payer::Unknown {
            same_amount_invoices,
        } => decide_unknown_payer(amount, same_amount_invoices),
        Payer::Known {
            fee_deducted,
            open_invoices,
        } => decide_known_payer(amount, fee_deducted, open_invoices, config),
    }
}

fn decide_unknown_payer(amount: Yen, same_amount_invoices: &[OpenInvoice]) -> Decision {
    let candidates = oldest_first(same_amount_invoices.iter().filter(|i| i.amount == amount));
    match candidates.first() {
        None => Decision::Unmatched {
            reason: UnmatchedReason::PayerUnknown,
            subset_search_skipped: false,
        },
        Some(oldest) => Decision::Suggested {
            rule: Rule::R4,
            invoice_ids: vec![oldest.id],
            fee_amount: Yen::ZERO,
            reason: SuggestReason::PayerUnknown {
                candidate_invoice_ids: candidates
                    .iter()
                    .take(MatchingConfig::MAX_R4_CANDIDATES)
                    .map(|i| i.id)
                    .collect(),
            },
        },
    }
}

fn decide_known_payer(
    amount: Yen,
    fee_deducted: bool,
    open_invoices: &[OpenInvoice],
    config: &MatchingConfig,
) -> Decision {
    let invoices = oldest_first(open_invoices.iter());
    let subset_search_skipped = invoices.len() > config.max_subset_invoices;

    // R1
    let same_amount: Vec<&OpenInvoice> = invoices
        .iter()
        .copied()
        .filter(|i| i.amount == amount)
        .collect();
    match same_amount.as_slice() {
        [] => {}
        [only] => {
            return Decision::AutoConfirmed {
                rule: Rule::R1,
                invoice_ids: vec![only.id],
                fee_amount: Yen::ZERO,
            };
        }
        [oldest, ..] => {
            return Decision::Suggested {
                rule: Rule::R1,
                invoice_ids: vec![oldest.id],
                fee_amount: Yen::ZERO,
                reason: SuggestReason::SameAmountInvoices {
                    count: same_amount.len(),
                },
            };
        }
    }

    // R2
    if !subset_search_skipped {
        let combinations = combinations_summing_to(&invoices, amount, 2);
        let several = combinations.len() > 1;
        if let Some(first) = combinations.into_iter().next() {
            let invoice_ids = first.iter().map(|i| i.id).collect();
            return if several {
                Decision::Suggested {
                    rule: Rule::R2,
                    invoice_ids,
                    fee_amount: Yen::ZERO,
                    reason: SuggestReason::SeveralCombinations,
                }
            } else {
                Decision::AutoConfirmed {
                    rule: Rule::R2,
                    invoice_ids,
                    fee_amount: Yen::ZERO,
                }
            };
        }
    }

    // R3
    if fee_deducted {
        let mut explanations: Vec<(Yen, Vec<&OpenInvoice>)> = Vec::new();
        for &fee in &config.fee_amounts {
            if explanations.len() > 1 {
                break;
            }
            let billed = amount + fee;
            // The fee is taken off the oldest invoice, which must stay positive after it.
            let covers_fee = |combination: &Vec<&OpenInvoice>| {
                combination.first().is_some_and(|i| i.amount > fee)
            };
            explanations.extend(
                invoices
                    .iter()
                    .filter(|i| i.amount == billed)
                    .map(|i| vec![*i])
                    .filter(covers_fee)
                    .map(|c| (fee, c)),
            );
            if !subset_search_skipped {
                explanations.extend(
                    combinations_summing_to(&invoices, billed, 2)
                        .into_iter()
                        .filter(covers_fee)
                        .map(|c| (fee, c)),
                );
            }
        }
        let several = explanations.len() > 1;
        if let Some((fee, combination)) = explanations.into_iter().next() {
            return Decision::Suggested {
                rule: Rule::R3,
                invoice_ids: combination.iter().map(|i| i.id).collect(),
                fee_amount: fee,
                reason: if several {
                    SuggestReason::SeveralCombinations
                } else {
                    SuggestReason::FeeDeducted
                },
            };
        }
    }

    Decision::Unmatched {
        reason: UnmatchedReason::NoCandidate,
        subset_search_skipped,
    }
}

/// The invoices by due date, then id: the order every rule reports and searches in, whatever order the caller
/// supplied.
fn oldest_first<'a>(invoices: impl Iterator<Item = &'a OpenInvoice>) -> Vec<&'a OpenInvoice> {
    let mut sorted: Vec<&OpenInvoice> = invoices.collect();
    sorted.sort_by_key(|i| (i.due_date, i.id));
    sorted
}

/// Up to `limit` sets of two or more invoices whose amounts add up to `target`. The first set found is the one
/// that prefers the oldest invoices. `limit` is 2 everywhere: enough to tell "unique" from "several".
fn combinations_summing_to<'a>(
    invoices: &[&'a OpenInvoice],
    target: Yen,
    limit: usize,
) -> Vec<Vec<&'a OpenInvoice>> {
    // remaining_total[i] = the sum of invoices[i..]; a branch that cannot reach the target is cut.
    let mut remaining_total = vec![Yen::ZERO; invoices.len() + 1];
    for (i, invoice) in invoices.iter().enumerate().rev() {
        remaining_total[i] = remaining_total[i + 1] + invoice.amount;
    }
    let mut search = SubsetSearch {
        invoices,
        remaining_total,
        limit,
        chosen: Vec::new(),
        found: Vec::new(),
    };
    search.visit(0, target);
    search.found
}

struct SubsetSearch<'s, 'a> {
    invoices: &'s [&'a OpenInvoice],
    remaining_total: Vec<Yen>,
    limit: usize,
    chosen: Vec<&'a OpenInvoice>,
    found: Vec<Vec<&'a OpenInvoice>>,
}

impl SubsetSearch<'_, '_> {
    fn visit(&mut self, index: usize, missing: Yen) {
        if self.found.len() >= self.limit {
            return;
        }
        if missing == Yen::ZERO {
            // Amounts are positive, so adding another invoice would overshoot: this branch ends here.
            if self.chosen.len() >= 2 {
                self.found.push(self.chosen.clone());
            }
            return;
        }
        if index == self.invoices.len() || missing > self.remaining_total[index] {
            return;
        }
        let invoice = self.invoices[index];
        if invoice.amount <= missing {
            self.chosen.push(invoice);
            self.visit(index + 1, missing - invoice.amount);
            self.chosen.pop();
        }
        self.visit(index + 1, missing);
    }
}

/// Splits the cash of a payment over the invoices of a match: every invoice in full, except that the transfer
/// fee is taken off the first one. The parts add up to (sum of the invoices - fee), the paid amount.
///
/// Returns `None` when the fee is negative or does not leave the first invoice a positive part.
#[must_use]
pub fn allocate(invoices: &[(InvoiceId, Yen)], fee: Yen) -> Option<Vec<(InvoiceId, Yen)>> {
    let (&(first_id, first_amount), rest) = invoices.split_first()?;
    if fee < Yen::ZERO || first_amount <= fee {
        return None;
    }
    let mut parts = Vec::with_capacity(invoices.len());
    parts.push((first_id, first_amount - fee));
    parts.extend_from_slice(rest);
    Some(parts)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use proptest::prelude::*;
    use time::Month;
    use uuid::Uuid;

    use super::*;

    fn invoice(n: u128, amount: i64, due: (i32, u8, u8)) -> OpenInvoice {
        OpenInvoice {
            id: InvoiceId(Uuid::from_u128(n)),
            amount: Yen(amount),
            due_date: Date::from_calendar_date(due.0, Month::try_from(due.1).unwrap(), due.2)
                .unwrap(),
        }
    }

    fn id(n: u128) -> InvoiceId {
        InvoiceId(Uuid::from_u128(n))
    }

    fn known(open_invoices: &[OpenInvoice], fee_deducted: bool) -> Payer<'_> {
        Payer::Known {
            fee_deducted,
            open_invoices,
        }
    }

    fn config() -> MatchingConfig {
        MatchingConfig::default()
    }

    #[test]
    fn r1_confirms_the_only_invoice_with_the_paid_amount() {
        let invoices = [
            invoice(1, 110_000, (2026, 10, 31)),
            invoice(2, 55_000, (2026, 10, 31)),
        ];
        assert_eq!(
            decide(Yen(110_000), known(&invoices, false), &config()),
            Decision::AutoConfirmed {
                rule: Rule::R1,
                invoice_ids: vec![id(1)],
                fee_amount: Yen::ZERO
            }
        );
    }

    #[test]
    fn r1_suggests_the_oldest_when_several_invoices_have_the_paid_amount() {
        let invoices = [
            invoice(1, 55_000, (2026, 11, 30)),
            invoice(2, 55_000, (2026, 10, 31)),
        ];
        assert_eq!(
            decide(Yen(55_000), known(&invoices, false), &config()),
            Decision::Suggested {
                rule: Rule::R1,
                invoice_ids: vec![id(2)],
                fee_amount: Yen::ZERO,
                reason: SuggestReason::SameAmountInvoices { count: 2 },
            }
        );
    }

    #[test]
    fn r2_confirms_the_only_combination() {
        let invoices = [
            invoice(1, 110_000, (2026, 9, 30)),
            invoice(2, 55_000, (2026, 10, 31)),
            invoice(3, 33_000, (2026, 11, 30)),
        ];
        assert_eq!(
            decide(Yen(165_000), known(&invoices, false), &config()),
            Decision::AutoConfirmed {
                rule: Rule::R2,
                invoice_ids: vec![id(1), id(2)],
                fee_amount: Yen::ZERO
            }
        );
    }

    #[test]
    fn r2_suggests_the_oldest_combination_when_there_are_several() {
        let invoices = [
            invoice(1, 110_000, (2026, 9, 30)),
            invoice(2, 55_000, (2026, 11, 30)),
            invoice(3, 55_000, (2026, 10, 31)),
        ];
        assert_eq!(
            decide(Yen(165_000), known(&invoices, false), &config()),
            Decision::Suggested {
                rule: Rule::R2,
                invoice_ids: vec![id(1), id(3)],
                fee_amount: Yen::ZERO,
                reason: SuggestReason::SeveralCombinations,
            }
        );
    }

    #[test]
    fn r2_is_skipped_above_the_invoice_limit() {
        let invoices = [
            invoice(1, 10_000, (2026, 9, 30)),
            invoice(2, 20_000, (2026, 10, 31)),
            invoice(3, 40_000, (2026, 11, 30)),
        ];
        let config = MatchingConfig {
            max_subset_invoices: 2,
            ..MatchingConfig::default()
        };
        assert_eq!(
            decide(Yen(30_000), known(&invoices, false), &config),
            Decision::Unmatched {
                reason: UnmatchedReason::NoCandidate,
                subset_search_skipped: true
            }
        );
    }

    #[test]
    fn r3_suggests_an_invoice_short_by_a_transfer_fee() {
        let invoices = [invoice(1, 110_000, (2026, 10, 31))];
        assert_eq!(
            decide(Yen(109_560), known(&invoices, true), &config()),
            Decision::Suggested {
                rule: Rule::R3,
                invoice_ids: vec![id(1)],
                fee_amount: Yen(440),
                reason: SuggestReason::FeeDeducted,
            }
        );
    }

    #[test]
    fn r3_applies_to_combinations_too() {
        let invoices = [
            invoice(1, 110_000, (2026, 9, 30)),
            invoice(2, 55_000, (2026, 10, 31)),
        ];
        assert_eq!(
            decide(Yen(164_340), known(&invoices, true), &config()),
            Decision::Suggested {
                rule: Rule::R3,
                invoice_ids: vec![id(1), id(2)],
                fee_amount: Yen(660),
                reason: SuggestReason::FeeDeducted,
            }
        );
    }

    #[test]
    fn r3_needs_a_customer_that_deducts_fees() {
        let invoices = [invoice(1, 110_000, (2026, 10, 31))];
        assert_eq!(
            decide(Yen(109_560), known(&invoices, false), &config()),
            Decision::Unmatched {
                reason: UnmatchedReason::NoCandidate,
                subset_search_skipped: false
            }
        );
    }

    #[test]
    fn r3_reports_several_explanations() {
        // 109,780 is 110,000 - 220 and also 110,110 - 330.
        let invoices = [
            invoice(1, 110_000, (2026, 10, 31)),
            invoice(2, 110_110, (2026, 11, 30)),
        ];
        assert_eq!(
            decide(Yen(109_780), known(&invoices, true), &config()),
            Decision::Suggested {
                rule: Rule::R3,
                invoice_ids: vec![id(1)],
                fee_amount: Yen(220),
                reason: SuggestReason::SeveralCombinations,
            }
        );
    }

    #[test]
    fn r3_never_uses_a_fee_that_is_not_configured() {
        let invoices = [invoice(1, 110_000, (2026, 10, 31))];
        assert_eq!(
            decide(Yen(109_999), known(&invoices, true), &config()),
            Decision::Unmatched {
                reason: UnmatchedReason::NoCandidate,
                subset_search_skipped: false
            }
        );
    }

    #[test]
    fn r4_suggests_the_oldest_invoice_with_the_paid_amount() {
        let invoices = [
            invoice(1, 88_000, (2026, 11, 30)),
            invoice(2, 88_000, (2026, 10, 31)),
            invoice(3, 99_000, (2026, 9, 30)),
        ];
        assert_eq!(
            decide(
                Yen(88_000),
                Payer::Unknown {
                    same_amount_invoices: &invoices
                },
                &config()
            ),
            Decision::Suggested {
                rule: Rule::R4,
                invoice_ids: vec![id(2)],
                fee_amount: Yen::ZERO,
                reason: SuggestReason::PayerUnknown {
                    candidate_invoice_ids: vec![id(2), id(1)]
                },
            }
        );
    }

    #[test]
    fn unknown_payer_without_a_same_amount_invoice_is_unmatched() {
        assert_eq!(
            decide(
                Yen(88_000),
                Payer::Unknown {
                    same_amount_invoices: &[]
                },
                &config()
            ),
            Decision::Unmatched {
                reason: UnmatchedReason::PayerUnknown,
                subset_search_skipped: false
            }
        );
    }

    #[test]
    fn known_payer_without_open_invoices_is_unmatched() {
        assert_eq!(
            decide(Yen(88_000), known(&[], true), &config()),
            Decision::Unmatched {
                reason: UnmatchedReason::NoCandidate,
                subset_search_skipped: false
            }
        );
    }

    #[test]
    fn allocate_takes_the_fee_off_the_first_invoice() {
        assert_eq!(
            allocate(&[(id(1), Yen(110_000)), (id(2), Yen(55_000))], Yen(660)),
            Some(vec![(id(1), Yen(109_340)), (id(2), Yen(55_000))])
        );
        assert_eq!(allocate(&[(id(1), Yen(440))], Yen(440)), None);
        assert_eq!(allocate(&[(id(1), Yen(440))], Yen(-1)), None);
        assert_eq!(allocate(&[], Yen::ZERO), None);
    }

    #[test]
    fn decision_serializes_with_its_reason() {
        let decision = Decision::Suggested {
            rule: Rule::R3,
            invoice_ids: vec![id(1)],
            fee_amount: Yen(440),
            reason: SuggestReason::FeeDeducted,
        };
        assert_eq!(
            serde_json::to_value(&decision).unwrap(),
            serde_json::json!({
                "outcome": "suggested",
                "rule": "R3",
                "invoice_ids": ["00000000-0000-0000-0000-000000000001"],
                "fee_amount": 440,
                "reason": { "kind": "fee_deducted" },
            })
        );
    }

    /// Small amounts in multiples of 1,000 make equal amounts and equal sums frequent.
    fn arb_invoices() -> impl Strategy<Value = Vec<OpenInvoice>> {
        prop::collection::vec((1i64..=12, 1u8..=28), 0..10).prop_map(|rows| {
            rows.into_iter()
                .enumerate()
                .map(|(n, (thousands, day))| {
                    invoice(n as u128 + 1, thousands * 1_000, (2026, 10, day))
                })
                .collect()
        })
    }

    fn arb_amount() -> impl Strategy<Value = Yen> {
        prop_oneof![
            (1i64..=40).prop_map(|t| Yen(t * 1_000)),
            (
                (1i64..=40),
                prop::sample::select(MatchingConfig::DEFAULT_FEES.to_vec())
            )
                .prop_map(|(t, fee)| Yen(t * 1_000 - fee)),
        ]
    }

    /// The number of non-empty subsets of the invoices that add up to `target`, by brute force.
    fn count_subsets(invoices: &[OpenInvoice], target: Yen) -> usize {
        (1u32..(1 << invoices.len()))
            .filter(|mask| {
                let sum: Yen = invoices
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, inv)| inv.amount)
                    .sum();
                sum == target
            })
            .count()
    }

    proptest! {
        /// Whatever is confirmed or suggested is made of distinct open invoices that add up to the payment
        /// plus the fee, and the allocation of the cash adds up to the payment.
        #[test]
        fn proposals_add_up(
            invoices in arb_invoices(),
            amount in arb_amount(),
            fee_deducted in any::<bool>(),
        ) {
            let config = config();
            let (rule, invoice_ids, fee) = match decide(amount, known(&invoices, fee_deducted), &config) {
                Decision::AutoConfirmed { rule, invoice_ids, fee_amount } => (rule, invoice_ids, fee_amount),
                Decision::Suggested { rule, invoice_ids, fee_amount, .. } => (rule, invoice_ids, fee_amount),
                Decision::Unmatched { .. } => return Ok(()),
            };
            let distinct: HashSet<_> = invoice_ids.iter().collect();
            prop_assert_eq!(distinct.len(), invoice_ids.len());

            let chosen: Vec<(InvoiceId, Yen)> = invoice_ids
                .iter()
                .map(|id| (*id, invoices.iter().find(|i| i.id == *id).expect("an invoice of the input").amount))
                .collect();
            let billed: Yen = chosen.iter().map(|(_, a)| *a).sum();
            prop_assert_eq!(billed, amount + fee);

            if rule == Rule::R3 {
                prop_assert!(fee_deducted);
                prop_assert!(config.fee_amounts.contains(&fee));
            } else {
                prop_assert_eq!(fee, Yen::ZERO);
            }

            let parts = allocate(&chosen, fee).expect("the fee fits the first invoice");
            prop_assert_eq!(parts.iter().map(|(_, a)| *a).sum::<Yen>(), amount);
            prop_assert!(parts.iter().all(|(_, a)| *a > Yen::ZERO));
        }

        /// A match is confirmed without a person only when it is the single explanation under its rule:
        /// one invoice with the paid amount (R1 wins over combinations), or otherwise one combination (R2).
        #[test]
        fn auto_confirmation_means_a_unique_explanation(invoices in arb_invoices(), amount in arb_amount()) {
            let same_amount = invoices.iter().filter(|i| i.amount == amount).count();
            let subsets = count_subsets(&invoices, amount);
            match decide(amount, known(&invoices, true), &config()) {
                Decision::AutoConfirmed { rule: Rule::R1, fee_amount, .. } => {
                    prop_assert_eq!(fee_amount, Yen::ZERO);
                    prop_assert_eq!(same_amount, 1);
                }
                Decision::AutoConfirmed { rule, fee_amount, .. } => {
                    prop_assert_eq!(rule, Rule::R2);
                    prop_assert_eq!(fee_amount, Yen::ZERO);
                    prop_assert_eq!((same_amount, subsets), (0, 1));
                }
                Decision::Suggested { rule: Rule::R1, .. } => prop_assert!(same_amount > 1),
                Decision::Suggested { rule: Rule::R2, .. } => {
                    prop_assert_eq!(same_amount, 0);
                    prop_assert!(subsets > 1);
                }
                Decision::Suggested { .. } | Decision::Unmatched { .. } => prop_assert_eq!(subsets, 0),
            }
        }

        /// The decision does not depend on the order in which the invoices were read.
        #[test]
        fn decision_ignores_input_order(
            invoices in arb_invoices().prop_shuffle(),
            amount in arb_amount(),
            fee_deducted in any::<bool>(),
        ) {
            let mut sorted = invoices.clone();
            sorted.sort_by_key(|i| i.id);
            prop_assert_eq!(
                decide(amount, known(&invoices, fee_deducted), &config()),
                decide(amount, known(&sorted, fee_deducted), &config())
            );
        }
    }
}
