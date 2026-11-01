#![cfg_attr(test, allow(clippy::unwrap_used, clippy::panic, clippy::expect_used))]

mod budget;
mod database;
mod error;
mod values;

pub use database::Database;
pub use error::QueryError;

#[cfg(test)]
mod tests;
