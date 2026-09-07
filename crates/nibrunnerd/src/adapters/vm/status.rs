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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_microvm_this_host_knows_nothing_about_claims_nothing_about_it() {
        let unknown = UNKNOWN_VM;
        assert_eq!(unknown, VmStatus::default());
        assert!(!unknown.loaded);
        assert!(!unknown.active);
        assert!(!unknown.failed);
        assert!(!unknown.started_this_boot);
        assert_eq!(unknown.exit_code, None);
    }

    #[test]
    fn a_status_that_differs_in_any_field_is_a_different_status() {
        assert_ne!(
            VmStatus {
                exit_code: Some(0),
                ..UNKNOWN_VM
            },
            UNKNOWN_VM
        );
        assert_ne!(
            VmStatus {
                active: true,
                ..UNKNOWN_VM
            },
            UNKNOWN_VM
        );
        assert_eq!(
            VmStatus {
                loaded: true,
                ..UNKNOWN_VM
            },
            VmStatus {
                loaded: true,
                active: false,
                failed: false,
                started_this_boot: false,
                exit_code: None,
            }
        );
    }
}
