//! Matches bank payments against open invoices (入金消込).
//!
//! - [`domain`]: types, payer-name normalization and the matching rules. No I/O.
//! - [`repo`]: SQL, checked against the schema at compile time.
//! - [`service`]: transactions that combine the two.
//! - [`http`]: routing, request validation and the error format.

pub mod config;
pub mod domain;
pub mod error;
pub mod http;
pub mod repo;
pub mod service;
