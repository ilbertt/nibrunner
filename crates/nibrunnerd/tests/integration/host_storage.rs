#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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

    fn detach(&self) {
        run(Command::new("losetup").arg("--detach").arg(&self.device));
        let backing_file = Path::new("/sys/class/block")
            .join(self.device.file_name().unwrap())
            .join("loop/backing_file");
        let deadline = Instant::now() + Duration::from_secs(10);
        // Loop detachment is lazy; an empty udev queue does not prove the last opener has closed.
        loop {
            match std::fs::read_to_string(&backing_file) {
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        || error.raw_os_error() == Some(libc::ENODEV) =>
                {
                    return;
                }
                Err(error) => panic!("{}: {error}", backing_file.display()),
                Ok(backing) => assert!(
                    Instant::now() < deadline,
                    "{} is still attached to {backing}",
                    self.device.display()
                ),
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.mount_point).output();
        let _ = Command::new("losetup").arg("--detach").arg(&self.device).output();
    }
}

#[test]
fn a_loop_device_can_be_replaced_after_a_probe_delays_its_detachment() {
    if !super::enabled() {
        return;
    }
    let disk = Disk::blank();
    let held = File::open(&disk.device).unwrap();
    let probe = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        drop(held);
    });
    disk.detach();
    let replacement = disk.directory.path().join("replacement.img");
    let mut file = File::create(&replacement).unwrap();
    file.set_len(512 * 1024 * 1024).unwrap();
    file.write_all(b"replacement").unwrap();
    file.sync_all().unwrap();
    run(Command::new("losetup").arg(&disk.device).arg(&replacement));
    probe.join().unwrap();
    let mut bytes = [0; 11];
    File::open(&disk.device).unwrap().read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"replacement");
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
        if filesystem == StorageFilesystem::Ext4 {
            let superblock = run(Command::new("tune2fs").arg("-l").arg(&disk.device));
            let features = superblock
                .lines()
                .find_map(|line| line.strip_prefix("Filesystem features:"))
                .unwrap();
            assert!(features.split_whitespace().any(|feature| feature == "verity"));
        }
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
        disk.detach();
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
fn an_existing_ext4_filesystem_keeps_its_features_and_contents() {
    if !super::enabled() {
        return;
    }
    let disk = Disk::blank();
    run(Command::new("mkfs.ext4")
        .args(["-q", "-O", "^verity"])
        .arg(&disk.device));
    let features_before = run(Command::new("tune2fs").arg("-l").arg(&disk.device));
    let config = disk.config(StorageFilesystem::Ext4);
    storage::ensure_mounted(&config).unwrap();
    std::fs::write(disk.mount_point.join("preserved"), "existing data").unwrap();
    disk.unmount();
    storage::ensure_mounted(&config).unwrap();
    assert_eq!(
        std::fs::read_to_string(disk.mount_point.join("preserved")).unwrap(),
        "existing data"
    );
    disk.unmount();
    let features_after = run(Command::new("tune2fs").arg("-l").arg(&disk.device));
    let features = |superblock: &str| {
        superblock
            .lines()
            .find(|line| line.starts_with("Filesystem features:"))
            .unwrap()
            .to_owned()
    };
    assert_eq!(features(&features_before), features(&features_after));
}

#[test]
fn host_storage_checks_the_host_mount_namespace_inside_a_service_sandbox() {
    if !super::enabled() {
        return;
    }
    let disk = Disk::blank();
    let config = disk.config(StorageFilesystem::Ext4);
    storage::ensure_mounted(&config).unwrap();
    storage::ready(&config).unwrap();
    let cache = disk.mount_point.join("cache");
    std::fs::create_dir(&cache).unwrap();
    let config_file = disk.directory.path().join("config.toml");
    std::fs::write(&config_file, config.to_toml()).unwrap();
    run(Command::new("unshare")
        .args([
            "--mount",
            "--propagation",
            "private",
            "/bin/sh",
            "-ec",
            "mount --bind \"$1\" \"$1\"; exec \"$2\" storage check",
            "storage-check",
        ])
        .arg(&cache)
        .arg(env!("CARGO_BIN_EXE_nibrunnerd"))
        .env("NIBRUNNER_CONFIG", &config_file));
    storage::check(&config).unwrap();
}

#[test]
fn verity_seals_files_without_preventing_jailer_links_or_private_writable_mappings() {
    use nibrunnerd::adapters::fs_verity;

    if !super::enabled() {
        return;
    }
    let disk = Disk::blank();
    storage::ensure_mounted(&disk.config(StorageFilesystem::Ext4)).unwrap();
    let path = disk.mount_point.join("memory");
    std::fs::write(&path, [42; 8192]).unwrap();
    let file = File::open(&path).unwrap();
    let digest = fs_verity::seal(&file).unwrap().expect("ext4 supports verity");
    file.sync_all().unwrap();
    assert_eq!(fs_verity::measure(&file).unwrap(), digest);
    assert_eq!(fs_verity::seal(&file).unwrap(), Some(digest));
    assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
    let linked = disk.mount_point.join("jailed-memory");
    std::os::unix::fs::chown(&path, Some(0), Some(1234)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o440)).unwrap();
    std::fs::hard_link(&path, &linked).unwrap();
    std::fs::remove_file(&path).unwrap();
    let file = File::open(&linked).unwrap();
    std::fs::remove_file(&linked).unwrap();
    assert_eq!(fs_verity::measure(&file).unwrap(), digest);
    private_mapping_keeps_verity_pages_readable_and_writes_private(&file);
    let mut original = [0];
    (&file).read_exact(&mut original).unwrap();
    assert_eq!(original, [42]);
}

#[allow(
    unsafe_code,
    reason = "the Firecracker file backend uses a private writable mmap"
)]
fn private_mapping_keeps_verity_pages_readable_and_writes_private(file: &File) {
    use std::os::fd::AsRawFd;

    unsafe {
        let mapping = libc::mmap(
            std::ptr::null_mut(),
            8192,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        );
        assert_ne!(mapping, libc::MAP_FAILED, "{}", std::io::Error::last_os_error());
        let byte = mapping.cast::<u8>();
        assert_eq!(byte.read_volatile(), 42);
        byte.write_volatile(7);
        assert_eq!(byte.read_volatile(), 7);
        assert_eq!(libc::munmap(mapping, 8192), 0);
    }
}

#[test]
fn unsupported_filesystems_leave_files_available_for_full_file_verification() {
    use nibrunnerd::adapters::fs_verity;

    if !super::enabled() {
        return;
    }
    let disk = Disk::blank();
    storage::ensure_mounted(&disk.config(StorageFilesystem::Xfs)).unwrap();
    let path = disk.mount_point.join("memory");
    std::fs::write(&path, b"memory").unwrap();
    let file = File::open(&path).unwrap();
    assert_eq!(fs_verity::seal(&file).unwrap(), None);
    assert!(fs_verity::measure(&file).is_err());
    std::fs::write(&path, b"replacement").unwrap();
}

#[test]
fn host_storage_initializes_nonzero_devices_without_signatures_and_reuses_their_filesystems() {
    if !super::enabled() {
        return;
    }
    for filesystem in [StorageFilesystem::Ext4, StorageFilesystem::Xfs] {
        let disk = Disk::blank();
        let mut file = File::options().write(true).open(&disk.device).unwrap();
        let data: Vec<u8> = (1..=255).cycle().take(65536).collect();
        for offset in [0, 1024 * 1024, 400 * 1024 * 1024] {
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(&data).unwrap();
        }
        file.sync_all().unwrap();
        drop(file);
        let signatures = run(Command::new("wipefs")
            .args(["--no-act", "--noheadings", "--output", "TYPE"])
            .arg(&disk.device));
        assert!(signatures.is_empty(), "{signatures}");
        let config = disk.config(filesystem);
        storage::ensure_mounted(&config).unwrap();
        std::fs::write(disk.mount_point.join("preserved"), "existing data").unwrap();
        let uuid = run(Command::new("blkid")
            .args(["--probe", "--output", "value", "--match-tag", "UUID"])
            .arg(&disk.device));
        disk.unmount();
        storage::ensure_mounted(&config).unwrap();
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
    }
}

#[test]
fn host_storage_refuses_partition_tables_and_mismatched_filesystems_without_writing() {
    if !super::enabled() {
        return;
    }
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
