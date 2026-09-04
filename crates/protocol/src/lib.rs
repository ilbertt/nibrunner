//! The wire documents a host exchanges with the nibrun control plane, ported field for field from
//! `packages/protocol` in the nibrun repository. The wire format is JSON and only JSON: ISO
//! strings for timestamps, hex for digests, numbers for sizes. Every field keeps the name it has
//! on the wire, and unknown properties are tolerated on the way in: the two sides are deployed by
//! different pipelines, so a field the newer side sends is not a reason to reject its message.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::panic, clippy::expect_used))]

#[macro_use]
mod wire;
mod control;
mod domain;

pub use control::*;
pub use domain::*;
pub use wire::*;

#[cfg(test)]
mod tests;
