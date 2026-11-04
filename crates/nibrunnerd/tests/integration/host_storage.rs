#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use nibrunnerd::config::{HostConfig, HostStorage, StorageFilesystem, VolumeBackend};
use nibrunnerd::install::{service_user, storage};

struct Disk {
    directory: tempfile::TempDir,
    device: PathBuf,
    mount_point: PathBuf,
}

fn run(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

impl Disk {
    fn blank() -> Self {
        super::require_root();
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let image = directory.path().join("disk.img");
        File::create(&image).unwrap().set_len(512 * 1024 * 1024).unwrap();
        let device = run(Command::new("losetup").args(["--find", "--show"]).arg(&image)).into();
        let mount_point = directory.path().join("data");
        Self {
            directory,
            device,
            mount_point,
        }
    }

    fn config(&self, filesystem: StorageFilesystem) -> HostConfig {
        let mut config = HostConfig::under(self.directory.path());
        config.snapshot_dir = self.mount_point.join("snapshots");
        config.host_storage = Some(HostStorage {
            device: self.device.clone(),
            mount_point: self.mount_point.clone(),
            filesystem,
        });
        config
    }

    fn unmount(&self) {
        run(Command::new("umount").arg(&self.mount_point));
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.mount_point).output();
        let _ = Command::new("losetup").arg("--detach").arg(&self.device).output();
    }
}

#[test]
fn host_storage_initializes_blank_devices_and_preserves_existing_filesystems_across_starts() {
    if !super::enabled() {
        return;
    }
    for filesystem in [StorageFilesystem::Ext4, StorageFilesystem::Xfs] {
        let disk = Disk::blank();
        let mut config = disk.config(filesystem);
        service_user::ensure(service_user::ZEROFS_USER).unwrap();
        config.volumes =
            VolumeBackend::Zerofs(Box::new(nibrunnerd::test_support::zerofs_settings(|settings| {
                settings.cache_dir = disk.mount_point.join("cache");
                settings.checkpoint_cache_dir = disk.mount_point.join("checkpoint-cache");
            })));
        assert!(storage::check(&config).is_err());
        storage::ensure_mounted(&config).unwrap();
        storage::ready(&config).unwrap();
        run(Command::new("runuser")
            .args(["--user", service_user::ZEROFS_USER, "--", "touch"])
            .arg(config.volumes.zerofs().unwrap().cache_dir.join("writable")));
        service_user::own(
            &config.snapshot_dir,
            service_user::ids(service_user::ZEROFS_USER).unwrap(),
        )
        .unwrap();
        service_user::own(&config.volumes.zerofs().unwrap().cache_dir, (0, 0)).unwrap();
        std::fs::write(disk.mount_point.join("preserved"), "existing data").unwrap();
        let uuid = run(Command::new("blkid")
            .args(["--probe", "--output", "value", "--match-tag", "UUID"])
            .arg(&disk.device));
        for _ in 0..2 {
            storage::ensure_mounted(&config).unwrap();
            storage::ready(&config).unwrap();
            assert_eq!(
                std::fs::read_to_string(disk.mount_point.join("preserved")).unwrap(),
                "existing data"
            );
            assert_eq!(
                run(Command::new("blkid")
                    .args(["--probe", "--output", "value", "--match-tag", "UUID"])
                    .arg(&disk.device)),
                uuid
            );
            disk.unmount();
            assert!(storage::ready(&config).is_err());
        }
        storage::ensure_mounted(&config).unwrap();
        let settings = config.volumes.zerofs().unwrap();
        let cache = std::fs::metadata(&settings.cache_dir).unwrap();
        let snapshots = std::fs::metadata(&config.snapshot_dir).unwrap();
        let checkpoint = std::fs::metadata(&settings.checkpoint_cache_dir).unwrap();
        assert_eq!(
            (cache.uid(), cache.gid()),
            service_user::ids(service_user::ZEROFS_USER).unwrap()
        );
        for metadata in [&cache, &snapshots, &checkpoint] {
            assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        }
        assert_eq!(snapshots.uid(), 0);
        assert_eq!(checkpoint.uid(), 0);
        disk.unmount();
        run(Command::new("losetup").arg("--detach").arg(&disk.device));
        // Loop detachment is lazy; udev can still hold the device open after losetup returns.
        run(Command::new("udevadm").args(["settle", "--timeout=10"]));
        let replacement = disk.directory.path().join("replacement.img");
        File::create(&replacement)
            .unwrap()
            .set_len(512 * 1024 * 1024)
            .unwrap();
        run(Command::new("losetup").arg(&disk.device).arg(&replacement));
        storage::prepare(&config).unwrap();
        assert!(storage::check(&config).is_err());
        run(Command::new("mount")
            .args(["--types", filesystem.as_str()])
            .arg(&disk.device)
            .arg(&disk.mount_point));
        storage::ready(&config).unwrap();
        assert!(config.snapshot_dir.is_dir());
        assert!(settings.cache_dir.is_dir());
        assert!(settings.checkpoint_cache_dir.is_dir());
        assert!(!disk.mount_point.join("preserved").exists());
        run(Command::new("runuser")
            .args(["--user", service_user::ZEROFS_USER, "--", "touch"])
            .arg(settings.cache_dir.join("writable")));
    }
}

#[test]
fn host_storage_refuses_unknown_data_partition_tables_and_mismatched_filesystems_without_writing() {
    if !super::enabled() {
        return;
    }
    let disk = Disk::blank();
    let mut file = File::options().write(true).open(&disk.device).unwrap();
    file.seek(SeekFrom::Start(400 * 1024 * 1024)).unwrap();
    file.write_all(b"unrecognized data").unwrap();
    file.sync_all().unwrap();
    let config = disk.config(StorageFilesystem::Ext4);
    assert!(storage::ensure_mounted(&config)
        .unwrap_err()
        .to_string()
        .contains("contains data"));
    assert!(!disk.mount_point.exists());
    let disk = Disk::blank();
    run(Command::new("mkswap").arg(&disk.device));
    assert!(storage::ensure_mounted(&disk.config(StorageFilesystem::Ext4))
        .unwrap_err()
        .to_string()
        .contains("signatures"));
    let disk = Disk::blank();
    let mut partitioner = Command::new("sfdisk")
        .arg(&disk.device)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    partitioner
        .stdin
        .take()
        .unwrap()
        .write_all(b"label: gpt\n,64M,L\n")
        .unwrap();
    assert!(partitioner.wait().unwrap().success());
    assert!(storage::ensure_mounted(&disk.config(StorageFilesystem::Ext4)).is_err());
    let disk = Disk::blank();
    storage::ensure_mounted(&disk.config(StorageFilesystem::Ext4)).unwrap();
    std::fs::write(disk.mount_point.join("preserved"), "kept").unwrap();
    disk.unmount();
    assert!(storage::ensure_mounted(&disk.config(StorageFilesystem::Xfs)).is_err());
    storage::ensure_mounted(&disk.config(StorageFilesystem::Ext4)).unwrap();
    assert_eq!(
        std::fs::read_to_string(disk.mount_point.join("preserved")).unwrap(),
        "kept"
    );
}

#[test]
fn host_storage_refuses_conflicting_mounts_nonempty_targets_and_symlinked_directories() {
    if !super::enabled() {
        return;
    }
    let disk = Disk::blank();
    let config = disk.config(StorageFilesystem::Ext4);
    std::fs::create_dir(&disk.mount_point).unwrap();
    std::fs::write(disk.mount_point.join("root-disk-data"), "kept").unwrap();
    assert!(storage::ensure_mounted(&config)
        .unwrap_err()
        .to_string()
        .contains("not empty"));
    std::fs::remove_file(disk.mount_point.join("root-disk-data")).unwrap();
    run(Command::new("mount")
        .args(["-t", "tmpfs", "tmpfs"])
        .arg(&disk.mount_point));
    assert!(storage::ensure_mounted(&config)
        .unwrap_err()
        .to_string()
        .contains("conflicting mount"));
    disk.unmount();
    storage::ensure_mounted(&config).unwrap();
    let mut elsewhere = config.clone();
    elsewhere.host_storage.as_mut().unwrap().mount_point = disk.directory.path().join("elsewhere");
    assert!(storage::ensure_mounted(&elsewhere)
        .unwrap_err()
        .to_string()
        .contains("conflicting mount"));
    std::os::unix::fs::symlink(disk.directory.path(), &config.snapshot_dir).unwrap();
    assert!(storage::ready(&config)
        .unwrap_err()
        .to_string()
        .contains("symlinks"));
    std::fs::remove_file(&config.snapshot_dir).unwrap();
    disk.unmount();
    assert!(storage::ready(&config).is_err());
    assert!(!config.snapshot_dir.exists());
    let mut missing = config.clone();
    missing.host_storage.as_mut().unwrap().device = Path::new("/dev/nibrunner-missing-device").to_path_buf();
    assert!(storage::ensure_mounted(&missing).is_err());
}
