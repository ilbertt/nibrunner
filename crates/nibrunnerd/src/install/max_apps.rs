//! What `max_apps` this machine holds, measured the way the daemon measures it, taking an app to
//! be what the protocol assumes one is until a document says otherwise: how many its memory runs
//! at once, how many its disk holds, how many its ports fit. Heuristics, said plainly and never
//! refused on — a host that has no configuration is written the least of them, and one that has
//! is told whether the number it has is still within them.

use std::path::Path;

use protocol::{DEFAULT_INSTANCE_RESOURCES, DEFAULT_VOLUME_SIZE_BYTES};

use crate::adapters::vm::snapshot::{measure_disk_under, snapshot_budget, snapshot_bytes_for};
use crate::adapters::volumes::CacheReservation;
use crate::config::VolumeBackend;
use crate::domain::report::capacity::{
    apps_held_on_disk, apps_up_at_once, guest_memory_mib, read_host_memory_mib,
};

const BYTES_PER_MIB: u64 = 1_048_576;
const BYTES_PER_GIB: u64 = 1_073_741_824;

/// What a bound that could not be measured is taken as: a starting point still has to be
/// written, and a number is what the key takes.
const MAX_APPS_WHEN_UNMEASURED: u32 = 100;

/// A bound as measured, or why this machine could not measure it.
pub type Measured = Result<u32, String>;

/// The bounds on `max_apps`, each in apps of the assumed size. The ports are a constant of the
/// layout rather than a measurement, so they are the one bound that is always known.
#[derive(Debug)]
pub struct Bounds {
    pub memory: Measured,
    pub disk: Measured,
    pub ports: u32,
}

/// Measured on the disk under `disk`, which need not be there yet: this machine's memory less
/// what the host and the storage cache keep, and the disk less the cache and the reserve. The
/// cache is read off the configuration rather than the backend, which `install` has not started.
pub fn measure(disk: &Path, volumes: &VolumeBackend) -> Bounds {
    let cache = volumes
        .zerofs()
        .map_or(CacheReservation::default(), |settings| CacheReservation {
            disk_bytes: settings.cache_disk_mib * BYTES_PER_MIB,
            memory_bytes: settings.cache_memory_mib * BYTES_PER_MIB,
        });
    let memory_mib = DEFAULT_INSTANCE_RESOURCES.memory_mib;
    // On a local-file host the volume is a file on this disk, taken as full; on a zerofs host it
    // is in the store, and what the store caches here is already off the budget.
    let bytes_each = match volumes {
        VolumeBackend::LocalFile => DEFAULT_VOLUME_SIZE_BYTES + snapshot_bytes_for(memory_mib),
        VolumeBackend::Zerofs(_) => snapshot_bytes_for(memory_mib),
    };
    Bounds {
        memory: match read_host_memory_mib() {
            0 => Err("/proc/meminfo could not be read".to_string()),
            host_memory_mib => Ok(apps_up_at_once(
                guest_memory_mib(host_memory_mib, cache.memory_mib()),
                memory_mib,
            )),
        },
        disk: measure_disk_under(disk, cache.disk_bytes)
            .map(|measured| apps_held_on_disk(snapshot_budget(&measured), bytes_each))
            .map_err(|error| format!("{}: {error}", disk.display())),
        ports: nft_render::most_apps_the_ports_fit(),
    }
}

fn assumed() -> String {
    format!(
        "{} vCPU, {} MiB and {} GiB",
        DEFAULT_INSTANCE_RESOURCES.vcpu_count,
        DEFAULT_INSTANCE_RESOURCES.memory_mib,
        DEFAULT_VOLUME_SIZE_BYTES / BYTES_PER_GIB
    )
}

/// What `install` and `start` say of the `max_apps` a host has, against what it holds. Above
/// what the memory runs at once is only said, because that is what sleeping is for; above what
/// the disk holds or the ports fit is a lower number to set, since neither bends.
pub fn said(max_apps: u32, bounds: &Bounds) -> String {
    let ports = bounds.ports;
    let holds = match (&bounds.memory, &bounds.disk) {
        (Ok(memory), Ok(disk)) => {
            format!("runs {memory} at once, holds {disk} on disk, and fits {ports} on its ports")
        }
        (Ok(memory), Err(_)) => format!("runs {memory} at once and fits {ports} on its ports"),
        (Err(_), Ok(disk)) => format!("holds {disk} on disk and fits {ports} on its ports"),
        (Err(_), Err(_)) => format!("fits {ports} on its ports"),
    };
    let mut clauses = vec![format!(
        "assuming an app is {} to start, this host {holds}",
        assumed()
    )];
    if let Ok(memory) = bounds.memory {
        if max_apps > memory {
            clauses.push(format!("above {memory} counts on apps sleeping"));
        }
    }
    let exceeded = [
        ("disk holds", bounds.disk.as_ref().ok().copied()),
        ("ports fit", Some(ports)),
    ]
    .into_iter()
    .filter_map(|(what, bound)| bound.map(|bound| (what, bound.max(1))))
    .filter(|(_, least)| max_apps > *least)
    .min_by_key(|(_, least)| *least);
    if let Some((what, least)) = exceeded {
        clauses.push(format!("more than its {what}: set max_apps = {least}"));
    }
    if let Err(why) = &bounds.memory {
        clauses.push(format!("what it runs at once could not be measured: {why}"));
    }
    if let Err(why) = &bounds.disk {
        clauses.push(format!("what its disk holds could not be measured: {why}"));
    }
    format!("max_apps = {max_apps}: {}", clauses.join("; "))
}

/// What a host with no configuration is laid out for, and the line above the key saying where
/// the number came from: the least of the bounds, never none, with one that could not be measured
/// taken as a stated number rather than as no bound at all.
pub fn starter(bounds: &Bounds) -> (u32, String) {
    let taken = |bound: &Measured| bound.as_ref().copied().unwrap_or(MAX_APPS_WHEN_UNMEASURED);
    let least = taken(&bounds.memory)
        .min(taken(&bounds.disk))
        .min(bounds.ports)
        .max(1);
    let found = |what: &str, bound: &Measured| match bound {
        Ok(count) => format!("{what} {count}"),
        Err(why) => {
            format!("{what} could not be measured ({why}) and is taken as {MAX_APPS_WHEN_UNMEASURED}")
        }
    };
    let derived = format!(
        "To start, install assumed an app to be {}, and measured how many this machine holds: {}, {}, ports {}.",
        assumed(),
        found("memory", &bounds.memory),
        found("disk", &bounds.disk),
        bounds.ports
    );
    (least, derived)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORTS: u32 = 5567;

    fn measured(memory: u32, disk: u32) -> Bounds {
        Bounds {
            memory: Ok(memory),
            disk: Ok(disk),
            ports: PORTS,
        }
    }

    #[test]
    fn the_ports_bound_is_the_one_the_configuration_is_held_to() {
        assert_eq!(PORTS, nft_render::most_apps_the_ports_fit());
    }

    #[test]
    fn a_number_within_every_bound_is_told_them_all() {
        assert_eq!(
            said(200, &measured(245, 1600)),
            "max_apps = 200: assuming an app is 1 vCPU, 256 MiB and 8 GiB to start, this host runs 245 at once, holds 1600 on disk, and fits 5567 on its ports"
        );
        assert_eq!(
            said(245, &measured(245, 1600)),
            "max_apps = 245: assuming an app is 1 vCPU, 256 MiB and 8 GiB to start, this host runs 245 at once, holds 1600 on disk, and fits 5567 on its ports"
        );
    }

    #[test]
    fn a_number_above_only_what_the_memory_runs_at_once_is_told_what_it_counts_on() {
        assert_eq!(
            said(1000, &measured(245, 1600)),
            "max_apps = 1000: assuming an app is 1 vCPU, 256 MiB and 8 GiB to start, this host runs 245 at once, holds 1600 on disk, and fits 5567 on its ports; above 245 counts on apps sleeping"
        );
        assert_eq!(
            said(1600, &measured(245, 1600)),
            "max_apps = 1600: assuming an app is 1 vCPU, 256 MiB and 8 GiB to start, this host runs 245 at once, holds 1600 on disk, and fits 5567 on its ports; above 245 counts on apps sleeping"
        );
    }

    #[test]
    fn a_number_above_what_the_disk_holds_is_told_the_one_to_set() {
        assert_eq!(
            said(5000, &measured(245, 1600)),
            "max_apps = 5000: assuming an app is 1 vCPU, 256 MiB and 8 GiB to start, this host runs 245 at once, holds 1600 on disk, and fits 5567 on its ports; above 245 counts on apps sleeping; more than its disk holds: set max_apps = 1600"
        );
        assert_eq!(
            said(2, &measured(29, 1)),
            "max_apps = 2: assuming an app is 1 vCPU, 256 MiB and 8 GiB to start, this host runs 29 at once, holds 1 on disk, and fits 5567 on its ports; more than its disk holds: set max_apps = 1"
        );
    }

    // The configuration is held to the ports, so it is only ever a wider disk that puts the ports
    // below the number — and then the ports are the bound named, since they are the lower one.
    #[test]
    fn a_number_above_what_the_ports_fit_is_told_the_one_to_set() {
        let few_ports = Bounds {
            ports: 500,
            ..measured(245, 1600)
        };
        assert_eq!(
            said(1000, &few_ports),
            "max_apps = 1000: assuming an app is 1 vCPU, 256 MiB and 8 GiB to start, this host runs 245 at once, holds 1600 on disk, and fits 500 on its ports; above 245 counts on apps sleeping; more than its ports fit: set max_apps = 500"
        );
    }

    // A disk with no room past the reserve holds nothing, and a host laid out for nothing is
    // refused — so the number to set is the least one, and one is not told to lower itself.
    #[test]
    fn a_host_that_holds_nothing_on_disk_is_never_told_to_set_nothing() {
        assert!(said(5, &measured(3, 0)).ends_with("more than its disk holds: set max_apps = 1"));
        assert_eq!(
            said(1, &measured(3, 0)),
            "max_apps = 1: assuming an app is 1 vCPU, 256 MiB and 8 GiB to start, this host runs 3 at once, holds 0 on disk, and fits 5567 on its ports"
        );
    }

    #[test]
    fn a_bound_that_could_not_be_measured_is_said_to_be_and_the_others_are_still_told() {
        let no_memory = Bounds {
            memory: Err("/proc/meminfo could not be read".into()),
            ..measured(0, 1600)
        };
        assert_eq!(
            said(1000, &no_memory),
            "max_apps = 1000: assuming an app is 1 vCPU, 256 MiB and 8 GiB to start, this host holds 1600 on disk and fits 5567 on its ports; what it runs at once could not be measured: /proc/meminfo could not be read"
        );
        assert!(said(5000, &no_memory).contains("set max_apps = 1600"));

        let no_disk = Bounds {
            disk: Err("/data/snapshots: Permission denied (os error 13)".into()),
            ..measured(245, 0)
        };
        assert_eq!(
            said(1000, &no_disk),
            "max_apps = 1000: assuming an app is 1 vCPU, 256 MiB and 8 GiB to start, this host runs 245 at once and fits 5567 on its ports; above 245 counts on apps sleeping; what its disk holds could not be measured: /data/snapshots: Permission denied (os error 13)"
        );

        let nothing = Bounds {
            memory: Err("memory".into()),
            disk: Err("disk".into()),
            ports: PORTS,
        };
        assert_eq!(
            said(1000, &nothing),
            "max_apps = 1000: assuming an app is 1 vCPU, 256 MiB and 8 GiB to start, this host fits 5567 on its ports; what it runs at once could not be measured: memory; what its disk holds could not be measured: disk"
        );
    }

    #[test]
    fn a_host_with_no_configuration_is_laid_out_for_the_least_of_what_it_holds() {
        assert_eq!(
            starter(&measured(245, 1600)),
            (
                245,
                "To start, install assumed an app to be 1 vCPU, 256 MiB and 8 GiB, and measured how many this machine holds: memory 245, disk 1600, ports 5567.".to_string()
            )
        );
        assert_eq!(starter(&measured(29, 1)).0, 1);
        assert_eq!(starter(&measured(6000, 9000)).0, PORTS);
        assert_eq!(starter(&measured(3, 0)).0, 1);
    }

    #[test]
    fn a_bound_a_starter_could_not_measure_is_taken_as_a_stated_number_that_says_so() {
        let no_disk = Bounds {
            disk: Err("/var/lib: Permission denied (os error 13)".into()),
            ..measured(29, 0)
        };
        assert_eq!(
            starter(&no_disk),
            (
                29,
                "To start, install assumed an app to be 1 vCPU, 256 MiB and 8 GiB, and measured how many this machine holds: memory 29, disk could not be measured (/var/lib: Permission denied (os error 13)) and is taken as 100, ports 5567.".to_string()
            )
        );
        let nothing = Bounds {
            memory: Err("memory".into()),
            disk: Err("disk".into()),
            ports: PORTS,
        };
        assert_eq!(starter(&nothing).0, MAX_APPS_WHEN_UNMEASURED);
    }

    // What is measured is the machine the tests run on, so only its shape is held to: each bound
    // measured or said not to be, nothing made, and a volume counted on the disk only where it
    // would be a file on it.
    #[test]
    fn what_this_machine_holds_is_measured_under_a_path_without_making_anything() {
        let directory = tempfile::tempdir().unwrap();
        let unmade = directory.path().join("nibrunner");
        let uncached_store =
            VolumeBackend::Zerofs(Box::new(crate::test_support::zerofs_settings(|settings| {
                settings.cache_disk_mib = 0;
                settings.cache_memory_mib = 0;
            })));
        let bounds = measure(&unmade, &uncached_store);
        assert!(!unmade.exists());
        assert_eq!(bounds.ports, PORTS);
        if let Ok(with_volumes_in_the_store) = bounds.disk {
            let with_volumes_as_files = measure(&unmade, &VolumeBackend::LocalFile).disk.unwrap();
            assert!(with_volumes_as_files <= with_volumes_in_the_store);
        }
        if let Err(why) = &bounds.memory {
            assert!(why.contains("meminfo"), "{why}");
        }
    }
}
