//! Pure domain logic: nothing here touches the database or the network.

pub mod ids;
pub mod matching;
pub mod money;
pub mod normalize;

pub use ids::{AliasId, BatchId, CustomerId, InvoiceId, MatchId, PaymentId};
pub use money::Yen;

// Dates travel as `YYYY-MM-DD`; timestamps as RFC 3339.
time::serde::format_description!(pub iso_date, Date, "[year]-[month]-[day]");
