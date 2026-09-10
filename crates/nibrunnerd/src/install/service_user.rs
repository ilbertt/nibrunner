//! The account ZeroFS runs under. It holds this host's object-store credentials and the key every
//! tenant's blocks are encrypted with, so it is not root — and since nothing else on the host
//! creates it, creating it is one of the things laying a host out means.

use std::path::Path;

use super::InstallError;

/// nibrun's name for it, kept so a host laid out here and one laid out there read the same.
pub const ZEROFS_USER: &str = "zerofs";

pub enum Made {
    AlreadyThere,
    Created,
}

/// Idempotent: a host being brought up to a new release has this account already, and `useradd`
/// on an account that exists is an error rather than a no-op.
pub fn ensure(name: &str) -> Result<Made, InstallError> {
    if ids(name).is_some() {
        return Ok(Made::AlreadyThere);
    }
    let made = std::process::Command::new("useradd")
        .args([
            "--system",
            "--no-create-home",
            "--shell",
            "/usr/sbin/nologin",
            name,
        ])
        .output()
        .map_err(|error| InstallError::Refused(format!("useradd could not be run: {error}")))?;
    if !made.status.success() {
        return Err(InstallError::Refused(format!(
            "the {name} account could not be created: {}",
            String::from_utf8_lossy(&made.stderr).trim()
        )));
    }
    ids(name).map(|_| Made::Created).ok_or_else(|| {
        InstallError::Refused(format!(
            "the {name} account was created and then could not be read"
        ))
    })
}

/// Asked of the system rather than read out of `/etc/passwd`, because an account can come from
/// somewhere else entirely and a host that resolves one is a host that can run as it.
pub fn ids(name: &str) -> Option<(u32, u32)> {
    let ask = |flag: &str| {
        let output = std::process::Command::new("id")
            .arg(flag)
            .arg(name)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    parse_ids(&ask("-u")?, &ask("-g")?)
}

fn parse_ids(uid: &str, gid: &str) -> Option<(u32, u32)> {
    Some((uid.trim().parse().ok()?, gid.trim().parse().ok()?))
}

/// The cache is the one thing the live server writes to that systemd does not create for it, so it
/// is the one thing whose ownership has to be handed over here.
pub fn own(path: &Path, (uid, gid): (u32, u32)) -> Result<(), InstallError> {
    std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(|error| {
        InstallError::Refused(format!(
            "{} could not be given to its owner: {error}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_account_is_read_as_the_two_numbers_it_is() {
        assert_eq!(parse_ids("999", "998"), Some((999, 998)));
        assert_eq!(parse_ids(" 999 \n", "998\n"), Some((999, 998)));
    }

    #[test]
    fn anything_that_is_not_a_pair_of_numbers_is_no_account_at_all() {
        assert_eq!(parse_ids("", ""), None);
        assert_eq!(parse_ids("zerofs", "zerofs"), None);
        assert_eq!(parse_ids("999", "id: no such user"), None);
    }

    // Every host has root, so this is the one lookup that can be asserted anywhere.
    #[test]
    fn an_account_this_machine_has_resolves() {
        assert_eq!(ids("root"), Some((0, 0)));
        assert_eq!(ids("nibrunner-no-such-account"), None);
    }
}
