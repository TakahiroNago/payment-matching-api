//! Matches bank payments against open invoices (入金消込).
//!
//! - [`domain`]: types, payer-name normalization and the matching rules. No I/O. A crate of its own
//!   (`crates/domain`), which the browser demo compiles to WebAssembly.
//! - [`repo`]: SQL, checked against the schema at compile time.
//! - [`service`]: transactions that combine the two.
//! - [`http`]: routing, request validation and the error format.

pub mod config;
pub mod error;
pub mod http;
pub mod repo;
pub mod service;

pub use payment_matching_domain as domain;
