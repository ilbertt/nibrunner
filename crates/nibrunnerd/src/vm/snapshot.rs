//! What a snapshot is, what it may be loaded against, and when one may be taken at all.

use std::path::{Path, PathBuf};

use protocol::{AppId, DeploymentId};
use serde::{Deserialize, Serialize};

use crate::json_store::read_json;
use crate::report::capacity::FilesystemSpace;
use crate::services::VmError;

pub const SNAPSHOT_STATE_FILENAME: &str = "vmstate";
pub const SNAPSHOT_MEMORY_FILENAME: &str = "memory";

/// Written last and consumed first, which is what makes a restore happen **at most once**: the
/// daemon writes it only once the two files beside it are complete, and a start deletes it before
/// it asks the VMM to load them. A start that finds no stamp is a cold boot — so a daemon that
/// died between the load and the cleanup leaves a microVM that boots off its disk rather than one
/// that resumes from a snapshot the disk has since moved past.
///
/// **At most once is a security invariant and not only a crash-safety one.** The guest kernel is
/// built with `CONFIG_VMGENID=y`, so Firecracker updates the generation id and injects its
/// interrupt before vCPUs resume, and Linux reseeds the kernel CRNG off it — `getrandom()` and
/// `/dev/urandom` are fresh on the far side of a wake. Nothing reseeds the PRNG state already
/// resident in tenant memory: OpenSSL's `RAND` buffer, a runtime's per-thread generator, a nonce
/// already drawn. One snapshot restored twice is therefore the same randomness in two live VMs,
/// which is key reuse with no symptom to notice it by. That is what is being traded away by
/// anyone who makes a snapshot into a reusable warm-start template, and it is a trade to make
/// deliberately rather than to discover.
pub const SNAPSHOT_STAMP_FILENAME: &str = "stamp.json";

/// What a snapshot may be loaded against, and nothing else.
///
/// Firecracker restores a microVM's drives, tap and vsock from paths recorded in the vmstate, and
/// never asks whether what sits at those paths is what sat there when the snapshot was taken. A
/// kernel, rootfs or artifact image swapped underneath one restores a guest whose page cache
/// describes bytes that are gone — silent corruption rather than a refusal. Every field here
/// names something that can move while a microVM sleeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotStamp {
    pub deployment_id: DeploymentId,
    /// The guest image directory is read through one path, so the kernel and rootfs move without
    /// the path changing.
    pub guest_image_version: String,
    pub host_boot_id: String,
    /// The tap, the addresses, the MAC and the data device all derive from this one number.
    pub slot: u32,
}

/// Ordered by what a reader most wants to hear first, and the only place a field is named twice.
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

/// The two moments a microVM survives being snapshotted and its tenant does not survive being
/// woken. Enforced here rather than left to whatever decides an app should sleep, because a
/// policy is the thing that changes: the obvious next idea is to snapshot an app right after
/// creating it to make cold starts cheap, and that idea has to fail here rather than in
/// production.
///
/// **A stop already in flight.** `clock_realtime` advances the clocksource rather than applying a
/// wall-clock offset, so `CLOCK_MONOTONIC` moves forward with it. The SIGTERM-to-SIGKILL deadline
/// the guest's supervisor holds is a monotonic instant, so it lands in the past on the first poll
/// after a wake and the tenant is killed for a shutdown it was in the middle of handling.
///
/// **A guest that has not finished booting.** Firecracker injects the VMGenID interrupt before
/// vCPUs resume, and a kernel snapshotted before its interrupt handling was in place can crash
/// taking it. This guest boots with `panic=1 reboot=k`, so that crash is the end of the microVM
/// rather than a line on its console. `ever_healthy` is the bar because it is the host's only
/// first-hand evidence: the tenant accepted a connection, which is far past the window that is
/// dangerous, and it stays true for a guest that has since gone unhealthy — being unwell is not
/// the same as never having booted.
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

/// What the disk a snapshot goes on has to keep free for everything on it that is not one, and
/// slack for a filesystem nobody wants at 100%.
const DISK_RESERVE_GIB: u64 = 8;

const BYTES_PER_MIB: u64 = 1_048_576;
const BYTES_PER_GIB: u64 = 1_073_741_824;
const DISK_RESERVE_BYTES: u64 = DISK_RESERVE_GIB * BYTES_PER_GIB;

/// The disk the snapshots are on, as the decision to write another one needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotDisk {
    pub total_bytes: u64,
    pub available_bytes: u64,
    /// What a storage cache on this disk may take, which is disk it has not taken yet rather than
    /// disk it is using.
    pub cache_bytes: u64,
    /// What the snapshots already here hold.
    pub snapshot_bytes: u64,
}

/// A memory file is exactly the guest's RAM. The vmstate beside it is kilobytes, and is slack.
pub fn snapshot_bytes_for(memory_mib: u32) -> u64 {
    u64::from(memory_mib) * BYTES_PER_MIB
}

/// All the disk snapshots may ever hold together, floored at none for a host with no room at all.
pub fn snapshot_budget(disk: &SnapshotDisk) -> u64 {
    disk.total_bytes.saturating_sub(disk.cache_bytes).saturating_sub(DISK_RESERVE_BYTES)
}

fn gibibytes(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / BYTES_PER_GIB as f64)
}

/// Why this host will not keep another snapshot, or `None` when it will.
///
/// A snapshot is the size of the app's configured memory rather than of the default, so a host
/// carrying a few multi-gigabyte apps runs out of disk on its own. What that costs is not the
/// sleeping apps: the snapshots share a disk with the cache every app on the host runs through,
/// so a cost optimisation nobody opted into would be taking down apps that never sleep. A refusal
/// here costs one app one cold start it was not going to take anyway.
///
/// Both bounds are needed and neither implies the other. The budget is against the disk's *size*,
/// because a cache fills lazily: free space on a fresh host is space already promised. The floor
/// is against what is *actually* free, because the promise is not the only claim.
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

/// What the snapshots on this host hold, measured rather than derived from the daemon's own
/// record of which apps are asleep: one an earlier daemon left behind occupies the same disk as
/// one this daemon wrote, and the disk is what is running out.
pub fn read_snapshot_bytes(snapshot_dir: &Path) -> u64 {
    fn walk(directory: &Path, held: &mut u64) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            match entry.metadata() {
                // A file gone between the listing and the stat is a snapshot being discarded.
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
    let FilesystemSpace { total_bytes, available_bytes } =
        crate::report::capacity::read_filesystem_space(snapshot_dir)?;
    Ok(SnapshotDisk {
        total_bytes,
        available_bytes,
        cache_bytes,
        snapshot_bytes: read_snapshot_bytes(snapshot_dir),
    })
}

/// That the snapshot beside this stamp describes the restore being asked for. A snapshot that
/// fails here is one nothing may ever load, which is why the caller discards it rather than
/// leaving it for a later start to find and take as an instruction.
pub fn ensure_loadable(stamp_path: &Path, expected: &SnapshotStamp) -> Result<(), VmError> {
    let stored: Option<SnapshotStamp> = read_json(stamp_path).ok().flatten();
    let Some(stored) = stored else {
        return Err(VmError::SnapshotUnusable { reason: "this host kept none".into() });
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

    /// Each of these is a way the files a snapshot names get replaced underneath it, and
    /// Firecracker restores drive paths out of the vmstate without checking any of them.
    #[test]
    fn every_way_a_snapshot_stops_being_loadable_is_named() {
        assert_eq!(drift_from(&stamp(), &stamp()), None);
        let redeployed = SnapshotStamp { deployment_id: DeploymentId::parse("dep-2").unwrap(), ..stamp() };
        assert!(drift_from(&redeployed, &stamp()).unwrap().contains("deployed again"));
        let newer_image = SnapshotStamp { guest_image_version: "6.1.181-x".into(), ..stamp() };
        assert!(drift_from(&newer_image, &stamp()).unwrap().contains("guest image"));
        let rebooted = SnapshotStamp { host_boot_id: "another".into(), ..stamp() };
        assert!(drift_from(&rebooted, &stamp()).unwrap().contains("rebooted"));
        let moved = SnapshotStamp { slot: 8, ..stamp() };
        assert!(drift_from(&moved, &stamp()).unwrap().contains("another slot"));
    }

    /// Both refusals are about surviving the wake, not about whether sleeping is a good idea.
    #[test]
    fn the_moments_a_microvm_must_not_be_snapshotted() {
        let sleepable = SleepSubject { stop_requested: false, desired_running: true, ever_healthy: true };
        assert_eq!(refusal_to_sleep(Some(sleepable)), None);
        assert!(refusal_to_sleep(Some(SleepSubject { stop_requested: true, ..sleepable }))
            .unwrap()
            .contains("asked to stop"));
        assert!(refusal_to_sleep(Some(SleepSubject { desired_running: false, ..sleepable }))
            .unwrap()
            .contains("asked to stop"));
        assert!(refusal_to_sleep(Some(SleepSubject { ever_healthy: false, ..sleepable }))
            .unwrap()
            .contains("finished booting"));
        assert!(refusal_to_sleep(None).is_some());
    }

    const GIB: u64 = 1_073_741_824;

    /// An app host as a fleet runs it: 110 GiB of disk, 70 GiB of it the storage cache's.
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
            refusal_for_disk(&SnapshotDisk { snapshot_bytes: asleep, ..host_disk() }, snapshot_bytes_for(256)),
            None
        );
        // A snapshot is the size of the app's configured memory, which is what makes a bound
        // necessary at all: a few of these are the disk.
        assert!(refusal_for_disk(&SnapshotDisk { snapshot_bytes: 30 * GIB, ..host_disk() }, snapshot_bytes_for(4096))
            .unwrap()
            .contains("already hold"));
        // The budget is against the size of the disk, not what is free on it today.
        let cold_cache = SnapshotDisk { available_bytes: 105 * GIB, snapshot_bytes: 30 * GIB, ..host_disk() };
        assert!(refusal_for_disk(&cold_cache, snapshot_bytes_for(4096)).is_some());
        // And the other way: the budget is untouched and the disk is gone anyway.
        let crowded = SnapshotDisk { available_bytes: 8 * GIB, snapshot_bytes: GIB, ..host_disk() };
        assert!(refusal_for_disk(&crowded, snapshot_bytes_for(256)).unwrap().contains("every app"));
        assert_eq!(snapshot_budget(&SnapshotDisk { total_bytes: 0, ..host_disk() }), 0);
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
        // An app this host kept no snapshot of is refused rather than guessed at.
        assert!(matches!(
            ensure_loadable(&paths.stamp_path, &stamp()),
            Err(VmError::SnapshotUnusable { .. })
        ));
        write_json(&paths.stamp_path, &stamp()).unwrap();
        ensure_loadable(&paths.stamp_path, &stamp()).unwrap();
        let rebooted = SnapshotStamp { host_boot_id: "after-a-reboot".into(), ..stamp() };
        let refused = ensure_loadable(&paths.stamp_path, &rebooted).unwrap_err();
        assert!(refused.message().contains("rebooted"));
    }
}
