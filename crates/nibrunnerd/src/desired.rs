use std::path::{Path, PathBuf};
use std::time::Duration;

use protocol::HostDesiredState;

use crate::json_store::{read_json, write_json, StoreError};

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

pub fn cache_desired_state(path: &Path, state: &HostDesiredState) -> Result<(), StoreError> {
    write_json(path, state)
}

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

    pub fn accept(&mut self, state: HostDesiredState) -> bool {
        if self.latest.as_ref() == Some(&state) {
            return false;
        }
        self.latest = Some(state);
        true
    }
}

pub struct DesiredStateWatch {
    directory: PathBuf,
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

    pub async fn changed(&self) {
        #[cfg(target_os = "linux")]
        if let Some(inotify) = &self.inotify {
            let deadline = tokio::time::Instant::now() + WATCH_BACKSTOP;
            loop {
                match linux::wait_for(inotify, &self.filename, deadline).await {
                    linux::Settled::Named => return,
                    linux::Settled::RanOut => return,
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

    const WATCHED: AddWatchFlags = AddWatchFlags::IN_CLOSE_WRITE
        .union(AddWatchFlags::IN_MOVED_TO)
        .union(AddWatchFlags::IN_CREATE)
        .union(AddWatchFlags::IN_DELETE)
        .union(AddWatchFlags::IN_MOVED_FROM);

    pub(super) enum Settled {
        Named,
        SomethingElse,
        RanOut,
    }

    pub(super) fn watch(directory: &std::path::Path) -> Option<Inotify> {
        let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC).ok()?;
        inotify.add_watch(directory, WATCHED).ok()?;
        Some(inotify)
    }

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

    #[test]
    fn a_file_that_is_not_json_at_all_is_told_apart_from_one_that_is_the_wrong_document() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("desired.json");
        std::fs::write(&path, "}{").unwrap();
        let error = read_desired_state(&path).unwrap_err();
        assert!(matches!(error, DesiredStateError::Unreadable(_)), "{error}");
        assert!(error.message().contains("does not hold the JSON"));

        std::fs::write(&path, r#"{"hostId":"host-1","volumes":[]}"#).unwrap();
        let error = read_desired_state(&path).unwrap_err();
        assert!(matches!(error, DesiredStateError::Malformed { .. }), "{error}");
        assert!(error.message().contains("desired.json"), "{error}");
    }

    #[test]
    fn a_cache_that_has_taken_nothing_in_yet_has_nothing_to_converge_on() {
        let cache = DesiredStateCache::new();
        assert!(cache.latest().is_none());
        assert!(DesiredStateCache::default().latest().is_none());
    }

    #[test]
    fn a_document_that_moved_back_to_what_it_was_is_still_a_change_to_converge_on() {
        let mut cache = DesiredStateCache::new();
        let empty = desired_state(|_| {});
        let one = desired_state(|state| state.instances = vec![desired_instance(|_| {})]);
        assert!(cache.accept(empty.clone()));
        assert!(cache.accept(one));
        assert!(cache.accept(empty.clone()));
        assert_eq!(cache.latest(), Some(&empty));
    }

    #[test]
    fn a_watch_makes_the_directory_it_is_asked_to_watch() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("state");
        let watch = DesiredStateWatch::on(&nested.join("desired.json"));
        assert_eq!(watch.directory(), nested);
        assert!(nested.is_dir());
    }

    #[test]
    fn a_path_with_no_parent_at_all_is_watched_where_this_host_stands() {
        assert_eq!(DesiredStateWatch::on(Path::new("/")).directory(), Path::new("."));
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
        let settled = tokio::time::timeout(Duration::from_secs(5), watch.changed()).await;
        writer.await.unwrap();
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
