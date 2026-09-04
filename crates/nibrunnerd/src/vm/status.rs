//! What the host can see of one microVM without asking the guest anything.
//!
//! nibrun's agent reads this off `systemctl show`. This daemon parents its own Firecracker
//! processes, so the same four facts come from a pidfile it wrote and a process it can signal:
//! `loaded` is a record on disk, `active` is a process alive under that pid, `failed` is a
//! recorded non-zero exit, and `started_this_boot` compares the boot id in the pidfile with the
//! host's own, which is what `InactiveExitTimestampMonotonic` was standing in for.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VmStatus {
    pub loaded: bool,
    pub active: bool,
    pub failed: bool,
    /// Distinguishes a VM stopped this boot from one that has never run since the host booted.
    pub started_this_boot: bool,
    pub exit_code: Option<i32>,
}

pub const UNKNOWN_VM: VmStatus =
    VmStatus { loaded: false, active: false, failed: false, started_this_boot: false, exit_code: None };
