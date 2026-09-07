use protocol::{HostCapacity, InstanceResources, InstanceState};

use crate::services::report::InstanceRecord;

const BYTES_PER_MIB: u64 = 1_048_576;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FilesystemSpace {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

pub const HOST_BASELINE_MIB: u64 = 640;

pub fn guest_memory_mib(host_memory_mib: u64, storage_cache_mib: u64) -> u64 {
    host_memory_mib
        .saturating_sub(storage_cache_mib)
        .saturating_sub(HOST_BASELINE_MIB)
}

const HOLDS_NOTHING: [InstanceState; 3] =
    [InstanceState::Idle, InstanceState::Stopped, InstanceState::Failed];

pub fn committed_resources(records: &[InstanceRecord]) -> Vec<InstanceResources> {
    records
        .iter()
        .filter(|record| !HOLDS_NOTHING.contains(&record.state))
        .map(|record| record.resources)
        .collect()
}

fn committed_memory_mib(committed: &[InstanceResources]) -> u64 {
    committed.iter().map(|entry| u64::from(entry.memory_mib)).sum()
}

pub fn allocatable_capacity(
    capacity: &HostCapacity,
    committed: &[InstanceResources],
    available_cache_bytes: u64,
) -> HostCapacity {
    let used_vcpu: u32 = committed.iter().map(|entry| entry.vcpu_count).sum();
    HostCapacity {
        vcpu_count: capacity.vcpu_count.saturating_sub(used_vcpu),
        memory_mib: capacity
            .memory_mib
            .saturating_sub(committed_memory_mib(committed)),
        cache_bytes: available_cache_bytes.min(capacity.cache_bytes),
    }
}

pub fn memory_shortfall_mib(
    host_memory_mib: u64,
    committed: &[InstanceResources],
    wanted: &InstanceResources,
) -> u64 {
    (committed_memory_mib(committed) + u64::from(wanted.memory_mib)).saturating_sub(host_memory_mib)
}

pub fn read_host_memory_mib() -> u64 {
    read_meminfo_kib("MemTotal:").map_or(0, |kib| (kib * 1024) / BYTES_PER_MIB)
}

#[cfg(target_os = "linux")]
fn read_meminfo_kib(field: &str) -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    meminfo
        .lines()
        .find(|line| line.starts_with(field))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
}

#[cfg(not(target_os = "linux"))]
fn read_meminfo_kib(_field: &str) -> Option<u64> {
    None
}

pub fn read_vcpu_count() -> u32 {
    std::thread::available_parallelism().map_or(1, |count| count.get() as u32)
}

pub fn read_filesystem_space(directory: &std::path::Path) -> std::io::Result<FilesystemSpace> {
    #[cfg(unix)]
    {
        let stats = nix::sys::statvfs::statvfs(directory)
            .map_err(|error| std::io::Error::from_raw_os_error(error as i32))?;
        let block_size = stats.fragment_size() as u64;
        Ok(FilesystemSpace {
            total_bytes: stats.blocks() as u64 * block_size,
            available_bytes: stats.blocks_available() as u64 * block_size,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        Ok(FilesystemSpace::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::instance_record;
    use protocol::{AppId, DEFAULT_INSTANCE_RESOURCES};

    const APP_MEMORY_MIB: u64 = DEFAULT_INSTANCE_RESOURCES.memory_mib as u64;
    const NEIGHBOURS_THAT_FIT: u64 = 3;
    const HOST_MEMORY_MIB: u64 = APP_MEMORY_MIB * (NEIGHBOURS_THAT_FIT + 1);

    fn neighbours(count: u64, state: InstanceState) -> Vec<InstanceRecord> {
        (0..count)
            .map(|index| {
                instance_record(|record| {
                    record.app_id = AppId::parse(format!("neighbour-{index}")).unwrap();
                    record.state = state;
                })
            })
            .collect()
    }

    fn shortfall(count: u64, state: InstanceState) -> u64 {
        memory_shortfall_mib(
            HOST_MEMORY_MIB,
            &committed_resources(&neighbours(count, state)),
            &DEFAULT_INSTANCE_RESOURCES,
        )
    }

    #[test]
    fn a_host_has_room_for_one_more_microvm_until_it_does_not() {
        assert_eq!(shortfall(0, InstanceState::Running), 0);
        assert_eq!(shortfall(NEIGHBOURS_THAT_FIT, InstanceState::Running), 0);
        assert_eq!(
            shortfall(NEIGHBOURS_THAT_FIT + 1, InstanceState::Running),
            APP_MEMORY_MIB
        );
        assert_eq!(shortfall(NEIGHBOURS_THAT_FIT * 2, InstanceState::Idle), 0);
    }

    #[test]
    fn memory_the_host_needs_is_not_memory_a_guest_may_be_given() {
        const HOST_MIB: u64 = 7779;
        const CACHE_MIB: u64 = 2048;
        assert_eq!(
            guest_memory_mib(HOST_MIB, CACHE_MIB),
            HOST_MIB - CACHE_MIB - HOST_BASELINE_MIB
        );
        let roomier = guest_memory_mib(HOST_MIB, CACHE_MIB);
        let tighter = guest_memory_mib(HOST_MIB, CACHE_MIB + 1024);
        assert_eq!(roomier - tighter, 1024);
        assert_eq!(guest_memory_mib(512, CACHE_MIB), 0);
        let fits = roomier / APP_MEMORY_MIB;
        assert!(
            memory_shortfall_mib(
                roomier,
                &committed_resources(&neighbours(fits, InstanceState::Running)),
                &DEFAULT_INSTANCE_RESOURCES
            ) > 0
        );
    }

    #[test]
    fn allocatable_is_what_is_left_once_every_booted_app_is_taken_off() {
        let capacity = HostCapacity {
            vcpu_count: 4,
            memory_mib: 8192,
            cache_bytes: 1000,
        };
        let booted = vec![DEFAULT_INSTANCE_RESOURCES, DEFAULT_INSTANCE_RESOURCES];
        assert_eq!(
            allocatable_capacity(&capacity, &booted, 400),
            HostCapacity {
                vcpu_count: 4 - DEFAULT_INSTANCE_RESOURCES.vcpu_count * 2,
                memory_mib: 8192 - APP_MEMORY_MIB * 2,
                cache_bytes: 400,
            }
        );
        let small = HostCapacity {
            vcpu_count: 1,
            memory_mib: APP_MEMORY_MIB,
            cache_bytes: 1000,
        };
        assert_eq!(
            allocatable_capacity(
                &small,
                &[InstanceResources {
                    vcpu_count: 4,
                    memory_mib: 8192
                }],
                400
            ),
            HostCapacity {
                vcpu_count: 0,
                memory_mib: 0,
                cache_bytes: 400
            }
        );
    }

    #[test]
    fn every_state_with_a_microvm_behind_it_still_holds_what_it_was_given() {
        let states = [
            InstanceState::Running,
            InstanceState::Starting,
            InstanceState::Unhealthy,
            InstanceState::Stopping,
            InstanceState::Idle,
            InstanceState::Stopped,
            InstanceState::Failed,
        ];
        let records: Vec<InstanceRecord> = states
            .iter()
            .map(|state| instance_record(|record| record.state = *state))
            .collect();
        assert_eq!(committed_resources(&records).len(), 4);
        assert!(committed_resources(&[instance_record(|record| {
            record.state = InstanceState::Idle;
            record.on_request = true;
        })])
        .is_empty());
    }
}
