use guest_contract::firecracker::MachineConfig;

pub(crate) const CGROUP_PARENT: &str = "nibrunner-jailer";
const CPU_PERIOD_US: u64 = 100_000;
const BYTES_PER_MIB: u64 = 1024 * 1024;
const VMM_OVERHEAD_MIB: u64 = 64;
const MEMORY_OVERHEAD_DIVISOR: u64 = 8;

pub(crate) struct VmLimits {
    cpu_quota_us: u64,
    memory_max_bytes: u64,
}

impl VmLimits {
    pub(crate) fn for_machine(machine: &MachineConfig) -> std::io::Result<Self> {
        if machine.vcpu_count == 0 || machine.mem_size_mib == 0 {
            return Err(std::io::Error::other(
                "a microVM needs positive CPU and memory resources",
            ));
        }
        let guest_bytes = u64::from(machine.mem_size_mib) * BYTES_PER_MIB;
        // The memory controller charges VMM allocations and KVM page tables alongside guest RAM.
        let memory_max_bytes =
            guest_bytes + VMM_OVERHEAD_MIB * BYTES_PER_MIB + guest_bytes / MEMORY_OVERHEAD_DIVISOR;
        Ok(Self {
            cpu_quota_us: u64::from(machine.vcpu_count) * CPU_PERIOD_US,
            memory_max_bytes,
        })
    }

    pub(crate) fn configure(&self, command: &mut tokio::process::Command) {
        command
            .arg("--cgroup")
            .arg(format!("cpu.max={} {CPU_PERIOD_US}", self.cpu_quota_us))
            .arg("--cgroup")
            .arg(format!("memory.max={}", self.memory_max_bytes))
            .arg("--cgroup")
            .arg("memory.swap.max=0");
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn cgroup_path(app_id: &protocol::AppId) -> std::path::PathBuf {
    std::path::Path::new("/sys/fs/cgroup")
        .join(CGROUP_PARENT)
        .join(super::jailer::jail_id(app_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(vcpu_count: u32, mem_size_mib: u32) -> MachineConfig {
        MachineConfig {
            vcpu_count,
            mem_size_mib,
            smt: false,
        }
    }

    #[test]
    fn host_limits_follow_guest_resources_with_room_for_vmm_and_kernel_memory() {
        for (cpus, memory, ceiling) in [(1, 128, 208), (2, 256, 352), (32, 4096, 4672)] {
            let limits = VmLimits::for_machine(&machine(cpus, memory)).unwrap();
            assert_eq!(limits.cpu_quota_us, u64::from(cpus) * 100_000);
            assert_eq!(limits.memory_max_bytes, ceiling * BYTES_PER_MIB);
            let mut command = tokio::process::Command::new("jailer");
            limits.configure(&mut command);
            let args: Vec<_> = command
                .as_std()
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect();
            assert_eq!(
                args,
                [
                    "--cgroup",
                    &format!("cpu.max={} 100000", u64::from(cpus) * 100_000),
                    "--cgroup",
                    &format!("memory.max={}", ceiling * BYTES_PER_MIB),
                    "--cgroup",
                    "memory.swap.max=0"
                ]
            );
        }
    }

    #[test]
    fn resource_limits_reject_zero_and_do_not_overflow_at_the_protocols_integer_boundary() {
        assert!(VmLimits::for_machine(&machine(0, 256)).is_err());
        assert!(VmLimits::for_machine(&machine(1, 0)).is_err());
        let limits = VmLimits::for_machine(&machine(u32::MAX, u32::MAX)).unwrap();
        assert_eq!(limits.cpu_quota_us, u64::from(u32::MAX) * CPU_PERIOD_US);
        assert!(limits.memory_max_bytes > u64::from(u32::MAX) * BYTES_PER_MIB);
    }
}
