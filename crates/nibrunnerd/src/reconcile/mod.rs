//! Pull, converge, report. Nothing here is driven by a command: desired state describes a world,
//! this compares it with what the host is observed to be doing, and the difference is the work.

pub mod idle;
pub mod instances;
pub mod network;
pub mod plan;
pub mod volumes;

pub use plan::*;
