use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use protocol::AppId;

use crate::json_store::make_directory;
use crate::ports::{LogSink, TenantLogEvent};

const LOG_DIR_MODE: u32 = 0o700;

fn previous_path(path: &Path) -> PathBuf {
    let mut previous = path.as_os_str().to_owned();
    previous.push(".1");
    PathBuf::from(previous)
}

pub struct FileLogSink {
    directory: PathBuf,
    /// Where `<appId>.jsonl` is cut: past this, it becomes `<appId>.jsonl.1` and a fresh one starts,
    /// so an app has between one and two of these on disk however much it writes. The disk is
    /// the one every other tenant's volume cache and snapshots are on.
    keep_bytes_per_app: u64,
    // One handle held open per app: a busy tenant's output is no longer paid for with an
    // open()/close() around every line, which is what capped the drain. Writes are buffered and
    // flushed once the batch the receiver handed us has drained, so a daemon crash loses nothing
    // that a publish returned for.
    writers: Mutex<BTreeMap<AppId, AppLog>>,
}

struct AppLog {
    writer: BufWriter<File>,
    // Counted here rather than asked of the file: the buffered tail is not on disk yet, and the
    // handle keeps writing to its inode after a rename, whatever the path then holds.
    size: u64,
}

impl AppLog {
    fn open(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            writer: BufWriter::new(file),
            size,
        })
    }
}

impl Write for AppLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.writer.write(buf)?;
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

impl FileLogSink {
    pub fn new(directory: PathBuf, keep_bytes_per_app: u64) -> Self {
        Self {
            directory,
            keep_bytes_per_app,
            writers: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn path_for(&self, app_id: &AppId) -> PathBuf {
        self.directory.join(format!("{app_id}.jsonl"))
    }

    /// What `<appId>.jsonl` held before the last cut.
    pub fn previous_path_for(&self, app_id: &AppId) -> PathBuf {
        previous_path(&self.path_for(app_id))
    }

    /// Everything this host kept for an app that is no longer on it, taken under the lock a
    /// publish holds too, so that no batch lands between the handle going and the files. The
    /// handle goes first: one left open holds the bytes against the unlinked inode for as long as
    /// the daemon runs, and would have whatever is deployed under this name next appending to
    /// output it never wrote.
    pub fn discard(&self, app_id: &AppId) {
        let mut writers = self.writers.lock().expect("no panic holds the log writer lock");
        drop(writers.remove(app_id));
        // Old hosts may still have text logs to remove when an app leaves.
        for path in [
            self.path_for(app_id),
            self.directory.join(format!("{app_id}.log")),
        ]
        .into_iter()
        .flat_map(|current| {
            let previous = previous_path(&current);
            [current, previous]
        }) {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(%app_id, %error, "tenant output outlived the app it was kept for")
                }
            }
        }
    }

    /// The cut. The file is renamed over the previous one and the handle let go; the next line
    /// opens a fresh `<appId>.jsonl`, so the newest output is always at the path an operator tails.
    fn rotate(&self, app_id: &AppId, mut full: AppLog) {
        if let Err(error) = full.flush() {
            tracing::warn!(%app_id, %error, "tenant output could not be flushed");
        }
        drop(full);
        match std::fs::rename(self.path_for(app_id), self.previous_path_for(app_id)) {
            Ok(()) => {}
            // Already moved out from under the handle, by whoever is reading it: nothing to keep.
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%app_id, %error, "tenant output could not be rotated")
            }
        }
    }
}

#[async_trait]
impl LogSink for FileLogSink {
    async fn publish(&self, events: Vec<TenantLogEvent>) {
        if events.is_empty() {
            return;
        }
        let mut writers = self.writers.lock().expect("no panic holds the log writer lock");
        let mut touched: BTreeSet<AppId> = BTreeSet::new();
        for event in events {
            if !writers.contains_key(&event.app_id) {
                if let Err(error) = make_directory(&self.directory, LOG_DIR_MODE) {
                    tracing::warn!(%error, "tenant logs have nowhere to go");
                    continue;
                }
                match AppLog::open(&self.path_for(&event.app_id)) {
                    Ok(log) => {
                        writers.insert(event.app_id.clone(), log);
                    }
                    Err(error) => {
                        tracing::warn!(app_id = %event.app_id, %error, "tenant output could not be written");
                        continue;
                    }
                }
            }
            let Some(log) = writers.get_mut(&event.app_id) else {
                continue;
            };
            let rendered = serde_json::to_writer(&mut *log, &event)
                .map_err(std::io::Error::other)
                .and_then(|()| log.write_all(b"\n"));
            if let Err(error) = rendered {
                tracing::warn!(app_id = %event.app_id, %error, "tenant output could not be written");
                continue;
            }
            if log.size >= self.keep_bytes_per_app {
                if let Some(full) = writers.remove(&event.app_id) {
                    self.rotate(&event.app_id, full);
                }
            }
            touched.insert(event.app_id);
        }
        for app_id in touched {
            if let Some(log) = writers.get_mut(&app_id) {
                if let Err(error) = log.flush() {
                    tracing::warn!(%app_id, %error, "tenant output could not be flushed");
                }
            }
        }
    }
}

impl Drop for FileLogSink {
    fn drop(&mut self) {
        // A BufWriter flushes as it drops, but it swallows the error; a torn-down sink says so
        // rather than losing the tail of a tenant's output in silence.
        if let Ok(mut writers) = self.writers.lock() {
            for (app_id, log) in writers.iter_mut() {
                if let Err(error) = log.flush() {
                    tracing::warn!(%app_id, %error, "tenant output could not be flushed as its sink was torn down");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{app_id, deployment_id, observed_at, tenant_restart};
    use protocol::{TenantLogBody, TenantLogStream};

    fn event(body: TenantLogBody, sequence: u64) -> TenantLogEvent {
        TenantLogEvent {
            app_id: app_id(),
            deployment_id: deployment_id(),
            source_id: "source-1".into(),
            sequence,
            observed_at: observed_at(),
            body,
        }
    }

    /// A sink with a cap no test here reaches.
    fn unbounded(logs: PathBuf) -> FileLogSink {
        FileLogSink::new(logs, u64::MAX)
    }

    #[tokio::test]
    async fn json_lines_preserve_every_event_identity_and_embedded_newline() {
        let directory = tempfile::tempdir().unwrap();
        let sink = unbounded(directory.path().join("logs"));
        let events = vec![
            event(
                TenantLogBody::Data {
                    stream: TenantLogStream::Stdout,
                    text: "first\nsecond".into(),
                },
                0,
            ),
            event(
                TenantLogBody::CronData {
                    stream: TenantLogStream::Stderr,
                    text: "cron output".into(),
                    job_id: "cron-job".into(),
                    run_id: "cron-run".into(),
                },
                1,
            ),
            event(TenantLogBody::Gap { dropped_bytes: 1024 }, 2),
            event(TenantLogBody::Restart(tenant_restart(|_| {})), 3),
        ];
        sink.publish(events.clone()).await;
        let written = std::fs::read_to_string(sink.path_for(&app_id())).unwrap();
        let recovered: Vec<TenantLogEvent> = written
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(recovered, events);
        assert!(sink.path_for(&app_id()).ends_with(format!("{}.jsonl", app_id())));
        let line: serde_json::Value = serde_json::from_str(written.lines().nth(1).unwrap()).unwrap();
        assert_eq!(line["deploymentId"], deployment_id().as_str());
        assert_eq!(line["kind"], "cron-data");
        assert_eq!(line["jobId"], "cron-job");
        assert_eq!(line["runId"], "cron-run");
    }

    #[tokio::test]
    async fn json_rotation_keeps_complete_records_and_discard_removes_legacy_files() {
        let directory = tempfile::tempdir().unwrap();
        let sink = FileLogSink::new(directory.path().join("logs"), 1);
        let first = event(TenantLogBody::Gap { dropped_bytes: 1 }, 0);
        sink.publish(vec![first.clone()]).await;
        let written = std::fs::read_to_string(sink.previous_path_for(&app_id())).unwrap();
        assert_eq!(serde_json::from_str::<TenantLogEvent>(&written).unwrap(), first);
        assert!(!sink.path_for(&app_id()).exists());
        let text = directory.path().join("logs").join(format!("{}.log", app_id()));
        std::fs::write(&text, "old text output").unwrap();
        std::fs::write(previous_path(&text), "older text output").unwrap();
        sink.discard(&app_id());
        assert!(!text.exists());
        assert!(!previous_path(&text).exists());
        assert!(!sink.previous_path_for(&app_id()).exists());
    }

    #[tokio::test]
    async fn output_lands_in_one_file_per_app_with_a_gap_in_its_place_in_the_stream() {
        let directory = tempfile::tempdir().unwrap();
        let sink = unbounded(directory.path().join("logs"));
        let events = vec![
            event(
                TenantLogBody::Data {
                    stream: TenantLogStream::Stdout,
                    text: "listening".into(),
                },
                0,
            ),
            event(TenantLogBody::Gap { dropped_bytes: 4096 }, 1),
            event(
                TenantLogBody::Data {
                    stream: TenantLogStream::Stderr,
                    text: "warning".into(),
                },
                2,
            ),
        ];
        sink.publish(events.clone()).await;
        assert_eq!(events_in(&sink.path_for(&app_id())), events);
        sink.publish(vec![event(
            TenantLogBody::Data {
                stream: TenantLogStream::Stdout,
                text: "again".into(),
            },
            3,
        )])
        .await;
        assert_eq!(
            std::fs::read_to_string(sink.path_for(&app_id()))
                .unwrap()
                .lines()
                .count(),
            4
        );
    }

    #[tokio::test]
    async fn a_publish_with_nothing_in_it_does_not_make_a_place_to_put_it() {
        let directory = tempfile::tempdir().unwrap();
        let logs = directory.path().join("logs");
        unbounded(logs.clone()).publish(vec![]).await;
        assert!(!logs.exists());
    }

    #[tokio::test]
    async fn no_tenant_ever_reads_a_line_another_tenant_wrote() {
        let directory = tempfile::tempdir().unwrap();
        let sink = unbounded(directory.path().join("logs"));
        let neighbour = protocol::AppId::parse("app-2").unwrap();
        sink.publish(vec![
            event(
                TenantLogBody::Data {
                    stream: TenantLogStream::Stdout,
                    text: "mine".into(),
                },
                0,
            ),
            TenantLogEvent {
                app_id: neighbour.clone(),
                ..event(
                    TenantLogBody::Data {
                        stream: TenantLogStream::Stdout,
                        text: "theirs".into(),
                    },
                    1,
                )
            },
        ])
        .await;
        assert_ne!(sink.path_for(&app_id()), sink.path_for(&neighbour));
        let mine = std::fs::read_to_string(sink.path_for(&app_id())).unwrap();
        assert!(mine.contains("mine"));
        assert!(!mine.contains("theirs"));
        assert!(std::fs::read_to_string(sink.path_for(&neighbour))
            .unwrap()
            .contains("theirs"));
    }

    #[tokio::test]
    async fn a_flood_of_lines_across_many_batches_all_land_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let sink = unbounded(directory.path().join("logs"));
        let mut sequence = 0u64;
        for _ in 0..200 {
            let batch: Vec<TenantLogEvent> = (0..50)
                .map(|_| {
                    let at = sequence;
                    sequence += 1;
                    event(
                        TenantLogBody::Data {
                            stream: TenantLogStream::Stdout,
                            text: format!("line {at}"),
                        },
                        at,
                    )
                })
                .collect();
            sink.publish(batch).await;
        }
        let written = std::fs::read_to_string(sink.path_for(&app_id())).unwrap();
        let lines: Vec<TenantLogEvent> = written
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 10_000);
        for (sequence, entry) in lines.into_iter().enumerate() {
            assert_eq!(
                entry,
                event(
                    TenantLogBody::Data {
                        stream: TenantLogStream::Stdout,
                        text: format!("line {sequence}"),
                    },
                    sequence as u64
                )
            );
        }
    }

    #[tokio::test]
    async fn the_handle_is_kept_open_so_a_later_batch_does_not_reopen_the_path() {
        let directory = tempfile::tempdir().unwrap();
        let logs = directory.path().join("logs");
        let sink = unbounded(logs.clone());
        sink.publish(vec![event(
            TenantLogBody::Data {
                stream: TenantLogStream::Stdout,
                text: "before".into(),
            },
            0,
        )])
        .await;

        // Move the file out of the way. A sink that reopened the path per batch would make a new
        // one here; a sink that holds the handle open keeps writing to the same inode.
        let moved = logs.join("app-1.jsonl.moved");
        std::fs::rename(sink.path_for(&app_id()), &moved).unwrap();
        sink.publish(vec![event(
            TenantLogBody::Data {
                stream: TenantLogStream::Stdout,
                text: "after".into(),
            },
            1,
        )])
        .await;

        let written = std::fs::read_to_string(&moved).unwrap();
        assert!(written.contains("before"), "{written}");
        assert!(written.contains("after"), "{written}");
        assert!(
            !sink.path_for(&app_id()).exists(),
            "a held-open handle is not a fresh open of the path"
        );
    }

    #[tokio::test]
    async fn what_a_sink_buffered_is_on_disk_once_it_is_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let logs = directory.path().join("logs");
        let sink = unbounded(logs.clone());
        sink.publish(vec![event(
            TenantLogBody::Data {
                stream: TenantLogStream::Stdout,
                text: "last words".into(),
            },
            0,
        )])
        .await;
        let path = sink.path_for(&app_id());
        drop(sink);
        assert!(std::fs::read_to_string(&path).unwrap().contains("last words"));
    }

    fn line(sequence: u64) -> TenantLogEvent {
        event(
            TenantLogBody::Data {
                stream: TenantLogStream::Stdout,
                text: format!("line {sequence:03}"),
            },
            sequence,
        )
    }

    fn line_bytes() -> u64 {
        serde_json::to_vec(&line(0)).unwrap().len() as u64 + 1
    }

    fn events_in(path: &Path) -> Vec<TenantLogEvent> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn lines_in(path: &Path) -> Vec<String> {
        events_in(path)
            .into_iter()
            .map(|event| match event.body {
                TenantLogBody::Data { text, .. } => text.strip_prefix("line ").unwrap().to_string(),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    fn files_in(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn the_file_is_cut_on_the_line_that_reaches_the_cap_and_the_next_line_starts_a_fresh_one() {
        let directory = tempfile::tempdir().unwrap();
        let logs = directory.path().join("logs");
        let sink = FileLogSink::new(logs.clone(), 3 * line_bytes());
        let current = sink.path_for(&app_id());
        let previous = sink.previous_path_for(&app_id());

        sink.publish(vec![line(0), line(1)]).await;
        assert_eq!(lines_in(&current), ["000", "001"]);
        assert!(!previous.exists());

        sink.publish(vec![line(2)]).await;
        assert_eq!(lines_in(&previous), ["000", "001", "002"]);
        assert!(!current.exists(), "nothing has been written since the cut");

        sink.publish(vec![line(3)]).await;
        assert_eq!(lines_in(&current), ["003"]);
        assert_eq!(lines_in(&previous), ["000", "001", "002"]);
    }

    #[tokio::test]
    async fn only_the_newest_two_files_are_kept_however_much_an_app_writes() {
        let directory = tempfile::tempdir().unwrap();
        let logs = directory.path().join("logs");
        let sink = FileLogSink::new(logs.clone(), 2 * line_bytes());
        for batch in (0..21).collect::<Vec<u64>>().chunks(3) {
            sink.publish(batch.iter().copied().map(line).collect()).await;
        }
        assert_eq!(files_in(&logs), ["app-1.jsonl", "app-1.jsonl.1"]);
        assert_eq!(lines_in(&sink.previous_path_for(&app_id())), ["018", "019"]);
        assert_eq!(lines_in(&sink.path_for(&app_id())), ["020"]);
    }

    #[tokio::test]
    async fn what_an_earlier_daemon_left_in_the_file_counts_towards_the_cap() {
        let directory = tempfile::tempdir().unwrap();
        let logs = directory.path().join("logs");
        let cap = 3 * line_bytes();
        FileLogSink::new(logs.clone(), cap)
            .publish(vec![line(0), line(1)])
            .await;

        let sink = FileLogSink::new(logs.clone(), cap);
        sink.publish(vec![line(2)]).await;
        assert_eq!(
            lines_in(&sink.previous_path_for(&app_id())),
            ["000", "001", "002"]
        );
    }

    #[tokio::test]
    async fn a_discarded_app_leaves_behind_neither_the_file_it_was_writing_nor_the_one_before_it() {
        let directory = tempfile::tempdir().unwrap();
        let logs = directory.path().join("logs");
        let sink = FileLogSink::new(logs.clone(), 2 * line_bytes());
        let neighbour = protocol::AppId::parse("app-2").unwrap();
        for sequence in 0..5 {
            sink.publish(vec![
                line(sequence),
                TenantLogEvent {
                    app_id: neighbour.clone(),
                    ..line(sequence)
                },
            ])
            .await;
        }
        assert_eq!(files_in(&logs).len(), 4);

        sink.discard(&app_id());

        assert_eq!(files_in(&logs), ["app-2.jsonl", "app-2.jsonl.1"]);
    }

    #[tokio::test]
    async fn an_app_deployed_again_under_a_discarded_name_writes_to_a_file_of_its_own() {
        let directory = tempfile::tempdir().unwrap();
        let logs = directory.path().join("logs");
        let sink = unbounded(logs.clone());
        sink.publish(vec![line(0)]).await;
        sink.discard(&app_id());

        sink.publish(vec![line(1)]).await;

        assert_eq!(lines_in(&sink.path_for(&app_id())), ["001"]);
        assert_eq!(files_in(&logs), ["app-1.jsonl"]);
    }

    #[tokio::test]
    async fn discarding_an_app_this_host_kept_no_output_for_is_not_a_failure() {
        let directory = tempfile::tempdir().unwrap();
        let sink = unbounded(directory.path().join("logs"));
        sink.discard(&app_id());
    }

    #[tokio::test]
    async fn a_file_moved_out_from_under_the_handle_is_not_the_sinks_to_keep_at_the_cut() {
        let directory = tempfile::tempdir().unwrap();
        let logs = directory.path().join("logs");
        let sink = FileLogSink::new(logs.clone(), 2 * line_bytes());
        sink.publish(vec![line(0)]).await;
        let moved = logs.join("app-1.jsonl.moved");
        std::fs::rename(sink.path_for(&app_id()), &moved).unwrap();

        // The held-open handle takes the line that reaches the cap; the cut then finds nothing at
        // the path, and the next line starts afresh there.
        sink.publish(vec![line(1)]).await;
        sink.publish(vec![line(2)]).await;
        assert_eq!(lines_in(&moved), ["000", "001"]);
        assert_eq!(lines_in(&sink.path_for(&app_id())), ["002"]);
        assert_eq!(files_in(&logs), ["app-1.jsonl", "app-1.jsonl.moved"]);
    }
}
