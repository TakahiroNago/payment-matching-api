//! The automatic matching of one import, without a database: the browser demo.
//!
//! The rules and the payer-name normalization are the server's own (`payment-matching-domain`). What this
//! crate adds is the part the server does in SQL: which customer a name belongs to, which invoices are open,
//! and what a confirmed match does to the lines before and after it. [`simulate`] follows the server's import
//! (`service::import` and `service::auto`) step by step, and a test of the server
//! (`the_browser_demo_decides_like_the_server`) holds the two to the same results.
//!
//! Not here, because it needs the database: idempotent imports, concurrent confirmations, the history of
//! cancelled matches, confirming and cancelling by hand.

use std::collections::HashMap;

use payment_matching_domain::matching::{
    Decision, MatchingConfig, OpenInvoice, Payer, Rule, SuggestReason, UnmatchedReason, decide,
};
use payment_matching_domain::normalize::normalize_payer_name;
use payment_matching_domain::{InvoiceId, Yen};
use serde::{Deserialize, Serialize};
use time::Date;
use time::macros::format_description;
use uuid::Uuid;
use wasm_bindgen::prelude::*;

/// Customers, their open invoices, and the payments of one import, in the order of the bank statement.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub customers: Vec<CustomerIn>,
    pub invoices: Vec<InvoiceIn>,
    pub payments: Vec<PaymentIn>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomerIn {
    /// The reading of the customer's name in kana: its first payer name.
    pub name_kana: String,
    /// Whether the customer pays the invoice amount less the transfer fee (enables rule R3).
    #[serde(default)]
    pub fee_deducted: bool,
    /// Other payer names registered for the customer.
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvoiceIn {
    /// Index into `customers`.
    pub customer: usize,
    pub amount: i64,
    /// `YYYY-MM-DD`.
    pub due_date: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentIn {
    pub payer_name: String,
    pub amount: i64,
}

/// What is wrong with the scenario, in the server's terms (`errors: [{ field, code }]`).
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub field: String,
    pub code: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Outcome {
    /// Every payer name of every customer, in the order they would be registered.
    pub names: Vec<NameOut>,
    /// One per payment, in the order of the scenario.
    pub payments: Vec<PaymentOut>,
    /// One per invoice, in the order of the scenario: its state after the import.
    pub invoices: Vec<InvoiceOut>,
    /// The transfer fees R3 tries.
    pub fee_amounts: Vec<i64>,
}

#[derive(Debug, Serialize)]
pub struct NameOut {
    pub customer: usize,
    pub raw: String,
    /// `null` when the name cannot be normalized.
    pub normalized: Option<String>,
    /// `kana` for the customer's own name, `alias` otherwise.
    pub source: &'static str,
    /// `not_normalizable`, or `taken` when the normalized name already identifies a customer. The server
    /// refuses such a registration; here the name is left out.
    pub problem: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct PaymentOut {
    /// The payer name as the rules see it; `null` when it cannot be normalized.
    pub normalized: Option<String>,
    /// The customer the name belongs to (index into `customers`).
    pub customer: Option<usize>,
    /// `matched`, `suggested` or `unmatched`: the status after the whole import.
    pub status: &'static str,
    pub decision: DecisionOut,
    /// The later payment (index into `payments`) that cleared an invoice this payment was suggested, which
    /// cancelled the suggestion and left this payment unmatched.
    pub lost_to: Option<usize>,
}

/// A [`Decision`] with indexes into `invoices` in place of ids.
#[derive(Debug, Serialize)]
pub struct DecisionOut {
    /// `auto_confirmed`, `suggested` or `unmatched`.
    pub outcome: &'static str,
    pub rule: Option<&'static str>,
    pub invoices: Vec<usize>,
    pub fee_amount: i64,
    /// Why the match is only suggested (`same_amount_invoices`, `several_combinations`, `fee_deducted`,
    /// `payer_unknown`) or why there is none (`payer_unknown`, `no_candidate`).
    pub reason: Option<&'static str>,
    /// `same_amount_invoices`: how many open invoices have the paid amount.
    pub count: Option<usize>,
    /// R4: the invoices of the paid amount, oldest first.
    pub candidates: Vec<usize>,
    pub subset_search_skipped: bool,
}

#[derive(Debug, Serialize)]
pub struct InvoiceOut {
    /// `open` or `cleared`.
    pub status: &'static str,
    /// The payment (index into `payments`) whose confirmed match cleared it.
    pub cleared_by: Option<usize>,
}

struct Customer {
    fee_deducted: bool,
}

struct Invoice {
    customer: usize,
    open: OpenInvoice,
    cleared_by: Option<usize>,
}

/// Decides the payments of the scenario one after another, as the server does when it imports them.
///
/// # Errors
/// When an amount is out of range, a date does not parse or an invoice names a customer that is not there.
pub fn simulate(scenario: &Scenario) -> Result<Outcome, Vec<Problem>> {
    let config = MatchingConfig::default();
    let mut problems = Vec::new();

    // Payer names: the kana name of each customer, then its aliases. One normalized name, one customer.
    let mut names = Vec::new();
    let mut owner: HashMap<String, usize> = HashMap::new();
    let mut customers = Vec::new();
    for (index, customer) in scenario.customers.iter().enumerate() {
        customers.push(Customer {
            fee_deducted: customer.fee_deducted,
        });
        let own = std::iter::once((customer.name_kana.as_str(), "kana"));
        let others = customer
            .aliases
            .iter()
            .map(|alias| (alias.as_str(), "alias"));
        for (raw, source) in own.chain(others) {
            let normalized = normalize_payer_name(raw.trim());
            let problem = match &normalized {
                None => Some("not_normalizable"),
                Some(name) if owner.contains_key(name) => Some("taken"),
                Some(name) => {
                    owner.insert(name.clone(), index);
                    None
                }
            };
            names.push(NameOut {
                customer: index,
                raw: raw.to_owned(),
                normalized,
                source,
                problem,
            });
        }
    }

    // Invoices. The id is the position, so "older id" means "registered earlier", as with the server's
    // time-ordered ids.
    let mut invoices = Vec::new();
    for (index, invoice) in scenario.invoices.iter().enumerate() {
        let field = |name: &str| format!("invoices[{index}].{name}");
        if invoice.customer >= customers.len() {
            problems.push(Problem {
                field: field("customer"),
                code: "not_found",
            });
        }
        if !Yen(invoice.amount).is_valid_amount() {
            problems.push(Problem {
                field: field("amount"),
                code: "out_of_range",
            });
        }
        let format = format_description!("[year]-[month]-[day]");
        match Date::parse(&invoice.due_date, format) {
            Ok(due_date) => invoices.push(Invoice {
                customer: invoice.customer,
                open: OpenInvoice {
                    id: invoice_id(index),
                    amount: Yen(invoice.amount),
                    due_date,
                },
                cleared_by: None,
            }),
            Err(_) => problems.push(Problem {
                field: field("due_date"),
                code: "invalid",
            }),
        }
    }
    for (index, payment) in scenario.payments.iter().enumerate() {
        if !Yen(payment.amount).is_valid_amount() {
            problems.push(Problem {
                field: format!("payments[{index}].amount"),
                code: "out_of_range",
            });
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }

    let mut payments: Vec<PaymentOut> = Vec::new();
    for (index, payment) in scenario.payments.iter().enumerate() {
        let amount = Yen(payment.amount);
        let normalized = normalize_payer_name(&payment.payer_name);
        let customer = normalized
            .as_ref()
            .and_then(|name| owner.get(name))
            .copied();

        let decision = match customer {
            Some(customer) => {
                let open: Vec<OpenInvoice> = invoices
                    .iter()
                    .filter(|i| i.customer == customer && i.cleared_by.is_none())
                    .map(|i| i.open.clone())
                    .collect();
                let payer = Payer::Known {
                    fee_deducted: customers[customer].fee_deducted,
                    open_invoices: &open,
                };
                decide(amount, payer, &config)
            }
            None => {
                // The server reads only as many as the decision reports, the oldest.
                let mut same_amount: Vec<OpenInvoice> = invoices
                    .iter()
                    .filter(|i| i.cleared_by.is_none() && i.open.amount == amount)
                    .map(|i| i.open.clone())
                    .collect();
                same_amount.sort_by_key(|i| (i.due_date, i.id));
                same_amount.truncate(MatchingConfig::MAX_R4_CANDIDATES);
                let payer = Payer::Unknown {
                    same_amount_invoices: &same_amount,
                };
                decide(amount, payer, &config)
            }
        };

        let decision = describe(&decision);
        let status = match decision.outcome {
            "auto_confirmed" => "matched",
            "suggested" => "suggested",
            _ => "unmatched",
        };
        if status == "matched" {
            // A confirmed match clears its invoices. Suggestions of earlier payments that name one of them
            // could never be confirmed: they are cancelled and those payments are unmatched again.
            for cleared in &decision.invoices {
                invoices[*cleared].cleared_by = Some(index);
            }
            for earlier in &mut payments {
                if earlier.status == "suggested"
                    && earlier
                        .decision
                        .invoices
                        .iter()
                        .any(|named| decision.invoices.contains(named))
                {
                    earlier.status = "unmatched";
                    earlier.lost_to = Some(index);
                }
            }
        }
        payments.push(PaymentOut {
            normalized,
            customer,
            status,
            decision,
            lost_to: None,
        });
    }

    Ok(Outcome {
        names,
        payments,
        invoices: invoices
            .iter()
            .map(|invoice| InvoiceOut {
                status: if invoice.cleared_by.is_some() {
                    "cleared"
                } else {
                    "open"
                },
                cleared_by: invoice.cleared_by,
            })
            .collect(),
        fee_amounts: config.fee_amounts.iter().map(|fee| fee.0).collect(),
    })
}

fn invoice_id(index: usize) -> InvoiceId {
    InvoiceId(Uuid::from_u128(index as u128))
}

fn invoice_index(id: InvoiceId) -> usize {
    usize::try_from(id.0.as_u128()).unwrap_or(usize::MAX)
}

fn describe(decision: &Decision) -> DecisionOut {
    let indexes = |ids: &[InvoiceId]| ids.iter().copied().map(invoice_index).collect();
    match decision {
        Decision::AutoConfirmed {
            rule,
            invoice_ids,
            fee_amount,
        } => DecisionOut {
            outcome: "auto_confirmed",
            rule: Some(rule_name(*rule)),
            invoices: indexes(invoice_ids),
            fee_amount: fee_amount.0,
            reason: None,
            count: None,
            candidates: Vec::new(),
            subset_search_skipped: false,
        },
        Decision::Suggested {
            rule,
            invoice_ids,
            fee_amount,
            reason,
        } => {
            let (kind, count, candidates) = match reason {
                SuggestReason::SameAmountInvoices { count } => {
                    ("same_amount_invoices", Some(*count), Vec::new())
                }
                SuggestReason::SeveralCombinations => ("several_combinations", None, Vec::new()),
                SuggestReason::FeeDeducted => ("fee_deducted", None, Vec::new()),
                SuggestReason::PayerUnknown {
                    candidate_invoice_ids,
                } => ("payer_unknown", None, indexes(candidate_invoice_ids)),
            };
            DecisionOut {
                outcome: "suggested",
                rule: Some(rule_name(*rule)),
                invoices: indexes(invoice_ids),
                fee_amount: fee_amount.0,
                reason: Some(kind),
                count,
                candidates,
                subset_search_skipped: false,
            }
        }
        Decision::Unmatched {
            reason,
            subset_search_skipped,
        } => DecisionOut {
            outcome: "unmatched",
            rule: None,
            invoices: Vec::new(),
            fee_amount: 0,
            reason: Some(match reason {
                UnmatchedReason::PayerUnknown => "payer_unknown",
                UnmatchedReason::NoCandidate => "no_candidate",
            }),
            count: None,
            candidates: Vec::new(),
            subset_search_skipped: *subset_search_skipped,
        },
    }
}

fn rule_name(rule: Rule) -> &'static str {
    rule.as_str()
}

// ---------------------------------------------------------------------------------------------------------
// The JavaScript side: JSON in, JSON out.
// ---------------------------------------------------------------------------------------------------------

/// Forwards Rust panics to `console.error`.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// Takes a [`Scenario`] as JSON. Returns `{"ok": Outcome}`, or `{"errors": [{ field, code }]}` when the
/// scenario is not acceptable.
#[wasm_bindgen]
#[must_use]
pub fn decide_scenario(scenario_json: &str) -> String {
    let reply = match serde_json::from_str::<Scenario>(scenario_json) {
        Ok(scenario) => match simulate(&scenario) {
            Ok(outcome) => serde_json::json!({ "ok": outcome }),
            Err(problems) => serde_json::json!({ "errors": problems }),
        },
        Err(error) => serde_json::json!({
            "errors": [{ "field": "scenario", "code": "malformed", "detail": error.to_string() }]
        }),
    };
    reply.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario(json: serde_json::Value) -> Scenario {
        serde_json::from_value(json).unwrap()
    }

    /// The four payments of the README.
    #[test]
    fn the_example_of_the_readme() {
        let outcome = simulate(&scenario(serde_json::json!({
            "customers": [{ "name_kana": "カブシキガイシャ ヤマダショウジ", "fee_deducted": true }],
            "invoices": [
                { "customer": 0, "amount": 110_000, "due_date": "2026-09-30" },
                { "customer": 0, "amount": 55_000, "due_date": "2026-10-31" },
                { "customer": 0, "amount": 33_000, "due_date": "2026-11-30" },
                { "customer": 0, "amount": 220_000, "due_date": "2026-11-30" },
            ],
            "payments": [
                { "payer_name": "ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ", "amount": 165_000 },
                { "payer_name": "ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ", "amount": 219_560 },
                { "payer_name": "ﾔﾏﾀﾞ ｵｵｻｶｼﾃﾝ", "amount": 33_000 },
                { "payer_name": "ﾌﾒｲ ｼﾖｳｶｲ", "amount": 5_000 },
            ],
        })))
        .unwrap();

        assert_eq!(
            outcome.names[0].normalized.as_deref(),
            Some("ヤマダシヨウジ")
        );
        let summary: Vec<_> = outcome
            .payments
            .iter()
            .map(|p| {
                (
                    p.status,
                    p.decision.rule,
                    p.decision.invoices.clone(),
                    p.decision.fee_amount,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("matched", Some("R2"), vec![0, 1], 0),
                ("suggested", Some("R3"), vec![3], 440),
                ("suggested", Some("R4"), vec![2], 0),
                ("unmatched", None, vec![], 0),
            ]
        );
        assert_eq!(outcome.payments[1].decision.reason, Some("fee_deducted"));
        assert_eq!(outcome.payments[3].decision.reason, Some("payer_unknown"));
        let statuses: Vec<_> = outcome.invoices.iter().map(|i| i.status).collect();
        assert_eq!(statuses, vec!["cleared", "cleared", "open", "open"]);
    }

    #[test]
    fn a_later_line_that_clears_an_invoice_takes_the_suggestion_of_an_earlier_line() {
        let outcome = simulate(&scenario(serde_json::json!({
            "customers": [{ "name_kana": "ヤマダショウジ" }],
            "invoices": [
                { "customer": 0, "amount": 55_000, "due_date": "2026-10-31" },
                { "customer": 0, "amount": 55_000, "due_date": "2026-11-30" },
                { "customer": 0, "amount": 33_000, "due_date": "2026-11-30" },
            ],
            "payments": [
                { "payer_name": "ﾔﾏﾀﾞｼﾖｳｼﾞ", "amount": 55_000 },
                { "payer_name": "ﾔﾏﾀﾞｼﾖｳｼﾞ", "amount": 143_000 },
            ],
        })))
        .unwrap();

        let first = &outcome.payments[0];
        assert_eq!(first.decision.outcome, "suggested");
        assert_eq!(first.decision.count, Some(2));
        assert_eq!(first.status, "unmatched");
        assert_eq!(first.lost_to, Some(1));
        assert_eq!(outcome.payments[1].status, "matched");
        assert!(outcome.invoices.iter().all(|i| i.cleared_by == Some(1)));
    }

    #[test]
    fn a_name_identifies_one_customer_and_bad_input_is_reported() {
        let outcome = simulate(&scenario(serde_json::json!({
            "customers": [
                { "name_kana": "ヤマダショウジ", "aliases": ["ﾔﾏﾀﾞ ｵｵｻｶｼﾃﾝ", "）（"] },
                { "name_kana": "スズキコウギョウ", "aliases": ["ヤマダ オオサカシテン"] },
            ],
            "invoices": [],
            "payments": [{ "payer_name": "ﾔﾏﾀﾞ ｵｵｻｶｼﾃﾝ", "amount": 1 }],
        })))
        .unwrap();
        let problems: Vec<_> = outcome.names.iter().map(|n| n.problem).collect();
        assert_eq!(
            problems,
            vec![None, None, Some("not_normalizable"), None, Some("taken")]
        );
        assert_eq!(outcome.payments[0].customer, Some(0));
        assert_eq!(outcome.payments[0].decision.reason, Some("no_candidate"));

        let problems = simulate(&scenario(serde_json::json!({
            "customers": [],
            "invoices": [{ "customer": 0, "amount": 0, "due_date": "2026-13-01" }],
            "payments": [{ "payer_name": "x", "amount": -1 }],
        })))
        .unwrap_err();
        let found: Vec<_> = problems
            .iter()
            .map(|p| (p.field.as_str(), p.code))
            .collect();
        assert_eq!(
            found,
            vec![
                ("invoices[0].customer", "not_found"),
                ("invoices[0].amount", "out_of_range"),
                ("invoices[0].due_date", "invalid"),
                ("payments[0].amount", "out_of_range"),
            ]
        );

        let reply: serde_json::Value = serde_json::from_str(&decide_scenario("{")).unwrap();
        assert_eq!(reply["errors"][0]["code"], "malformed");
    }
}
