//! The contract between the host and the guest image, as `apps/runtime` in the nibrun repository
//! fixes it: the drive order, the kernel command line, the vsock ports, the `instance.env` format
//! and the byte formats spoken over the vsock. None of this is the host's to change; the C runtime
//! and the guest image are consumed as they are.

pub mod control;
pub mod filesystem;
pub mod firecracker;
pub mod instance_env;
pub mod logs;
pub mod vsock;

pub use firecracker::*;
