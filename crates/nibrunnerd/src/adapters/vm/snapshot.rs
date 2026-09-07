use std::path::{Path, PathBuf};

use protocol::{AppId, DeploymentId};
use serde::{Deserialize, Serialize};

use crate::json_store::read_json;
use crate::ports::VmError;
use crate::services::report::capacity::FilesystemSpace;

pub const SNAPSHOT_STATE_FILENAME: &str = "vmstate";
pub const SNAPSHOT_MEMORY_FILENAME: &str = "memory";

pub const SNAPSHOT_STAMP_FILENAME: &str = "stamp.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotStamp {
    pub deployment_id: DeploymentId,
    pub guest_image_version: String,
    pub host_boot_id: String,
    pub slot: u32,
}

fn drift_reason(stored: &SnapshotStamp, expected: &SnapshotStamp) -> Option<&'static str> {
    if stored.deployment_id != expected.deployment_id {
        return Some("the app has been deployed again since");
    }
    if stored.guest_image_version != expected.guest_image_version {
        return Some("the guest image has changed");
    }
    if stored.host_boot_id != expected.host_boot_id {
        return Some("the host has rebooted");
    }
    if stored.slot != expected.slot {
        return Some("the app has moved to another slot");
    }
    None
}

pub fn drift_from(stored: &SnapshotStamp, expected: &SnapshotStamp) -> Option<String> {
    drift_reason(stored, expected).map(str::to_string)
}

pub fn refusal_to_sleep(subject: Option<SleepSubject>) -> Option<&'static str> {
    let Some(subject) = subject else {
        return Some("this host holds no record of it");
    };
    if subject.stop_requested || !subject.desired_running {
        return Some("it has already been asked to stop");
    }
    if !subject.ever_healthy {
        return Some("it has never answered, so it may not have finished booting");
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SleepSubject {
    pub stop_requested: bool,
    pub desired_running: bool,
    pub ever_healthy: bool,
}

const DISK_RESERVE_GIB: u64 = 8;

const BYTES_PER_MIB: u64 = 1_048_576;
const BYTES_PER_GIB: u64 = 1_073_741_824;
const DISK_RESERVE_BYTES: u64 = DISK_RESERVE_GIB * BYTES_PER_GIB;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotDisk {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub cache_bytes: u64,
    pub snapshot_bytes: u64,
}

pub fn snapshot_bytes_for(memory_mib: u32) -> u64 {
    u64::from(memory_mib) * BYTES_PER_MIB
}

pub fn snapshot_budget(disk: &SnapshotDisk) -> u64 {
    disk.total_bytes
        .saturating_sub(disk.cache_bytes)
        .saturating_sub(DISK_RESERVE_BYTES)
}

fn gibibytes(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / BYTES_PER_GIB as f64)
}

pub fn refusal_for_disk(disk: &SnapshotDisk, wanted_bytes: u64) -> Option<String> {
    let budget = snapshot_budget(disk);
    if disk.snapshot_bytes + wanted_bytes > budget {
        return Some(format!(
            "snapshots on this host may hold {} and already hold {}",
            gibibytes(budget),
            gibibytes(disk.snapshot_bytes)
        ));
    }
    if disk.available_bytes.saturating_sub(wanted_bytes) < DISK_RESERVE_BYTES {
        return Some(format!(
            "the disk it would be written to has {} left, which the filesystem every app runs from needs more than it does",
            gibibytes(disk.available_bytes)
        ));
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPaths {
    pub directory: PathBuf,
    pub state_path: PathBuf,
    pub memory_path: PathBuf,
    pub stamp_path: PathBuf,
}

pub fn snapshot_paths(snapshot_dir: &Path, app_id: &AppId) -> SnapshotPaths {
    let directory = snapshot_dir.join(app_id.as_str());
    SnapshotPaths {
        state_path: directory.join(SNAPSHOT_STATE_FILENAME),
        memory_path: directory.join(SNAPSHOT_MEMORY_FILENAME),
        stamp_path: directory.join(SNAPSHOT_STAMP_FILENAME),
        directory,
    }
}

pub fn read_snapshot_bytes(snapshot_dir: &Path) -> u64 {
    fn walk(directory: &Path, held: &mut u64) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            match entry.metadata() {
                Ok(info) if info.is_file() => *held += info.len(),
                Ok(info) if info.is_dir() => walk(&entry.path(), held),
                _ => {}
            }
        }
    }
    let mut held = 0;
    walk(snapshot_dir, &mut held);
    held
}

pub fn measure_snapshot_disk(snapshot_dir: &Path, cache_bytes: u64) -> std::io::Result<SnapshotDisk> {
    crate::json_store::make_directory(snapshot_dir, 0o700)?;
    let FilesystemSpace {
        total_bytes,
        available_bytes,
    } = crate::services::report::capacity::read_filesystem_space(snapshot_dir)?;
    Ok(SnapshotDisk {
        total_bytes,
        available_bytes,
        cache_bytes,
        snapshot_bytes: read_snapshot_bytes(snapshot_dir),
    })
}

pub fn ensure_loadable(stamp_path: &Path, expected: &SnapshotStamp) -> Result<(), VmError> {
    let stored: Option<SnapshotStamp> = read_json(stamp_path).ok().flatten();
    let Some(stored) = stored else {
        return Err(VmError::SnapshotUnusable {
            reason: "this host kept none".into(),
        });
    };
    match drift_from(&stored, expected) {
        None => Ok(()),
        Some(reason) => Err(VmError::SnapshotUnusable { reason }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_store::write_json;
    use crate::test_support::{app_id, deployment_id};

    fn stamp() -> SnapshotStamp {
        SnapshotStamp {
            deployment_id: deployment_id(),
            guest_image_version: "6.1.180-98db6df338f0".into(),
            host_boot_id: "b6b8f0d2-0000-4000-8000-000000000001".into(),
            slot: 7,
        }
    }

    #[test]
    fn a_snapshot_is_three_files_under_the_app_it_belongs_to() {
        let paths = snapshot_paths(Path::new("/data/snapshots"), &app_id());
        assert_eq!(paths.directory, Path::new("/data/snapshots/app-1"));
        assert_eq!(paths.stamp_path, paths.directory.join(SNAPSHOT_STAMP_FILENAME));
        assert!(paths.state_path.starts_with(&paths.directory));
        assert!(paths.memory_path.starts_with(&paths.directory));
    }

    #[test]
    fn every_way_a_snapshot_stops_being_loadable_is_named() {
        assert_eq!(drift_from(&stamp(), &stamp()), None);
        let redeployed = SnapshotStamp {
            deployment_id: DeploymentId::parse("dep-2").unwrap(),
            ..stamp()
        };
        assert!(drift_from(&redeployed, &stamp())
            .unwrap()
            .contains("deployed again"));
        let newer_image = SnapshotStamp {
            guest_image_version: "6.1.181-x".into(),
            ..stamp()
        };
        assert!(drift_from(&newer_image, &stamp())
            .unwrap()
            .contains("guest image"));
        let rebooted = SnapshotStamp {
            host_boot_id: "another".into(),
            ..stamp()
        };
        assert!(drift_from(&rebooted, &stamp()).unwrap().contains("rebooted"));
        let moved = SnapshotStamp { slot: 8, ..stamp() };
        assert!(drift_from(&moved, &stamp()).unwrap().contains("another slot"));
    }

    #[test]
    fn the_moments_a_microvm_must_not_be_snapshotted() {
        let sleepable = SleepSubject {
            stop_requested: false,
            desired_running: true,
            ever_healthy: true,
        };
        assert_eq!(refusal_to_sleep(Some(sleepable)), None);
        assert!(refusal_to_sleep(Some(SleepSubject {
            stop_requested: true,
            ..sleepable
        }))
        .unwrap()
        .contains("asked to stop"));
        assert!(refusal_to_sleep(Some(SleepSubject {
            desired_running: false,
            ..sleepable
        }))
        .unwrap()
        .contains("asked to stop"));
        assert!(refusal_to_sleep(Some(SleepSubject {
            ever_healthy: false,
            ..sleepable
        }))
        .unwrap()
        .contains("finished booting"));
        assert!(refusal_to_sleep(None).is_some());
    }

    const GIB: u64 = 1_073_741_824;

    fn host_disk() -> SnapshotDisk {
        SnapshotDisk {
            total_bytes: 110 * GIB,
            available_bytes: 38 * GIB,
            cache_bytes: 70 * GIB,
            snapshot_bytes: 0,
        }
    }

    #[test]
    fn what_snapshots_may_hold_on_a_host() {
        let asleep = u64::from(nft_render::SLOT_COUNT) * snapshot_bytes_for(256);
        assert!(asleep < snapshot_budget(&host_disk()));
        assert_eq!(
            refusal_for_disk(
                &SnapshotDisk {
                    snapshot_bytes: asleep,
                    ..host_disk()
                },
                snapshot_bytes_for(256)
            ),
            None
        );
        assert!(refusal_for_disk(
            &SnapshotDisk {
                snapshot_bytes: 30 * GIB,
                ..host_disk()
            },
            snapshot_bytes_for(4096)
        )
        .unwrap()
        .contains("already hold"));
        let cold_cache = SnapshotDisk {
            available_bytes: 105 * GIB,
            snapshot_bytes: 30 * GIB,
            ..host_disk()
        };
        assert!(refusal_for_disk(&cold_cache, snapshot_bytes_for(4096)).is_some());
        let crowded = SnapshotDisk {
            available_bytes: 8 * GIB,
            snapshot_bytes: GIB,
            ..host_disk()
        };
        assert!(refusal_for_disk(&crowded, snapshot_bytes_for(256))
            .unwrap()
            .contains("every app"));
        assert_eq!(
            snapshot_budget(&SnapshotDisk {
                total_bytes: 0,
                ..host_disk()
            }),
            0
        );
    }

    #[test]
    fn what_snapshots_hold_is_measured_from_the_directory_they_are_in() {
        let directory = tempfile::tempdir().unwrap();
        for (app, size) in [("inst-1", 1024), ("inst-2", 512)] {
            let held = directory.path().join(app);
            std::fs::create_dir_all(&held).unwrap();
            std::fs::write(held.join(SNAPSHOT_MEMORY_FILENAME), vec![b'x'; size]).unwrap();
        }
        assert_eq!(read_snapshot_bytes(directory.path()), 1536);
        assert_eq!(read_snapshot_bytes(&directory.path().join("nowhere")), 0);
    }

    #[test]
    fn what_a_wake_checks_before_anything_is_started() {
        let directory = tempfile::tempdir().unwrap();
        let paths = snapshot_paths(directory.path(), &app_id());
        assert!(matches!(
            ensure_loadable(&paths.stamp_path, &stamp()),
            Err(VmError::SnapshotUnusable { .. })
        ));
        write_json(&paths.stamp_path, &stamp()).unwrap();
        ensure_loadable(&paths.stamp_path, &stamp()).unwrap();
        let rebooted = SnapshotStamp {
            host_boot_id: "after-a-reboot".into(),
            ..stamp()
        };
        let refused = ensure_loadable(&paths.stamp_path, &rebooted).unwrap_err();
        assert!(refused.message().contains("rebooted"));
    }
}
