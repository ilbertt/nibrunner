//! The contract between the host and the guest image, as `apps/runtime` in the nibrun repository
//! fixes it: the drive order, the kernel command line, the vsock ports, the `instance.env` format
//! and the byte formats spoken over the vsock. None of this is the host's to change; the C runtime
//! and the guest image are consumed as they are.

// A test that unwraps and panics *is* its own failure report, and one written to avoid saying
// so reads worse than the assertion it replaced. The lint stays on for everything else.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::panic, clippy::expect_used))]

pub mod control;
pub mod filesystem;
pub mod firecracker;
pub mod instance_env;
pub mod logs;
pub mod paths;
pub mod vsock;

pub use firecracker::*;
