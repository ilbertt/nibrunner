//! Browsing a tenant's filesystem, by asking the guest that has it mounted.
//!
//! The slot lookup is what scopes it: an app resolves to the single microVM this host runs for it,
//! so a path is only ever resolved inside the filesystem its own app owns — and inside the guest,
//! which is the only place that can answer without the volume's flush interval standing between
//! the tenant's last write and what somebody sees.

pub mod client;
pub mod reader;
