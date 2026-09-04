//! nibrunner's guest PID 1.
//!
//! Ported from `apps/runtime/src/`, which is C. Rust because the wire this speaks is the wire the
//! host speaks, and `nibrunner-guest-contract` now holds both halves of it: the frame codec, the
//! `instance.env` format and the boot paths are one definition each rather than a header, an
//! encoder and a decoder that agree by review.
//!
//! Boots one tenant binary inside one Firecracker microVM: mounts what the guest needs, reads the
//! instance config off its own drive, gives the tenant its data filesystem at `data/`, drops
//! privileges, and supervises it until it is asked to stop or has run out of restarts. Nothing
//! else runs in this VM.

// Unsafe is this crate's medium rather than an exception in it. A guest PID 1 mounts filesystems,
// forks, execs, drops privileges, freezes a filesystem and reboots a machine; every one of those
// is a syscall with no safe wrapper, and marking each individually would be twenty allows saying
// the same thing less clearly than this does. Each block still carries its own safety note.
#![allow(unsafe_code)]

/// The parts of supervision that are arithmetic rather than syscalls, so they can be tested on a
/// machine that is not a guest. Everything that reads them is Linux-only, which is why the whole
/// module looks unused anywhere else.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only the Linux guest reads any of it")
)]
mod supervise;

#[cfg(target_os = "linux")]
mod guest;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    {
        guest::run()
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("nibrunner-init is a Linux guest's PID 1 and has nothing to do here");
        std::process::ExitCode::FAILURE
    }
}
