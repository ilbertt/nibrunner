//! What this host says about itself: the record it keeps per app, what a host has room for, and
//! the document those become.

pub mod build_report;
pub mod capacity;
pub mod instance_record;
pub mod routes;
pub mod versions;

pub use instance_record::*;
