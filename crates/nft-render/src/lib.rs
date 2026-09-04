//! The host's network as a function of state: one small integer per app that every per-app
//! resource derives from, the nftables ruleset rendered whole, and the parsers for what `nft`
//! answers when asked what the kernel holds.

// A test that unwraps and panics *is* its own failure report, and one written to avoid saying
// so reads worse than the assertion it replaced. The lint stays on for everything else.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::panic, clippy::expect_used))]

pub mod counters;
pub mod firewall;
pub mod slot;
pub mod tables;

pub use counters::*;
pub use firewall::*;
pub use slot::*;
pub use tables::*;
