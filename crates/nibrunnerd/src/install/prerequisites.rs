//! What a host has to be before anything is laid down on it. Every one of these is something the
//! daemon would otherwise discover at the moment it mattered — at startup, or on the first tenant
//! to boot — and say so from too far away to act on.

use std::path::{Path, PathBuf};

use crate::config::{HostConfig, VolumeBackend};

pub struct Check {
    pub what: String,
    pub met: bool,
    /// What to do about it, said as an instruction rather than as a complaint.
    pub remedy: String,
}

impl Check {
    fn new(what: impl Into<String>, met: bool, remedy: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            met,
            remedy: remedy.into(),
        }
    }
}

/// Every check, in one pass. All of them run even after one fails: a host missing three things
/// should be told three times, not three times over.
pub fn check(config: &HostConfig) -> Vec<Check> {
    let mut checks = vec![
        Check::new(
            "/dev/kvm",
            Path::new("/dev/kvm").exists(),
            "a machine with hardware virtualisation; a microVM cannot be booted without it",
        ),
        Check::new(
            "ip_forward",
            reads_one("/proc/sys/net/ipv4/ip_forward"),
            "echo 'net.ipv4.ip_forward=1' > /etc/sysctl.d/99-nibrunner.conf && sysctl -w net.ipv4.ip_forward=1",
        ),
        Check::new(
            "nf_conntrack",
            Path::new("/proc/sys/net/netfilter/nf_conntrack_max").exists(),
            "echo nf_conntrack > /etc/modules-load.d/nibrunner.conf && modprobe nf_conntrack",
        ),
    ];
    for tool in ["nft", "mke2fs"] {
        checks.push(binary(tool, "install the nftables and e2fsprogs packages"));
    }

    match &config.volumes {
        VolumeBackend::LocalFile => {}
        VolumeBackend::Zerofs(_) => {
            for (tool, remedy) in [
                (
                    "nbd-client",
                    "install the nbd-client, e2fsprogs and fuse3 packages",
                ),
                ("debugfs", "install the nbd-client, e2fsprogs and fuse3 packages"),
                (
                    "fusermount3",
                    "install the nbd-client, e2fsprogs and fuse3 packages",
                ),
                // ZeroFS does not run as root, and this is what creates the account it does run as.
                (
                    "useradd",
                    "install the passwd package, or create the zerofs account by hand",
                ),
            ] {
                checks.push(binary(tool, remedy));
            }
            // Slot N takes /dev/nbdN and the export reader holds the last of them, so a host whose
            // module allocated fewer minors than that cannot read an export however it is asked.
            let reader = nft_render::export_reader_device_path();
            checks.push(Check::new(
                reader.clone(),
                Path::new(&reader).exists(),
                "echo 'options nbd nbds_max=1024' > /etc/modprobe.d/nbd.conf && modprobe -r nbd; modprobe nbd nbds_max=1024",
            ));
        }
    }

    checks.push(match guest_image(config) {
        Ok(version) => Check::new(format!("guest image {version}"), true, String::new()),
        Err(reason) => Check::new(
            "guest image",
            false,
            format!(
                "{reason}; lay vmlinux, rootfs.ext4 and manifest.json down in {}",
                config.guest_image_dir.display()
            ),
        ),
    });
    checks
}

pub fn guest_image(config: &HostConfig) -> Result<String, String> {
    crate::adapters::vm::manager::verify_guest_image(&config.guest_image_dir).map_err(|error| error.message())
}

fn binary(name: &str, remedy: &str) -> Check {
    Check::new(name, on_path(name).is_some(), remedy)
}

fn reads_one(path: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| text.trim() == "1")
}

pub fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|info| info.is_file() && info.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_setting_is_only_met_when_it_reads_exactly_one() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("flag");
        std::fs::write(&path, "1\n").unwrap();
        assert!(reads_one(&path.display().to_string()));
        std::fs::write(&path, "0\n").unwrap();
        assert!(!reads_one(&path.display().to_string()));
        assert!(!reads_one(&directory.path().join("absent").display().to_string()));
    }

    #[test]
    fn a_tool_is_found_by_walking_the_path_it_would_actually_be_run_from() {
        let directory = tempfile::tempdir().unwrap();
        let tool = directory.path().join("nibrunner-test-tool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        temp_env_path(directory.path(), || {
            assert_eq!(on_path("nibrunner-test-tool"), Some(tool.clone()));
            assert_eq!(on_path("nibrunner-test-absent"), None);
        });
    }

    // A file that is on PATH but not executable is not a tool this host can run, and reporting it
    // as present would send an operator looking at the wrong thing.
    #[test]
    fn something_on_the_path_that_cannot_be_run_is_not_a_tool() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("nibrunner-test-data"), "not a program").unwrap();
        temp_env_path(directory.path(), || {
            assert_eq!(on_path("nibrunner-test-data"), None);
        });
    }

    fn temp_env_path(directory: &Path, body: impl FnOnce()) {
        let restore = std::env::var_os("PATH");
        std::env::set_var("PATH", directory);
        body();
        match restore {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
    }

    #[test]
    fn a_local_file_host_is_not_asked_for_the_tools_only_zerofs_needs() {
        let config = HostConfig::under(Path::new("/srv/nibrunner"));
        let named: Vec<String> = check(&config).into_iter().map(|check| check.what).collect();
        for zerofs_only in ["nbd-client", "fusermount3"] {
            assert!(!named.iter().any(|what| what == zerofs_only), "{named:?}");
        }
        assert!(named.iter().any(|what| what == "nft"), "{named:?}");
    }
}
