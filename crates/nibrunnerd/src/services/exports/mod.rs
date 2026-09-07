//! Handing a tenant their data back.
//!
//! An export is a bundle — the tenant's filesystem, the binary that was running on it, and the
//! environment it ran under — built from a checkpoint rather than from the live disk, and put
//! somewhere the owner can fetch it.
//!
//! The order is the guarantee. The guest is frozen, because only its kernel can checkpoint the
//! ext4 journal and `debugfs` never replays one — an unfrozen filesystem is missing recent
//! metadata however durable the storage under it is. Then the checkpoint, which captures *now*.
//! Then the freeze is released: everything whose cost scales with the tenant's data — the read,
//! the archive, the upload — runs against that pinned view while they are writing again.

pub mod bundle;
pub mod freeze;
pub mod reader;
pub mod store;
