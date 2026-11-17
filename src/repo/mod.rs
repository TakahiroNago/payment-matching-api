//! SQL. Every statement goes through sqlx's macros, which check it against the schema at compile time (against
//! a live database, or against the `.sqlx` directory when `SQLX_OFFLINE=true`).
//!
//! The functions take a connection, so the caller decides what runs in one transaction.

pub mod customers;
pub mod invoices;
pub mod matches;
pub mod payments;
