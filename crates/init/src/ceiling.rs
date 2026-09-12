//! What the tenant may spend, and how to tell it is thrashing at that ceiling. Decisions only;
//! the cgroup they are enforced through is `guest::memory`, which exists on Linux alone.
//!
//! A guest with no swap that runs out of memory does not fail. The only pages left to reclaim are
//! the program's own code, so the kernel evicts them and reads them back on the next instruction,
//! for ever; the process still accepts connections, so nothing outside can tell. On the Hetzner
//! host that was a vCPU pinned at 100 % and a request answered a second late, indefinitely, from
//! a sixty-second burst of five hundred connections — in Go and in Bun alike. A cgroup with less
//! than the guest to spend makes a burst of anonymous memory an OOM kill instead; and since a
//! cgroup at its limit can still thrash its own file pages, the watch below looks for exactly
//! that — memory at the ceiling and major faults by the thousand — so the supervisor can end it.

use std::time::{Duration, Instant};

/// What the kernel, this runtime and the page cache the kernel itself needs keep out of the
/// tenant's reach. The kernel image and its reserved memory are already outside `MemTotal`.
const HEADROOM_BYTES: u64 = 32 * 1024 * 1024;

/// Below this, a guest is too small for a ceiling under the headroom to leave the tenant anything
/// worth running in; it gets the guest's total and the watch alone.
const SMALLEST_CEILING_BYTES: u64 = 64 * 1024 * 1024;

/// How close to the ceiling counts as at it. Usage sits exactly at `memory.max` only while the
/// kernel is reclaiming to keep it there, and a little under it between allocations.
const AT_THE_CEILING_PERCENT: u64 = 90;

/// Major faults a second that mean the program is being read back from disk faster than it can
/// run. A cold start of a large program pages in at this rate too, but not with memory at the
/// ceiling, and not for this long.
const THRASHING_FAULTS_PER_SECOND: u64 = 1000;

/// Consecutive seconds of that before the tenant is killed for it.
const STRIKES_BEFORE_A_KILL: u32 = 3;

pub(crate) const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) fn ceiling_for(guest_total_bytes: u64) -> u64 {
    let under_headroom = guest_total_bytes.saturating_sub(HEADROOM_BYTES);
    if under_headroom < SMALLEST_CEILING_BYTES {
        guest_total_bytes
    } else {
        under_headroom
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reading {
    pub current_bytes: u64,
    pub major_faults: u64,
    pub oom_kills: u64,
}

/// A `name value` line out of a cgroup stat file, by name and whole.
pub(crate) fn field(text: &str, name: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(' ')?.trim().parse().ok())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Fine,
    /// At the ceiling and faulting its code back in by the thousand, for long enough.
    Thrashing {
        faults_per_second: u64,
    },
}

/// One reading a second, judged against the last. The first says nothing: a rate needs two.
#[derive(Debug)]
pub(crate) struct Watch {
    limit_bytes: u64,
    last: Option<(Reading, Instant)>,
    strikes: u32,
}

impl Watch {
    pub(crate) fn new(limit_bytes: u64) -> Self {
        Self {
            limit_bytes,
            last: None,
            strikes: 0,
        }
    }

    pub(crate) fn observe(&mut self, reading: Reading, now: Instant) -> Verdict {
        let Some((before, then)) = self.last.replace((reading, now)) else {
            return Verdict::Fine;
        };
        let elapsed = now.saturating_duration_since(then).as_secs_f64();
        if elapsed <= 0.0 {
            return Verdict::Fine;
        }
        let faults_per_second =
            (reading.major_faults.saturating_sub(before.major_faults) as f64 / elapsed) as u64;
        let at_the_ceiling = reading.current_bytes * 100 >= self.limit_bytes * AT_THE_CEILING_PERCENT;
        if at_the_ceiling && faults_per_second >= THRASHING_FAULTS_PER_SECOND {
            self.strikes += 1;
        } else {
            self.strikes = 0;
        }
        if self.strikes >= STRIKES_BEFORE_A_KILL {
            self.strikes = 0;
            Verdict::Thrashing { faults_per_second }
        } else {
            Verdict::Fine
        }
    }

    /// Whether the kernel's own OOM killer has acted between two readings.
    pub(crate) fn kernel_killed_between(before: &Reading, after: &Reading) -> bool {
        after.oom_kills > before.oom_kills
    }
}

pub(crate) fn mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn the_ceiling_leaves_the_kernel_its_headroom() {
        assert_eq!(ceiling_for(245 * MIB), 213 * MIB);
        assert_eq!(ceiling_for(4096 * MIB), 4064 * MIB);
    }

    // A ceiling that would leave less than the smallest useful amount is no ceiling: the
    // watchdog still stands, and a kernel OOM kill at the guest's edge is still an exit.
    #[test]
    fn a_guest_too_small_for_headroom_gets_all_of_itself() {
        assert_eq!(ceiling_for(80 * MIB), 80 * MIB);
        assert_eq!(ceiling_for(96 * MIB), 64 * MIB);
    }

    fn reading(current_mib: u64, faults: u64) -> Reading {
        Reading {
            current_bytes: current_mib * MIB,
            major_faults: faults,
            oom_kills: 0,
        }
    }

    fn seconds(n: u64) -> Instant {
        // Any fixed origin; only differences are read.
        static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *ORIGIN.get_or_init(Instant::now) + Duration::from_secs(n)
    }

    #[test]
    fn at_the_ceiling_and_faulting_for_three_seconds_running_is_a_thrash() {
        let mut watch = Watch::new(200 * MIB);
        assert_eq!(watch.observe(reading(198, 0), seconds(0)), Verdict::Fine);
        assert_eq!(watch.observe(reading(198, 5_000), seconds(1)), Verdict::Fine);
        assert_eq!(watch.observe(reading(199, 10_000), seconds(2)), Verdict::Fine);
        assert_eq!(
            watch.observe(reading(199, 15_000), seconds(3)),
            Verdict::Thrashing {
                faults_per_second: 5_000
            }
        );
    }

    // Paging a large program in at start looks like faulting by the thousand, but memory is
    // nowhere near the ceiling then.
    #[test]
    fn faulting_with_room_to_spare_is_a_program_starting_not_a_thrash() {
        let mut watch = Watch::new(200 * MIB);
        for second in 0..10 {
            assert_eq!(
                watch.observe(reading(60, second * 8_000), seconds(second)),
                Verdict::Fine
            );
        }
    }

    // Memory at the ceiling on its own is a program using what it was given.
    #[test]
    fn full_but_not_faulting_is_a_program_using_its_memory() {
        let mut watch = Watch::new(200 * MIB);
        for second in 0..10 {
            assert_eq!(
                watch.observe(reading(199, second * 10), seconds(second)),
                Verdict::Fine
            );
        }
    }

    #[test]
    fn a_quiet_second_in_between_starts_the_count_again() {
        let mut watch = Watch::new(200 * MIB);
        watch.observe(reading(199, 0), seconds(0));
        watch.observe(reading(199, 5_000), seconds(1));
        watch.observe(reading(199, 10_000), seconds(2));
        assert_eq!(watch.observe(reading(199, 10_100), seconds(3)), Verdict::Fine);
        assert_eq!(watch.observe(reading(199, 15_000), seconds(4)), Verdict::Fine);
        assert_eq!(watch.observe(reading(199, 20_000), seconds(5)), Verdict::Fine);
        assert!(matches!(
            watch.observe(reading(199, 25_000), seconds(6)),
            Verdict::Thrashing { .. }
        ));
    }

    // The rate is per second of wall clock, so a sample that came late does not read as a burst.
    #[test]
    fn a_late_sample_is_judged_by_the_time_it_covered() {
        let mut watch = Watch::new(200 * MIB);
        watch.observe(reading(199, 0), seconds(0));
        // 3000 faults over 5 s is 600 a second, under the mark.
        assert_eq!(watch.observe(reading(199, 3_000), seconds(5)), Verdict::Fine);
        assert_eq!(watch.observe(reading(199, 3_600), seconds(6)), Verdict::Fine);
        assert_eq!(watch.observe(reading(199, 4_200), seconds(7)), Verdict::Fine);
    }

    #[test]
    fn the_fields_the_kernel_writes_are_read_by_name() {
        let stat = "anon 1000\nfile 2000\npgfault 300\npgmajfault 42\npgrefill 7\n";
        assert_eq!(field(stat, "pgmajfault"), Some(42));
        assert_eq!(field(stat, "pgfault"), Some(300));
        assert_eq!(field(stat, "pgsteal"), None);
        assert_eq!(
            field("low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\n", "oom_kill"),
            Some(1)
        );
    }

    #[test]
    fn a_kernel_kill_shows_as_the_counter_moving() {
        let before = Reading {
            oom_kills: 0,
            ..reading(100, 0)
        };
        let after = Reading {
            oom_kills: 1,
            ..reading(10, 0)
        };
        assert!(Watch::kernel_killed_between(&before, &after));
        assert!(!Watch::kernel_killed_between(&after, &after));
    }
}
