//! The kernel settings a host needs, made to stick and made true now.
//!
//! These are not packages. `/etc/sysctl.d`, `/etc/modules-load.d` and `/etc/modprobe.d` are the
//! same kind of thing as `/etc/systemd/system`, which this command already writes into — so
//! refusing to set them, the way installing packages is refused, was a distinction that did not
//! survive being looked at. Each one is written where the next boot reads it *and* applied to the
//! running kernel, because a host laid out today should not need a reboot to serve today.

use std::path::{Path, PathBuf};

use super::InstallError;
use crate::config::HostConfig;

pub const SYSCTL_FILE: &str = "/etc/sysctl.d/99-nibrunner.conf";
pub const MODULES_FILE: &str = "/etc/modules-load.d/nibrunner.conf";
pub const MODPROBE_FILE: &str = "/etc/modprobe.d/nibrunner-nbd.conf";

const IP_FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";
const NBDS_MAX: &str = "/sys/module/nbd/parameters/nbds_max";
const CONNTRACK_MAX: &str = "/proc/sys/net/netfilter/nf_conntrack_max";
const CONNTRACK_BUCKETS: &str = "/proc/sys/net/netfilter/nf_conntrack_buckets";

/// The connection-tracking table's allowance for one app. Every flow between the proxy and a
/// guest is an entry, DNAT'd from the guest's host port, and so is every flow the guest opens
/// out, NAT'd on its way; a closed TCP flow stays one for the 120 s of TIME_WAIT, so an app that
/// opens and closes eight connections a second holds a thousand on its own. An entry is some
/// 300 bytes, taken as flows appear: 300 KiB an app when it is full, 300 MB on a thousand-app
/// host that fills its table.
pub const CONNTRACK_ENTRIES_PER_APP: u32 = 1024;

/// What the kernel sizes its own table to on a 64-bit box with more than 4 GiB, which every
/// host that runs microVMs is. Never below it: a host laid out for a few apps is left where the
/// kernel put it.
pub const CONNTRACK_ENTRIES_FLOOR: u32 = 262_144;

/// Slot N takes /dev/nbdN and the export reader holds the one past the last slot, so the minors
/// this host has to be able to address is one more than the apps it is laid out for.
pub fn required_minors(config: &HostConfig) -> u32 {
    config.max_apps + 1
}

/// How many flows the kernel tracks before it drops packets — for every app on the host, since
/// the table is one. Also how many buckets it hashes them into, one per entry, the ratio the
/// kernel sizes its own by: raising the max alone leaves the table it hashes into as it was, and
/// a bucket is 8 bytes, 8 MB on a million entries.
pub fn conntrack_entries(config: &HostConfig) -> u32 {
    (config.max_apps * CONNTRACK_ENTRIES_PER_APP).max(CONNTRACK_ENTRIES_FLOOR)
}

/// Written where the next boot reads them. Returned rather than written here so they go through
/// the same marker and the same refusal as every other file this command lays down.
pub fn files(config: &HostConfig, config_file: &Path) -> Vec<(PathBuf, String)> {
    let header = |purpose: &str| super::render::header(purpose, config_file);
    let mut modules = format!(
        "{}\n# Loaded at boot, before the sysctls are applied. nf_conntrack is what the guest isolation\n# ruleset counts on, and what the net.netfilter keys are keys of.\nnf_conntrack\n",
        header("Kernel modules this host serves nothing without.")
    );
    let entries = conntrack_entries(config);
    let mut files = vec![
        (
            PathBuf::from(SYSCTL_FILE),
            format!(
                "{}\n# A packet from a guest reaches the world through this host, which is forwarding.\nnet.ipv4.ip_forward=1\n\
                 # Every flow between the proxy and a guest, and every flow a guest opens, is an entry here for\n\
                 # its lifetime and 120 s after it closes: {CONNTRACK_ENTRIES_PER_APP} for each app max_apps lays the host out for, and\n\
                 # never under the kernel's own {CONNTRACK_ENTRIES_FLOOR}. Full, the kernel drops packets for every app on the host.\n\
                 net.netfilter.nf_conntrack_max={entries}\n\
                 # One bucket per entry, the ratio the kernel sizes its own table by; raising the max alone would\n\
                 # leave the table it hashes into as it was.\n\
                 net.netfilter.nf_conntrack_buckets={entries}\n",
                header("Kernel settings this host serves nothing without.")
            ),
        ),
    ];
    if config.volumes.zerofs().is_some() {
        modules.push_str("nbd\n");
        files.push((
            PathBuf::from(MODPROBE_FILE),
            format!(
                "{}\n# One minor per app max_apps lays the host out for, plus the one the export reader holds.\n# This is read when the module loads and cannot be changed on a loaded one.\noptions nbd nbds_max={}\n",
                header("How many block devices ZeroFS volumes may be attached on."),
                required_minors(config)
            ),
        ));
    }
    files.push((PathBuf::from(MODULES_FILE), modules));
    files
}

/// Made true on the kernel that is running, now that the files above have made it true of the next
/// one. Everything here is idempotent.
pub fn apply(config: &HostConfig, laid: &mut super::Laid) -> Result<(), InstallError> {
    if set(IP_FORWARD, "1")? {
        laid.did("ip_forward on");
    }

    // The module first: its sysctls are not there to set until it is.
    if !Path::new(CONNTRACK_MAX).exists() {
        modprobe(&["nf_conntrack"])?;
        laid.did("nf_conntrack loaded");
    }
    let entries = conntrack_entries(config).to_string();
    if set(CONNTRACK_MAX, &entries)? | set(CONNTRACK_BUCKETS, &entries)? {
        laid.did(format!("conntrack table sized for {entries} entries"));
    }

    if config.volumes.zerofs().is_none() {
        return Ok(());
    }
    let required = required_minors(config);
    match verdict(loaded_minors(), required) {
        Verdict::Enough => {}
        Verdict::Load => {
            modprobe(&["nbd", &format!("nbds_max={required}")])?;
            laid.did(format!("nbd loaded with {required} minors"));
        }
        // The only ways out are a reboot, which the file just written makes correct, or unloading
        // the module — which errors every request queued on every attached volume. Neither is a
        // thing to do to a host on the strength of a re-run.
        Verdict::Refuse { loaded } => {
            return Err(InstallError::Refused(format!(
                "nbd is loaded with {loaded} minors and this host needs {required} for max_apps. nbds_max is read when the module loads, so it cannot be raised in place: reboot, which {MODPROBE_FILE} now makes enough, or unload the module by hand if nothing is attached to it."
            )))
        }
    }
    Ok(())
}

enum Verdict {
    Enough,
    Load,
    Refuse { loaded: u32 },
}

fn verdict(loaded: Option<u32>, required: u32) -> Verdict {
    match loaded {
        None => Verdict::Load,
        Some(minors) if minors >= required => Verdict::Enough,
        Some(minors) => Verdict::Refuse { loaded: minors },
    }
}

fn loaded_minors() -> Option<u32> {
    std::fs::read_to_string(NBDS_MAX).ok()?.trim().parse().ok()
}

/// Whether the kernel setting had to be written to hold `value`.
fn set(path: &str, value: &str) -> Result<bool, InstallError> {
    if std::fs::read_to_string(path).is_ok_and(|text| text.trim() == value) {
        return Ok(false);
    }
    std::fs::write(path, format!("{value}\n"))
        .map_err(|error| InstallError::Refused(format!("{path} could not be set: {error}")))?;
    Ok(true)
}

fn modprobe(arguments: &[&str]) -> Result<(), InstallError> {
    let loaded = std::process::Command::new("modprobe")
        .args(arguments)
        .output()
        .map_err(|error| InstallError::Refused(format!("modprobe could not be run: {error}")))?;
    if loaded.status.success() {
        return Ok(());
    }
    Err(InstallError::Refused(format!(
        "{} could not be loaded: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&loaded.stderr).trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VolumeBackend;

    fn zerofs_host() -> HostConfig {
        let mut config = HostConfig::under(Path::new("/srv/nibrunner"));
        config.volumes = VolumeBackend::Zerofs(Box::new(crate::test_support::zerofs_settings(|_| {})));
        config
    }

    fn named(files: &[(PathBuf, String)], path: &str) -> Option<String> {
        files
            .iter()
            .find(|(written, _)| written == Path::new(path))
            .map(|(_, body)| body.clone())
    }

    #[test]
    fn a_module_that_is_not_loaded_is_loaded_and_one_that_is_big_enough_is_left_alone() {
        assert!(matches!(verdict(None, 64), Verdict::Load));
        assert!(matches!(verdict(Some(64), 64), Verdict::Enough));
        assert!(matches!(verdict(Some(1024), 64), Verdict::Enough));
    }

    // Raising it means unloading the module, which errors every request queued on every attached
    // volume. That is a person's decision, so this reports rather than takes it.
    #[test]
    fn a_module_already_loaded_too_small_is_refused_rather_than_reloaded() {
        assert!(matches!(verdict(Some(16), 64), Verdict::Refuse { loaded: 16 }));
    }

    #[test]
    fn what_the_file_asks_for_covers_every_slot_and_the_export_reader() {
        for max_apps in [1, 63, 1000] {
            let mut config = zerofs_host();
            config.max_apps = max_apps;
            assert_eq!(required_minors(&config), max_apps + 1);
            let reader = nft_render::export_reader_device_path(max_apps);
            let highest: u32 = reader.trim_start_matches("/dev/nbd").parse().unwrap();
            assert!(
                highest < required_minors(&config),
                "{reader} is outside what is asked for"
            );
            let last_slot = nft_render::describe_slot(max_apps - 1, crate::test_support::app_id());
            assert_ne!(last_slot.nbd_device_path, reader);
            assert!(named(
                &files(&config, Path::new("/etc/nibrunner/config.toml")),
                MODPROBE_FILE
            )
            .unwrap()
            .contains(&format!("options nbd nbds_max={}\n", max_apps + 1)));
        }
    }

    // The table is one for the host, and a full one drops packets for every app on it. A host laid
    // out for a few apps is left at the kernel's own size rather than given a smaller one.
    #[test]
    fn the_conntrack_table_grows_with_max_apps_and_never_shrinks_under_the_kernels_own() {
        let mut config = zerofs_host();
        config.max_apps = 40;
        assert_eq!(conntrack_entries(&config), CONNTRACK_ENTRIES_FLOOR);
        config.max_apps = 256;
        assert_eq!(conntrack_entries(&config), CONNTRACK_ENTRIES_FLOOR);
        config.max_apps = 1000;
        assert_eq!(conntrack_entries(&config), 1_024_000);
        config.max_apps = nft_render::most_apps_the_ports_fit();
        assert_eq!(conntrack_entries(&config), 5_700_608);

        let sysctl = named(
            &files(&config, Path::new("/etc/nibrunner/config.toml")),
            SYSCTL_FILE,
        )
        .unwrap();
        assert!(
            sysctl.contains("net.netfilter.nf_conntrack_max=5700608\n"),
            "{sysctl}"
        );
        assert!(
            sysctl.contains("net.netfilter.nf_conntrack_buckets=5700608\n"),
            "raising the max alone leaves the hash table as it was: {sysctl}"
        );
    }

    #[test]
    fn every_setting_is_written_where_the_next_boot_reads_it() {
        let files = files(&zerofs_host(), Path::new("/etc/nibrunner/config.toml"));
        let sysctl = named(&files, SYSCTL_FILE).unwrap();
        assert!(sysctl.contains("net.ipv4.ip_forward=1"));
        assert!(sysctl.contains("net.netfilter.nf_conntrack_max=1024000"));
        assert!(named(&files, MODULES_FILE).unwrap().contains("nf_conntrack"));
        assert!(named(&files, MODPROBE_FILE)
            .unwrap()
            .contains("options nbd nbds_max=1001"));
    }

    #[test]
    fn a_setting_already_right_is_left_alone_and_one_that_is_not_is_written() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nf_conntrack_max");
        let path = path.to_str().unwrap();
        assert!(set(path, "262144").unwrap(), "not there yet");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "262144\n");
        assert!(!set(path, "262144").unwrap());
        assert!(set(path, "1024000").unwrap());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "1024000\n");
    }

    #[test]
    fn a_local_file_host_is_given_no_nbd_it_will_never_attach() {
        let files = files(
            &HostConfig::under(Path::new("/srv/nibrunner")),
            Path::new("/etc/nibrunner/config.toml"),
        );
        assert!(named(&files, MODPROBE_FILE).is_none(), "{files:?}");
        assert!(!named(&files, MODULES_FILE).unwrap().contains("nbd"));
        assert!(named(&files, MODULES_FILE).unwrap().contains("nf_conntrack"));
    }

    #[test]
    fn everything_written_here_carries_the_marker_that_makes_it_replaceable() {
        for (_, body) in files(&zerofs_host(), Path::new("/etc/nibrunner/config.toml")) {
            assert!(body.starts_with(super::super::render::GENERATED_MARKER), "{body}");
        }
    }
}
