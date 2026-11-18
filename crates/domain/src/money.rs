//! Amounts in whole yen. Never a floating-point number.

use std::iter::Sum;
use std::ops::{Add, Sub};

use serde::{Deserialize, Serialize};

/// An amount of yen. The newtype keeps amounts apart from counts, line numbers and other integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::Type), sqlx(transparent))]
#[serde(transparent)]
pub struct Yen(pub i64);

impl Yen {
    pub const ZERO: Yen = Yen(0);

    /// The largest amount the API accepts for one invoice or payment. Sums over the few invoices of a match
    /// stay far below `i64::MAX`, so the arithmetic below cannot overflow.
    pub const MAX: Yen = Yen(999_999_999_999);

    /// Whether this is acceptable as the amount of an invoice or a payment.
    #[must_use]
    pub fn is_valid_amount(self) -> bool {
        self > Yen::ZERO && self <= Yen::MAX
    }
}

impl Add for Yen {
    type Output = Yen;
    fn add(self, rhs: Yen) -> Yen {
        Yen(self.0 + rhs.0)
    }
}

impl Sub for Yen {
    type Output = Yen;
    fn sub(self, rhs: Yen) -> Yen {
        Yen(self.0 - rhs.0)
    }
}

impl Sum for Yen {
    fn sum<I: Iterator<Item = Yen>>(iter: I) -> Yen {
        iter.fold(Yen::ZERO, Add::add)
    }
}
