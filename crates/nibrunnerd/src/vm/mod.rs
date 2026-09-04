//! The microVM: how one is described to Firecracker, how it is started and stopped, what its
//! process looks like from outside, and what a snapshot of it may be loaded against.

pub mod artifacts;
pub mod firecracker_api;
pub mod manager;
pub mod process;
pub mod snapshot;
pub mod status;

pub use status::{VmStatus, UNKNOWN_VM};
