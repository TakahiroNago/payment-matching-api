//! One id type per table, so that an invoice id cannot be passed where a payment id is expected.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[cfg_attr(feature = "server", derive(sqlx::Type), sqlx(transparent))]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// A new UUID v7: time-ordered, so `ORDER BY id` is creation order.
            #[cfg(feature = "server")]
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(CustomerId);
id_type!(AliasId);
id_type!(InvoiceId);
id_type!(BatchId);
id_type!(PaymentId);
id_type!(MatchId);
