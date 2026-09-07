#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VmStatus {
    pub loaded: bool,
    pub active: bool,
    pub failed: bool,
    pub started_this_boot: bool,
    pub exit_code: Option<i32>,
}

pub const UNKNOWN_VM: VmStatus = VmStatus {
    loaded: false,
    active: false,
    failed: false,
    started_this_boot: false,
    exit_code: None,
};
