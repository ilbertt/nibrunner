use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use serde::Deserialize;

use super::{render, InstallError};
use crate::config::{HostConfig, HostStorage};

pub(crate) const PREPARE_UNIT: &str = "nibrunner-storage-prepare.service";
pub(crate) const READY_UNIT: &str = "nibrunner-storage.service";
pub(crate) const DROP_IN: &str = "host-storage.conf";

const DEVICE_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const DEVICE_LOCK_RETRY: Duration = Duration::from_millis(25);

fn refused(reason: impl Into<String>) -> InstallError {
    InstallError::Refused(format!("host storage is unavailable: {}", reason.into()))
}

fn io_error(error: std::io::Error) -> InstallError {
    refused(error.to_string())
}

fn output(command: &mut Command) -> Result<Vec<u8>, InstallError> {
    let result = command
        .output()
        .map_err(|error| refused(format!("{command:?}: {error}")))?;
    if !result.status.success() {
        return Err(refused(format!(
            "{command:?}: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        )));
    }
    Ok(result.stdout)
}

fn json<T: serde::de::DeserializeOwned>(command: &mut Command) -> Result<T, InstallError> {
    serde_json::from_slice(&output(command)?).map_err(|error| refused(format!("{command:?}: {error}")))
}

#[derive(Deserialize)]
struct BlockDevices {
    blockdevices: Vec<BlockDevice>,
}

#[derive(Deserialize)]
struct BlockDevice {
    #[serde(rename = "type")]
    kind: String,
    ro: bool,
    #[serde(default)]
    children: Vec<BlockDevice>,
}

fn check_layout(devices: &BlockDevices) -> Result<(), InstallError> {
    match devices.blockdevices.as_slice() {
        [device]
            if matches!(device.kind.as_str(), "disk" | "loop")
                && !device.ro
                && device.children.is_empty() =>
        {
            Ok(())
        }
        _ => Err(refused(
            "the device must be a writable whole disk without partitions or holders",
        )),
    }
}

#[derive(Deserialize)]
struct Signatures {
    signatures: Vec<Signature>,
}

#[derive(Deserialize)]
struct Signature {
    #[serde(rename = "type")]
    kind: String,
}

fn has_filesystem(signatures: &Signatures, storage: &HostStorage) -> Result<bool, InstallError> {
    if signatures.signatures.is_empty() {
        return Ok(false);
    }
    if signatures
        .signatures
        .iter()
        .all(|signature| signature.kind == storage.filesystem.as_str())
    {
        return Ok(true);
    }
    Err(refused(format!(
        "{} holds signatures other than {}; nothing was formatted",
        storage.device.display(),
        storage.filesystem.as_str()
    )))
}

#[derive(Deserialize)]
struct Mounts {
    filesystems: Vec<Mount>,
}

#[derive(Deserialize)]
struct Mount {
    #[serde(rename = "maj:min")]
    device: String,
    target: PathBuf,
    fstype: String,
    options: String,
    fsroot: String,
}

fn mounts() -> Result<Mounts, InstallError> {
    json(Command::new("findmnt").args([
        "--kernel",
        "--list",
        "--json",
        "--output",
        "MAJ:MIN,TARGET,FSTYPE,OPTIONS,FSROOT",
    ]))
}

fn mounted(mounts: &Mounts, storage: &HostStorage, device: &str) -> Result<bool, InstallError> {
    let mut found = false;
    for mount in &mounts.filesystems {
        if mount.target == storage.mount_point
            && mount.device == device
            && mount.fstype == storage.filesystem.as_str()
            && mount.fsroot == "/"
            && mount.options.split(',').any(|option| option == "rw")
            && !found
        {
            found = true;
        } else if mount.device == device || mount.target.starts_with(&storage.mount_point) {
            return Err(refused(format!(
                "conflicting mount at {}",
                mount.target.display()
            )));
        }
    }
    Ok(found)
}

fn no_symlinks(path: &Path) -> Result<(), InstallError> {
    for ancestor in path.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(refused(format!(
                    "{} must be a directory without symlinks",
                    ancestor.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(())
}

fn device_number(file: &File) -> Result<String, InstallError> {
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.file_type().is_block_device() {
        return Err(refused("the configured device is not a block device"));
    }
    Ok(format!(
        "{}:{}",
        libc::major(metadata.rdev() as libc::dev_t),
        libc::minor(metadata.rdev() as libc::dev_t)
    ))
}

fn zeroed(reader: &mut impl Read) -> Result<bool, std::io::Error> {
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(true);
        }
        if buffer[..read].iter().any(|byte| *byte != 0) {
            return Ok(false);
        }
    }
}

struct Prepared {
    _device: Flock<File>,
    path: PathBuf,
    number: String,
    mounted: bool,
}

fn lock_device(mut file: File, device: &Path, deadline: Instant) -> Result<Flock<File>, InstallError> {
    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(locked) => return Ok(locked),
            Err((returned, error)) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if error != Errno::EWOULDBLOCK || remaining.is_zero() {
                    return Err(refused(format!(
                        "{} could not be locked: {error}",
                        device.display()
                    )));
                }
                // udev holds shared locks while probing newly formatted or reattached devices.
                file = returned;
                std::thread::sleep(DEVICE_LOCK_RETRY.min(remaining));
            }
        }
    }
}

fn prepare_device(storage: &HostStorage) -> Result<Prepared, InstallError> {
    no_symlinks(&storage.mount_point)?;
    let device = std::fs::canonicalize(&storage.device).map_err(|error| {
        refused(format!(
            "{} could not be resolved: {error}",
            storage.device.display()
        ))
    })?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&device)
        .map_err(io_error)?;
    let number = device_number(&file)?;
    let mut file = lock_device(file, &device, Instant::now() + DEVICE_LOCK_TIMEOUT)?;
    let devices = json(
        Command::new("lsblk")
            .args(["--json", "--paths", "--output", "NAME,TYPE,RO"])
            .arg(&device),
    )?;
    check_layout(&devices)?;
    let is_mounted = mounted(&mounts()?, storage, &number)?;
    if !is_mounted
        && storage.mount_point.exists()
        && std::fs::read_dir(&storage.mount_point)
            .map_err(io_error)?
            .next()
            .transpose()
            .map_err(io_error)?
            .is_some()
    {
        return Err(refused(format!(
            "{} is not mounted and is not empty",
            storage.mount_point.display()
        )));
    }
    let signatures = json(
        Command::new("wipefs")
            .args(["--no-act", "--json", "--output", "TYPE"])
            .arg(&device),
    )?;
    if !has_filesystem(&signatures, storage)? {
        if is_mounted {
            return Err(refused("a mounted device has no filesystem signature"));
        }
        println!(
            "  checking {} is entirely zeroed before initialization",
            device.display()
        );
        file.seek(SeekFrom::Start(0)).map_err(io_error)?;
        if !zeroed(&mut *file).map_err(io_error)? {
            return Err(refused(format!(
                "{} contains data without a supported filesystem; nothing was formatted",
                device.display()
            )));
        }
        println!(
            "  initializing {} as {}",
            device.display(),
            storage.filesystem.as_str()
        );
        output(
            Command::new(format!("mkfs.{}", storage.filesystem.as_str()))
                .arg("-q")
                .arg(&device),
        )?;
    }
    crate::json_store::make_directory(&storage.mount_point, 0o755).map_err(io_error)?;
    Ok(Prepared {
        _device: file,
        path: device,
        number,
        mounted: is_mounted,
    })
}

#[cfg(target_os = "linux")]
pub fn prepare(config: &HostConfig) -> Result<(), InstallError> {
    if let Some(storage) = &config.host_storage {
        prepare_device(storage)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn prepare(config: &HostConfig) -> Result<(), InstallError> {
    if config.host_storage.is_some() {
        return Err(refused("managed storage requires Linux"));
    }
    Ok(())
}

pub fn ensure_mounted(config: &HostConfig) -> Result<(), InstallError> {
    let Some(storage) = &config.host_storage else {
        return Ok(());
    };
    if !cfg!(target_os = "linux") {
        return Err(refused("managed storage requires Linux"));
    }
    let prepared = prepare_device(storage)?;
    if !prepared.mounted {
        output(
            Command::new("mount")
                .args([
                    "--types",
                    storage.filesystem.as_str(),
                    "--options",
                    "rw,nodev,nosuid",
                ])
                .arg(&prepared.path)
                .arg(&storage.mount_point),
        )?;
    }
    if !mounted(&mounts()?, storage, &prepared.number)? {
        return Err(refused(format!(
            "{} did not become mounted",
            storage.mount_point.display()
        )));
    }
    Ok(())
}

pub fn check(config: &HostConfig) -> Result<(), InstallError> {
    let Some(storage) = &config.host_storage else {
        return Ok(());
    };
    no_symlinks(&storage.mount_point)?;
    let device = File::open(&storage.device).map_err(|error| {
        refused(format!(
            "{} could not be opened: {error}",
            storage.device.display()
        ))
    })?;
    if !mounted(&mounts()?, storage, &device_number(&device)?)? {
        return Err(refused(format!(
            "{} is not mounted from {}",
            storage.mount_point.display(),
            storage.device.display()
        )));
    }
    for directory in super::directories(config) {
        if directory.starts_with(&storage.mount_point) {
            no_symlinks(&directory)?;
        }
    }
    Ok(())
}

pub fn ready(config: &HostConfig) -> Result<(), InstallError> {
    check(config)?;
    if config.host_storage.is_some() {
        super::make_directories(config)?;
    }
    Ok(())
}

pub(crate) fn mount_unit_name(storage: &HostStorage) -> String {
    path_unit(&storage.mount_point, "mount")
}

fn path_unit(path: &Path, suffix: &str) -> String {
    let mut name = String::new();
    for byte in path.as_os_str().as_encoded_bytes().iter().skip(1) {
        match byte {
            b'/' => name.push('-'),
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b':' | b'.'
                if *byte != b'.' || !name.is_empty() =>
            {
                name.push(char::from(*byte))
            }
            _ => name.push_str(&format!("\\x{byte:02x}")),
        }
    }
    format!("{name}.{suffix}")
}

fn argument(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    )
}

pub(crate) fn files(config: &HostConfig, config_file: &Path, binary: &Path) -> Vec<(PathBuf, String)> {
    let Some(storage) = &config.host_storage else {
        return Vec::new();
    };
    let header = render::header("The explicitly configured host storage.", config_file);
    let mount = mount_unit_name(storage);
    let device = path_unit(&storage.device, "device");
    let environment = format!(
        "Environment=\"NIBRUNNER_CONFIG={}\"",
        config_file
            .display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    );
    let executable = argument(binary);
    let prepare = format!(
        "{header}\n\
         [Unit]\n\
         Description=Prepare nibrunner host storage\n\
         Requires={device}\n\
         After={device} local-fs.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         {environment}\n\
         ExecStart={executable} storage prepare\n\
         TimeoutStartSec=infinity\n"
    );
    // Local mount units normally precede local-fs.target; preparation runs after boot's mounts
    // so it can detect conflicting mounts before touching the device.
    let mounted = format!(
        "{header}\n\
         [Unit]\n\
         Description=nibrunner host storage\n\
         DefaultDependencies=no\n\
         Requires={PREPARE_UNIT}\n\
         After={PREPARE_UNIT}\n\
         Before=umount.target\n\
         Conflicts=umount.target\n\
         \n\
         [Mount]\n\
         What={}\n\
         Where={}\n\
         Type={}\n\
         Options=rw,nodev,nosuid\n",
        storage.device.display(),
        storage.mount_point.display(),
        storage.filesystem.as_str()
    );
    let ready = format!(
        "{header}\n\
         [Unit]\n\
         Description=Prepare nibrunner storage directories\n\
         BindsTo={mount}\n\
         After={mount}\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         {environment}\n\
         ExecStart={executable} storage ready\n"
    );
    let dependency = format!(
        "{header}\n\
         [Unit]\n\
         Requires={READY_UNIT}\n\
         After={READY_UNIT} {mount}\n\
         BindsTo={mount}\n\
         \n\
         [Service]\n\
         {environment}\n\
         ExecStartPre=+{executable} storage check\n"
    );
    let mut files = vec![
        (super::unit_path(PREPARE_UNIT), prepare),
        (super::unit_path(&mount), mounted),
        (super::unit_path(READY_UNIT), ready),
        (
            super::unit_path(&format!("{}.d/{DROP_IN}", render::DAEMON_UNIT)),
            dependency.clone(),
        ),
    ];
    if config.volumes.zerofs().is_some() {
        files.push((
            super::unit_path(&format!("{}.d/{DROP_IN}", render::ZEROFS_UNIT)),
            dependency,
        ));
    }
    files
}

pub(crate) fn remove_unused_dependencies(
    config: &HostConfig,
    laid: &mut super::Laid,
) -> Result<(), InstallError> {
    for (unit, needed) in [
        (render::DAEMON_UNIT, config.host_storage.is_some()),
        (
            render::ZEROFS_UNIT,
            config.host_storage.is_some() && config.volumes.zerofs().is_some(),
        ),
    ] {
        let path = super::unit_path(&format!("{unit}.d/{DROP_IN}"));
        if !needed {
            remove_generated(&path, laid)?;
        }
    }
    if config.host_storage.is_none() {
        let ready = super::unit_path(READY_UNIT);
        if let Ok(text) = std::fs::read_to_string(&ready) {
            if text.starts_with(render::GENERATED_MARKER) {
                if let Some(mount) = text.lines().find_map(|line| line.strip_prefix("BindsTo=")) {
                    if mount.ends_with(".mount")
                        && Path::new(mount).file_name() == Some(std::ffi::OsStr::new(mount))
                    {
                        remove_generated(&super::unit_path(mount), laid)?;
                    }
                }
                remove_generated(&super::unit_path(PREPARE_UNIT), laid)?;
                remove_generated(&ready, laid)?;
            }
        }
    }
    Ok(())
}

fn remove_generated(path: &Path, laid: &mut super::Laid) -> Result<(), InstallError> {
    if std::fs::read_to_string(path).is_ok_and(|text| text.starts_with(render::GENERATED_MARKER)) {
        std::fs::remove_file(path).map_err(io_error)?;
        laid.did(format!("{} removed", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> HostStorage {
        HostConfig::example().host_storage.unwrap()
    }

    #[test]
    fn a_temporary_shared_probe_lock_delays_preparation_until_it_is_released() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let shared = Flock::lock(file.reopen().unwrap(), FlockArg::LockShared).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(shared);
        });
        let locked = lock_device(
            file.reopen().unwrap(),
            file.path(),
            Instant::now() + Duration::from_secs(2),
        );
        release.join().unwrap();
        assert!(locked.is_ok());
    }

    #[test]
    fn a_device_that_stays_locked_is_refused_when_the_deadline_expires() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let _shared = Flock::lock(file.reopen().unwrap(), FlockArg::LockShared).unwrap();
        let refused = lock_device(
            file.reopen().unwrap(),
            file.path(),
            Instant::now() + Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(refused.to_string().contains("could not be locked"));
    }

    #[test]
    fn only_writable_whole_devices_without_children_are_supported() {
        for (kind, ro, children, accepted) in [
            ("disk", false, "", true),
            ("loop", false, "", true),
            ("disk", true, "", false),
            ("part", false, "", false),
            ("lvm", false, "", false),
            ("raid0", false, "", false),
            (
                "disk",
                false,
                ",\"children\":[{\"type\":\"part\",\"ro\":false}]",
                false,
            ),
        ] {
            let devices = serde_json::from_str(&format!(
                r#"{{"blockdevices":[{{"type":"{kind}","ro":{ro}{children}}}]}}"#
            ))
            .unwrap();
            assert_eq!(check_layout(&devices).is_ok(), accepted, "{kind} {ro} {children}");
        }
    }

    #[test]
    fn existing_signatures_are_reused_only_when_all_match_the_configured_filesystem() {
        for (signatures, expected) in [
            (r#"{"signatures":[]}"#, Some(false)),
            (r#"{"signatures":[{"type":"xfs"}]}"#, Some(true)),
            (r#"{"signatures":[{"type":"ext4"}]}"#, None),
            (r#"{"signatures":[{"type":"xfs"},{"type":"gpt"}]}"#, None),
            (r#"{"signatures":[{"type":"swap"}]}"#, None),
        ] {
            let signatures = serde_json::from_str(signatures).unwrap();
            assert_eq!(has_filesystem(&signatures, &storage()).ok(), expected);
        }
    }

    #[test]
    fn data_anywhere_on_an_unrecognized_device_prevents_initialization() {
        let mut bytes = vec![0; 3 * 1024 * 1024];
        assert!(zeroed(&mut bytes.as_slice()).unwrap());
        *bytes.last_mut().unwrap() = 1;
        assert!(!zeroed(&mut bytes.as_slice()).unwrap());
    }

    fn mount(target: &str, device: &str, fstype: &str, options: &str, fsroot: &str) -> Mount {
        Mount {
            target: PathBuf::from(target),
            device: device.into(),
            fstype: fstype.into(),
            options: options.into(),
            fsroot: fsroot.into(),
        }
    }

    #[test]
    fn the_required_mount_must_be_the_whole_configured_filesystem_and_writable() {
        let storage = storage();
        assert!(!mounted(
            &Mounts {
                filesystems: vec![mount("/", "8:0", "ext4", "rw", "/")]
            },
            &storage,
            "259:1"
        )
        .unwrap());
        for (target, device, fstype, options, fsroot, accepted) in [
            ("/data", "259:1", "xfs", "rw,nodev,nosuid", "/", true),
            ("/elsewhere", "259:1", "xfs", "rw", "/", false),
            ("/data", "8:0", "xfs", "rw", "/", false),
            ("/data", "259:1", "ext4", "rw", "/", false),
            ("/data", "259:1", "xfs", "ro", "/", false),
            ("/data", "259:1", "xfs", "rw", "/subvolume", false),
            ("/data/cache", "8:0", "xfs", "rw", "/", false),
        ] {
            let mounts = Mounts {
                filesystems: vec![mount(target, device, fstype, options, fsroot)],
            };
            assert_eq!(
                mounted(&mounts, &storage, "259:1").is_ok_and(|found| found),
                accepted,
                "{target} {device} {fstype} {options} {fsroot}"
            );
        }
        let mounts = Mounts {
            filesystems: vec![
                mount("/data", "259:1", "xfs", "rw", "/"),
                mount("/data", "259:1", "xfs", "rw", "/"),
            ],
        };
        assert!(mounted(&mounts, &storage, "259:1").is_err());
    }

    #[test]
    fn storage_units_order_initialization_mounting_and_directories_before_consumers() {
        let config = HostConfig::example();
        let files = files(
            &config,
            Path::new("/etc/nibrunner/custom.toml"),
            Path::new("/opt/nibrunnerd"),
        );
        let contents = |unit: &str| {
            &files
                .iter()
                .find(|(path, _)| *path == super::super::unit_path(unit))
                .unwrap()
                .1
        };
        let mount = contents("data.mount");
        assert!(mount.contains("DefaultDependencies=no\n"));
        assert!(mount.contains(&format!("Requires={PREPARE_UNIT}\nAfter={PREPARE_UNIT}\n")));
        assert!(mount.contains("Type=xfs\nOptions=rw,nodev,nosuid\n"));
        let prepared = contents(PREPARE_UNIT);
        assert!(prepared.contains("TimeoutStartSec=infinity"));
        assert!(prepared.contains(" storage prepare\n"));
        assert!(contents(READY_UNIT).contains("BindsTo=data.mount\nAfter=data.mount\n"));
        for unit in [render::DAEMON_UNIT, render::ZEROFS_UNIT] {
            let path = format!("{unit}.d/{DROP_IN}");
            let consumer = contents(&path);
            assert!(consumer.contains(&format!(
                "Requires={READY_UNIT}\nAfter={READY_UNIT} data.mount\nBindsTo=data.mount"
            )));
            assert!(consumer.contains("Environment=\"NIBRUNNER_CONFIG=/etc/nibrunner/custom.toml\""));
            assert!(consumer.contains("ExecStartPre=+\"/opt/nibrunnerd\" storage check"));
        }
        assert_eq!(
            path_unit(Path::new("/data/host-cache"), "mount"),
            "data-host\\x2dcache.mount"
        );
    }

    #[test]
    fn an_unconfigured_host_needs_no_storage_tools_or_units() {
        let config = HostConfig::starter(1);
        check(&config).unwrap();
        ensure_mounted(&config).unwrap();
        assert!(files(
            &config,
            Path::new("/etc/nibrunner/config.toml"),
            Path::new("/usr/local/bin/nibrunnerd")
        )
        .is_empty());
    }

    #[test]
    fn retiring_storage_removes_only_generated_files_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let generated = directory.path().join("data.mount");
        let unmanaged = directory.path().join("custom.mount");
        std::fs::write(&generated, render::GENERATED_MARKER).unwrap();
        std::fs::write(&unmanaged, "[Mount]\n").unwrap();
        let mut laid = super::super::Laid::default();
        remove_generated(&generated, &mut laid).unwrap();
        remove_generated(&generated, &mut laid).unwrap();
        remove_generated(&unmanaged, &mut laid).unwrap();
        assert_eq!(laid.done, [format!("{} removed", generated.display())]);
        assert!(unmanaged.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn generated_storage_units_have_no_systemd_ordering_cycles() {
        let directory = tempfile::tempdir().unwrap();
        let config = HostConfig::example();
        for (path, text) in files(
            &config,
            Path::new("/etc/nibrunner/config.toml"),
            Path::new("/usr/bin/true"),
        ) {
            let path = directory
                .path()
                .join(path.strip_prefix(super::super::SYSTEMD_DIR).unwrap());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        for unit in [render::DAEMON_UNIT, render::ZEROFS_UNIT] {
            std::fs::write(
                directory.path().join(unit),
                "[Service]\nType=oneshot\nExecStart=/usr/bin/true\n",
            )
            .unwrap();
        }
        let output = Command::new("systemd-analyze")
            .arg("verify")
            .args([render::DAEMON_UNIT, render::ZEROFS_UNIT].map(|unit| directory.path().join(unit)))
            .env("SYSTEMD_UNIT_PATH", format!("{}:", directory.path().display()))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
