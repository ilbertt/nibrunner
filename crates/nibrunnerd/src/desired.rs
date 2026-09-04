//! Where desired state comes from: one file, watched.
//!
//! Watched rather than polled. A poll is a decision about how stale the host may be, taken by
//! whoever set the interval and paid for on every tick of every quiet host; a watch costs nothing
//! until the file moves and reacts as fast as the write. The interval that remains is a backstop
//! rather than the mechanism — a file moved on a filesystem that reports no events, an editor
//! that replaced the inode, a watch the kernel dropped — so it is measured in tens of seconds
//! rather than in the latency of a deploy.
//!
//! A remote control plane writes this file too. That is what keeps it an addon: the reconciler
//! has one source, and a host that loses the control plane goes on converging on the last
//! document it was given rather than on nothing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use protocol::HostDesiredState;

use crate::json_store::{read_json, write_json, StoreError};

/// The longest a change can go unnoticed if every watch this daemon holds has silently stopped
/// working. Long enough to cost a quiet host nothing, short enough that a host is never wrong for
/// an hour.
pub const WATCH_BACKSTOP: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum DesiredStateError {
    #[error("{0}")]
    Unreadable(#[from] StoreError),
    #[error("{path} holds a document this host cannot read: {reason}")]
    Malformed { path: String, reason: String },
}

impl DesiredStateError {
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// The document as it stands, or nothing where the file is not there yet — which is the ordinary
/// state of a host nobody has deployed to.
pub fn read_desired_state(path: &Path) -> Result<Option<HostDesiredState>, DesiredStateError> {
    let Some(value) = read_json::<serde_json::Value>(path)? else {
        return Ok(None);
    };
    serde_json::from_value(value)
        .map(Some)
        .map_err(|error| DesiredStateError::Malformed {
            path: path.display().to_string(),
            reason: error.to_string(),
        })
}

/// The last document this host was given, kept beside its own state so a restart during an outage
/// of whatever writes the file is a non-event: the host still knows what it is supposed to be
/// running.
pub fn cache_desired_state(path: &Path, state: &HostDesiredState) -> Result<(), StoreError> {
    write_json(path, state)
}

/// Holding the document is also what decides whether a change is news. Whatever wrote the file
/// says nothing about whether it moved, so the comparison lives with the only party that knows
/// what it converged on.
#[derive(Debug, Default)]
pub struct DesiredStateCache {
    latest: Option<HostDesiredState>,
}

impl DesiredStateCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn latest(&self) -> Option<&HostDesiredState> {
        self.latest.as_ref()
    }

    /// Whether this differs from what the host holds, and so whether it is worth converging on.
    pub fn accept(&mut self, state: HostDesiredState) -> bool {
        if self.latest.as_ref() == Some(&state) {
            return false;
        }
        self.latest = Some(state);
        true
    }
}

/// What the daemon holds open on the file. On Linux this is an inotify watch on the directory
/// rather than on the file itself: a document is replaced by a rename, which leaves a watch on
/// the old inode watching a file nothing will ever write to again.
///
/// Watching a directory means hearing about every file in it, and the daemon writes its own
/// state — the report, the records, the slots — into whatever directory it was pointed at. So
/// the events are read by name and everything but the document is dropped: without that, this
/// host wakes its own watch several times a second and re-parses a document nothing touched.
pub struct DesiredStateWatch {
    directory: PathBuf,
    /// Read only where there is an inotify to filter. Off Linux the backstop is the whole watch.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    filename: std::ffi::OsString,
    #[cfg(target_os = "linux")]
    inotify: Option<nix::sys::inotify::Inotify>,
}

impl DesiredStateWatch {
    pub fn on(path: &Path) -> Self {
        let directory = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let _ = crate::json_store::make_directory(&directory, 0o700);
        Self {
            #[cfg(target_os = "linux")]
            inotify: linux::watch(&directory),
            filename: path.file_name().unwrap_or_default().to_os_string(),
            directory,
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Settles when the document has changed, or when the backstop runs out. A watch that could
    /// not be established never settles early, which leaves the backstop doing the whole job
    /// rather than the daemon doing none of it.
    pub async fn changed(&self) {
        #[cfg(target_os = "linux")]
        if let Some(inotify) = &self.inotify {
            let deadline = tokio::time::Instant::now() + WATCH_BACKSTOP;
            loop {
                match linux::wait_for(inotify, &self.filename, deadline).await {
                    linux::Settled::Named => return,
                    linux::Settled::RanOut => return,
                    // Something else in the directory moved, which on a host is this daemon
                    // writing its own state. Nothing to converge on, so it waits again.
                    linux::Settled::SomethingElse => continue,
                }
            }
        }
        tokio::time::sleep(WATCH_BACKSTOP).await;
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsStr;
    use std::os::fd::{AsFd, AsRawFd};

    use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};

    /// Every way a document arrives: written in place, renamed over, or removed and recreated.
    const WATCHED: AddWatchFlags = AddWatchFlags::IN_CLOSE_WRITE
        .union(AddWatchFlags::IN_MOVED_TO)
        .union(AddWatchFlags::IN_CREATE)
        .union(AddWatchFlags::IN_DELETE)
        .union(AddWatchFlags::IN_MOVED_FROM);

    /// What a wait came back for, which is what tells the caller whether it has news, whether to
    /// wait again, or whether the backstop is now its job.
    pub(super) enum Settled {
        /// The document itself moved.
        Named,
        /// Something else in the directory moved, which on a host is this daemon's own writing.
        SomethingElse,
        /// The deadline arrived first.
        RanOut,
    }

    pub(super) fn watch(directory: &std::path::Path) -> Option<Inotify> {
        let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC).ok()?;
        inotify.add_watch(directory, WATCHED).ok()?;
        Some(inotify)
    }

    /// Read by name rather than drained blind: the name is the only thing separating a deploy from
    /// this daemon writing its own report into the same directory.
    pub(super) async fn wait_for(
        inotify: &Inotify,
        filename: &OsStr,
        deadline: tokio::time::Instant,
    ) -> Settled {
        let raw = inotify.as_fd().as_raw_fd();
        let Ok(async_fd) =
            tokio::io::unix::AsyncFd::with_interest(RawDescriptor(raw), tokio::io::Interest::READABLE)
        else {
            return Settled::RanOut;
        };
        let Ok(Ok(mut ready)) = tokio::time::timeout_at(deadline, async_fd.readable()).await else {
            return Settled::RanOut;
        };
        // The whole queue is read before answering, because a rename over the document can arrive
        // behind something this daemon wrote a moment earlier, and an event left unread would make
        // the next wait return at once on news already spent.
        let mut named = false;
        while let Ok(events) = inotify.read_events() {
            if events.is_empty() {
                break;
            }
            named |= events.iter().any(|event| event.name.as_deref() == Some(filename));
        }
        ready.clear_ready();
        if named {
            Settled::Named
        } else {
            Settled::SomethingElse
        }
    }

    /// A descriptor tokio may poll without owning: the watch outlives every wait taken on it.
    struct RawDescriptor(std::os::fd::RawFd);

    impl AsRawFd for RawDescriptor {
        fn as_raw_fd(&self) -> std::os::fd::RawFd {
            self.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{desired_instance, desired_state};

    #[test]
    fn a_file_that_is_not_there_yet_is_the_ordinary_state_of_a_fresh_host() {
        let directory = tempfile::tempdir().unwrap();
        assert!(read_desired_state(&directory.path().join("desired.json"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_document_round_trips_and_one_that_is_not_a_document_says_so() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("desired.json");
        let state = desired_state(|state| state.instances = vec![desired_instance(|_| {})]);
        cache_desired_state(&path, &state).unwrap();
        assert_eq!(read_desired_state(&path).unwrap(), Some(state));
        std::fs::write(&path, r#"{"hostId":"host-1"}"#).unwrap();
        assert!(read_desired_state(&path)
            .unwrap_err()
            .message()
            .contains("cannot read"));
    }

    #[test]
    fn only_a_document_that_moved_is_worth_converging_on() {
        let mut cache = DesiredStateCache::new();
        let state = desired_state(|_| {});
        assert!(cache.accept(state.clone()));
        assert!(!cache.accept(state));
        assert!(cache.accept(desired_state(
            |state| state.instances = vec![desired_instance(|_| {})]
        )));
        assert_eq!(cache.latest().map(|state| state.instances.len()), Some(1));
    }

    #[tokio::test]
    async fn a_watch_settles_when_the_document_is_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("desired.json");
        let watch = DesiredStateWatch::on(&path);
        let writer = tokio::spawn({
            let path = path.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                cache_desired_state(&path, &desired_state(|_| {})).unwrap();
            }
        });
        // Bounded well under the backstop, so a pass only settles here because the write did it.
        let settled = tokio::time::timeout(Duration::from_secs(5), watch.changed()).await;
        writer.await.unwrap();
        // Off Linux there is no watch to settle, so this only asserts the backstop is not what ran.
        if cfg!(target_os = "linux") {
            assert!(settled.is_ok(), "the watch should have settled on the write");
        }
    }

    #[tokio::test]
    async fn a_write_to_something_else_in_the_directory_does_not_settle_the_watch() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("desired.json");
        cache_desired_state(&path, &desired_state(|_| {})).unwrap();
        let watch = DesiredStateWatch::on(&path);

        let writer = tokio::spawn({
            let sibling = directory.path().join("reported.json");
            async move {
                for _ in 0..5 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    crate::json_store::write_json(&sibling, &serde_json::json!({ "a": 1 })).unwrap();
                }
            }
        });
        let settled = tokio::time::timeout(Duration::from_millis(400), watch.changed()).await;
        writer.await.unwrap();
        assert!(settled.is_err(), "a sibling moving is not the document moving");
    }
}
