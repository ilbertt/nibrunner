//! The host's networking: which slot an app holds, the tap that slot names, and the ruleset that
//! decides what reaches a guest.

pub mod allocator;
pub mod firewall;
pub mod tap;
