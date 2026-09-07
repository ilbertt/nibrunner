pub const FREEZE_REQUEST: &str = "FREEZE\n";
pub const FREEZE_HELD: &str = "OK";
pub const FREEZE_REFUSED_PREFIX: &str = "ERR";

pub const GUEST_SHUTDOWN_GRACE_MS: u64 = 10_000;

pub const GUEST_LOG_PREFIX: &str = "[nibrun] ";

pub fn last_guest_line(console: &str) -> Option<String> {
    console
        .lines()
        .rev()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(GUEST_LOG_PREFIX).map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_line_the_guest_wrote_is_the_verdict_without_its_prefix() {
        let console = [
            "[nibrun] starting the tenant as uid 65534 with data at /app/data",
            "[nibrun] the tenant used its 5 restarts without staying up; shutting the guest down",
            "[   15.736786] reboot: Restarting system",
            "2026-08-26T15:46:48.429 [anonymous-instance:main] Vmm is stopping.",
            "",
        ]
        .join("\n");
        assert_eq!(
            last_guest_line(&console).as_deref(),
            Some("the tenant used its 5 restarts without staying up; shutting the guest down")
        );
        assert_eq!(
            last_guest_line("[    0.000000] Linux version 6.1.180\nVmm is stopping.\n"),
            None
        );
        assert_eq!(last_guest_line(""), None);
    }
}
