/// How a microVM's process ended: with a code of its own, or under a signal from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmExit {
    Code(i32),
    Signal(i32),
}

// Firecracker v1.16.1 catches SIGBUS and exits with FcExitCode::SIGBUS rather than under the signal.
pub(crate) const FIRECRACKER_SIGBUS_EXIT_CODE: i32 = 149;
pub(crate) const FIRECRACKER_RUNTIME_ERROR_EXIT_CODE: i32 = 1;

impl VmExit {
    pub fn is_memory_fault(self) -> bool {
        matches!(
            self,
            Self::Code(FIRECRACKER_SIGBUS_EXIT_CODE) | Self::Signal(libc::SIGBUS)
        )
    }

    pub fn describe(self) -> String {
        if self.is_memory_fault() {
            return "exited after a host memory fault (SIGBUS)".into();
        }
        match self {
            VmExit::Code(code) => format!("exited with exit code {code}"),
            VmExit::Signal(signal) => {
                let name = nix::sys::signal::Signal::try_from(signal)
                    .map(|known| format!(" ({})", known.as_str()))
                    .unwrap_or_default();
                format!("was killed by signal {signal}{name}")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VmStatus {
    pub loaded: bool,
    pub active: bool,
    pub failed: bool,
    pub started_this_boot: bool,
    pub exit: Option<VmExit>,
}

pub const UNKNOWN_VM: VmStatus = VmStatus {
    loaded: false,
    active: false,
    failed: false,
    started_this_boot: false,
    exit: None,
};

impl VmStatus {
    pub(crate) fn host_failure(self) -> bool {
        !self.active
            && self.failed
            && self.started_this_boot
            && self.exit.is_some_and(|exit| {
                exit.is_memory_fault() || exit == VmExit::Code(FIRECRACKER_RUNTIME_ERROR_EXIT_CODE)
            })
    }
}

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
        assert_eq!(unknown.exit, None);
    }

    #[test]
    fn a_status_that_differs_in_any_field_is_a_different_status() {
        assert_ne!(
            VmStatus {
                exit: Some(VmExit::Code(0)),
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
                exit: None,
            }
        );
    }

    #[test]
    fn an_exit_says_how_the_process_ended_and_names_a_signal_the_host_knows() {
        assert_eq!(VmExit::Code(137).describe(), "exited with exit code 137");
        assert_eq!(VmExit::Signal(9).describe(), "was killed by signal 9 (SIGKILL)");
        assert_eq!(VmExit::Signal(15).describe(), "was killed by signal 15 (SIGTERM)");
        assert_eq!(VmExit::Signal(200).describe(), "was killed by signal 200");
    }
}
