//! The kernel's connection-tracking table, read at each scrape. Every flow between the proxy and a
//! guest is an entry, DNAT'd from the guest's host port, and so is every flow a guest opens out;
//! the table is one for the host, and a full one drops packets for every app on it, silently
//! everywhere but dmesg — which is where it was found the first time. So it is on the page, and
//! said once on the way up past four fifths and once on the way back under.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::domain::metrics::{Kind, Metric, Page};

const COUNT: &str = "/proc/sys/net/netfilter/nf_conntrack_count";
const MAX: &str = "/proc/sys/net/netfilter/nf_conntrack_max";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conntrack {
    pub used: u64,
    pub max: u64,
}

impl Conntrack {
    /// Off the kernel; none where nothing tracks — no nf_conntrack loaded, or not Linux.
    pub fn read() -> Option<Self> {
        Some(Self {
            used: read(COUNT)?,
            max: read(MAX)?,
        })
    }

    fn nearly_full(self) -> bool {
        self.max > 0 && self.used * 5 >= self.max * 4
    }
}

fn read(path: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Which way the table crossed four fifths of its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Crossing {
    Filling,
    BackUnder,
}

/// Whether the table has been said to be nearly full, so that it is said once each way rather
/// than on every scrape it stays there.
#[derive(Debug, Default)]
pub struct ConntrackWatch {
    nearly_full: AtomicBool,
}

impl ConntrackWatch {
    fn crossed(&self, conntrack: Conntrack) -> Option<Crossing> {
        let nearly_full = conntrack.nearly_full();
        match (self.nearly_full.swap(nearly_full, Ordering::Relaxed), nearly_full) {
            (false, true) => Some(Crossing::Filling),
            (true, false) => Some(Crossing::BackUnder),
            _ => None,
        }
    }

    /// Both at WARN, so a log filtered to warnings carries the all-clear beside the alarm.
    pub fn say(&self, conntrack: Conntrack) {
        let Conntrack { used, max } = conntrack;
        match self.crossed(conntrack) {
            Some(Crossing::Filling) => tracing::warn!(
                used,
                max,
                "the conntrack table is past four fifths: full, the kernel drops packets for every app on this host. max_apps sizes it"
            ),
            Some(Crossing::BackUnder) => {
                tracing::warn!(used, max, "the conntrack table is back under four fifths")
            }
            None => {}
        }
    }
}

static CONNTRACK_ENTRIES: Metric = Metric {
    name: "nibrunner_conntrack_entries",
    help: "The kernel's connection-tracking table, which every flow between the proxy and a guest and every flow a guest opens is an entry of, for its lifetime and 120 s after. Full, the kernel drops packets for every app on the host. max_apps in config.toml sizes the max. Absent where the kernel tracks nothing.",
    kind: Kind::Gauge,
    labels: &["of"],
};

pub(super) static DECLARED: &[&Metric] = &[&CONNTRACK_ENTRIES];

pub(super) fn render(page: &mut Page, conntrack: Option<Conntrack>) {
    page.declare(&CONNTRACK_ENTRIES);
    if let Some(Conntrack { used, max }) = conntrack {
        page.value(&CONNTRACK_ENTRIES, &[("of", "used")], used);
        page.value(&CONNTRACK_ENTRIES, &[("of", "max")], max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(used: u64) -> Conntrack {
        Conntrack { used, max: 1000 }
    }

    #[test]
    fn four_fifths_is_said_once_on_the_way_up_and_once_on_the_way_back_under() {
        let watch = ConntrackWatch::default();
        assert_eq!(watch.crossed(at(0)), None);
        assert_eq!(watch.crossed(at(799)), None);
        assert_eq!(watch.crossed(at(800)), Some(Crossing::Filling));
        assert_eq!(watch.crossed(at(950)), None, "still there is not news");
        assert_eq!(watch.crossed(at(1000)), None);
        assert_eq!(watch.crossed(at(700)), Some(Crossing::BackUnder));
        assert_eq!(watch.crossed(at(100)), None);
        assert_eq!(watch.crossed(at(900)), Some(Crossing::Filling));
    }

    #[test]
    fn a_table_of_no_size_is_never_nearly_full() {
        let watch = ConntrackWatch::default();
        assert_eq!(watch.crossed(Conntrack { used: 0, max: 0 }), None);
    }

    #[test]
    fn a_host_that_tracks_nothing_has_the_family_and_no_value() {
        let mut page = Page::new();
        render(&mut page, None);
        assert!(page.0.contains("# TYPE nibrunner_conntrack_entries gauge\n"));
        assert!(!page.0.contains("nibrunner_conntrack_entries{"));

        let mut page = Page::new();
        render(
            &mut page,
            Some(Conntrack {
                used: 210_000,
                max: 262_144,
            }),
        );
        assert!(page
            .0
            .contains("nibrunner_conntrack_entries{of=\"used\"} 210000\n"));
        assert!(page
            .0
            .contains("nibrunner_conntrack_entries{of=\"max\"} 262144\n"));
    }
}
