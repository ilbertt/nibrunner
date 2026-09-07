//! What fills the holes in `ports` on a real machine.
//!
//! Everything here touches something outside this process: the hypervisor, the kernel's network
//! tables, a block device, an object store, a tenant's own output, a socket somebody dialled.
//! None of it decides anything — `services` does that, and it does it through the traits next
//! door, which is why swapping any of these for a recording double leaves the reasoning intact.

pub mod artifact_store;
pub mod control_plane;
pub mod exec;
pub mod logs;
pub mod net;
pub mod proxy;
pub mod vm;
pub mod volumes;
