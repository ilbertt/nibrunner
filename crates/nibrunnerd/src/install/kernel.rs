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

/// What the file asks for. More than this host can use, deliberately: it costs nothing, and it is
/// what every measured run of this has been done on.
const NBDS_MAX_REQUESTED: u32 = 1024;

/// Slot N takes /dev/nbdN and the export reader holds the last of them, so the minors this host
/// has to be able to address is one more than the highest slot it will ever allocate.
pub fn required_minors() -> u32 {
    nft_render::NBD_SLOT_LIMIT + 1
}

/// Written where the next boot reads them. Returned rather than written here so they go through
/// the same marker and the same refusal as every other file this command lays down.
pub fn files(config: &HostConfig, config_file: &Path) -> Vec<(PathBuf, String)> {
    let header = |purpose: &str| super::render::header(purpose, config_file);
    let mut modules = format!(
        "{}\n# Loaded at boot. nf_conntrack is what the guest isolation ruleset counts on.\nnf_conntrack\n",
        header("Kernel modules this host serves nothing without.")
    );
    let mut files = vec![
        (
            PathBuf::from(SYSCTL_FILE),
            format!(
                "{}\n# A packet from a guest reaches the world through this host, which is forwarding.\nnet.ipv4.ip_forward=1\n",
                header("Kernel settings this host serves nothing without.")
            ),
        ),
    ];
    if config.volumes.zerofs().is_some() {
        modules.push_str("nbd\n");
        files.push((
            PathBuf::from(MODPROBE_FILE),
            format!(
                "{}\n# {} minors are needed: one per slot, plus the one the export reader holds.\n# This is read when the module loads and cannot be changed on a loaded one.\noptions nbd nbds_max={NBDS_MAX_REQUESTED}\n",
                header("How many block devices ZeroFS volumes may be attached on."),
                required_minors()
            ),
        ));
    }
    files.push((PathBuf::from(MODULES_FILE), modules));
    files
}

/// Made true on the kernel that is running, now that the files above have made it true of the next
/// one. Everything here is idempotent.
pub fn apply(config: &HostConfig, laid: &mut super::Laid) -> Result<(), InstallError> {
    if std::fs::read_to_string(IP_FORWARD).is_ok_and(|text| text.trim() == "1") {
        laid.note("ip_forward already on");
    } else {
        std::fs::write(IP_FORWARD, "1\n")
            .map_err(|error| InstallError::Refused(format!("{IP_FORWARD} could not be set: {error}")))?;
        laid.note("ip_forward on");
    }

    if Path::new("/proc/sys/net/netfilter/nf_conntrack_max").exists() {
        laid.note("nf_conntrack already loaded");
    } else {
        modprobe(&["nf_conntrack"])?;
        laid.note("nf_conntrack loaded");
    }

    if config.volumes.zerofs().is_none() {
        return Ok(());
    }
    match verdict(loaded_minors(), required_minors()) {
        Verdict::Enough => laid.note("nbd already has the minors this host needs"),
        Verdict::Load => {
            modprobe(&["nbd", &format!("nbds_max={NBDS_MAX_REQUESTED}")])?;
            laid.note(format!("nbd loaded with {NBDS_MAX_REQUESTED} minors"));
        }
        // The only ways out are a reboot, which the file just written makes correct, or unloading
        // the module — which errors every request queued on every attached volume. Neither is a
        // thing to do to a host on the strength of a re-run.
        Verdict::Refuse { loaded } => {
            return Err(InstallError::Refused(format!(
                "nbd is loaded with {loaded} minors and this host needs {}. nbds_max is read when the module loads, so it cannot be raised in place: reboot, which {MODPROBE_FILE} now makes enough, or unload the module by hand if nothing is attached to it.",
                required_minors()
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
        assert_eq!(required_minors(), nft_render::NBD_SLOT_LIMIT + 1);
        assert!(NBDS_MAX_REQUESTED >= required_minors());
        let reader = nft_render::export_reader_device_path();
        let highest: u32 = reader.trim_start_matches("/dev/nbd").parse().unwrap();
        assert!(
            highest < required_minors(),
            "{reader} is outside what is asked for"
        );
    }

    #[test]
    fn every_setting_is_written_where_the_next_boot_reads_it() {
        let files = files(&zerofs_host(), Path::new("/etc/nibrunner/config.toml"));
        assert!(named(&files, SYSCTL_FILE)
            .unwrap()
            .contains("net.ipv4.ip_forward=1"));
        assert!(named(&files, MODULES_FILE).unwrap().contains("nf_conntrack"));
        assert!(named(&files, MODPROBE_FILE)
            .unwrap()
            .contains(&format!("options nbd nbds_max={NBDS_MAX_REQUESTED}")));
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
