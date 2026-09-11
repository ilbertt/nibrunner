#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

//! Alone in its own binary: the umask is process-wide, and set inside the unit-test process it
//! would change what every other test's files come out as.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// The host that taught this had `install` run from a shell left at umask 077, and the zerofs
// account could not cross /opt/nibrunner to reach its own binary.
#[test]
fn what_is_made_on_the_way_to_a_directory_can_be_crossed_whatever_the_umask_was() {
    #[allow(unsafe_code, reason = "setting this process's umask has no safe spelling")]
    unsafe {
        libc::umask(0o077);
    }
    let root = tempfile::tempdir().unwrap();

    let binaries = root.path().join("opt/nibrunner/bin");
    nibrunnerd::json_store::make_directory(&binaries, 0o755).unwrap();
    for made in ["opt", "opt/nibrunner", "opt/nibrunner/bin"] {
        assert_eq!(mode(&root.path().join(made)), 0o755, "{made}");
    }

    // The mode asked for is the leaf's alone: what leads to a private directory is still public.
    let state = root.path().join("var/lib/nibrunner");
    nibrunnerd::json_store::make_directory(&state, 0o700).unwrap();
    assert_eq!(mode(&root.path().join("var")), 0o755);
    assert_eq!(mode(&root.path().join("var/lib")), 0o755);
    assert_eq!(mode(&state), 0o700);

    // A directory that was already there is not something this made, so its mode is not this
    // function's to change.
    let existing = root.path().join("data");
    std::fs::create_dir(&existing).unwrap();
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o710)).unwrap();
    nibrunnerd::json_store::make_directory(&existing.join("zerofs"), 0o700).unwrap();
    assert_eq!(mode(&existing), 0o710);
    assert_eq!(mode(&existing.join("zerofs")), 0o700);
}
