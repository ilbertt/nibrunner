#![allow(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::panic, clippy::expect_used))]

#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only the Linux guest reads any of it")
)]
mod ceiling;
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only the Linux guest reads any of it")
)]
mod supervise;

mod crontab;

#[cfg(target_os = "linux")]
mod guest;

fn main() -> std::process::ExitCode {
    let arguments: Vec<_> = std::env::args_os().collect();
    if let Some(arguments) = crontab::invoked(&arguments) {
        return crontab::run(arguments);
    }
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
