use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use protocol::AppId;

use crate::json_store::make_directory;
use crate::ports::{LogSink, TenantLogBody, TenantLogEvent};

const LOG_DIR_MODE: u32 = 0o700;

const GAP_MESSAGE: &str = "tenant output dropped by host buffering";

pub struct FileLogSink {
    directory: PathBuf,
    // One handle held open per app: a busy tenant's output is no longer paid for with an
    // open()/close() around every line, which is what capped the drain. Writes are buffered and
    // flushed once the batch the receiver handed us has drained, so a daemon crash loses nothing
    // that a publish returned for.
    writers: Mutex<BTreeMap<AppId, BufWriter<File>>>,
}

impl FileLogSink {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            writers: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn path_for(&self, app_id: &AppId) -> PathBuf {
        self.directory.join(format!("{app_id}.log"))
    }

    fn render(event: &TenantLogEvent) -> String {
        match &event.body {
            TenantLogBody::Data { stream, text } => {
                format!(
                    "{} {} {}/{} {}",
                    event.observed_at,
                    stream.as_str(),
                    event.source_id,
                    event.sequence,
                    text
                )
            }
            TenantLogBody::Gap { dropped_bytes } => format!(
                "{} stderr {}/{} {GAP_MESSAGE}: {dropped_bytes} bytes\n",
                event.observed_at, event.source_id, event.sequence
            ),
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
            let rendered = Self::render(&event);
            if !writers.contains_key(&event.app_id) {
                if let Err(error) = make_directory(&self.directory, LOG_DIR_MODE) {
                    tracing::warn!(%error, "tenant logs have nowhere to go");
                    continue;
                }
                let path = self.path_for(&event.app_id);
                match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                    Ok(file) => {
                        writers.insert(event.app_id.clone(), BufWriter::new(file));
                    }
                    Err(error) => {
                        tracing::warn!(app_id = %event.app_id, %error, "tenant output could not be written");
                        continue;
                    }
                }
            }
            let Some(writer) = writers.get_mut(&event.app_id) else {
                continue;
            };
            match writer.write_all(rendered.as_bytes()) {
                Ok(()) => {
                    touched.insert(event.app_id);
                }
                Err(error) => {
                    tracing::warn!(app_id = %event.app_id, %error, "tenant output could not be written")
                }
            }
        }
        for app_id in touched {
            if let Some(writer) = writers.get_mut(&app_id) {
                if let Err(error) = writer.flush() {
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
            for (app_id, writer) in writers.iter_mut() {
                if let Err(error) = writer.flush() {
                    tracing::warn!(%app_id, %error, "tenant output could not be flushed as its sink was torn down");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{app_id, deployment_id, observed_at};
    use protocol::TenantLogStream;

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

    #[tokio::test]
    async fn output_lands_in_one_file_per_app_with_a_gap_in_its_place_in_the_stream() {
        let directory = tempfile::tempdir().unwrap();
        let sink = FileLogSink::new(directory.path().join("logs"));
        sink.publish(vec![
            event(
                TenantLogBody::Data {
                    stream: TenantLogStream::Stdout,
                    text: "listening\n".into(),
                },
                0,
            ),
            event(TenantLogBody::Gap { dropped_bytes: 4096 }, 1),
            event(
                TenantLogBody::Data {
                    stream: TenantLogStream::Stderr,
                    text: "warning\n".into(),
                },
                2,
            ),
        ])
        .await;
        let written = std::fs::read_to_string(sink.path_for(&app_id())).unwrap();
        let lines: Vec<&str> = written.lines().collect();
        assert!(lines[0].ends_with("stdout source-1/0 listening"));
        assert!(lines[1].contains("dropped by host buffering: 4096 bytes"));
        assert!(lines[2].ends_with("stderr source-1/2 warning"));
        sink.publish(vec![event(
            TenantLogBody::Data {
                stream: TenantLogStream::Stdout,
                text: "again\n".into(),
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
        FileLogSink::new(logs.clone()).publish(vec![]).await;
        assert!(!logs.exists());
    }

    #[tokio::test]
    async fn no_tenant_ever_reads_a_line_another_tenant_wrote() {
        let directory = tempfile::tempdir().unwrap();
        let sink = FileLogSink::new(directory.path().join("logs"));
        let neighbour = protocol::AppId::parse("app-2").unwrap();
        sink.publish(vec![
            event(
                TenantLogBody::Data {
                    stream: TenantLogStream::Stdout,
                    text: "mine\n".into(),
                },
                0,
            ),
            TenantLogEvent {
                app_id: neighbour.clone(),
                ..event(
                    TenantLogBody::Data {
                        stream: TenantLogStream::Stdout,
                        text: "theirs\n".into(),
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

    #[test]
    fn a_gap_is_rendered_on_stderr_so_it_is_read_where_a_failure_would_be() {
        let rendered = FileLogSink::render(&event(TenantLogBody::Gap { dropped_bytes: 0 }, 9));
        assert!(rendered.starts_with(observed_at().as_str()), "{rendered}");
        assert!(rendered.contains("stderr source-1/9"), "{rendered}");
        assert!(rendered.ends_with('\n'), "{rendered}");
    }

    #[test]
    fn what_a_tenant_wrote_is_passed_through_rather_than_reshaped() {
        let rendered = FileLogSink::render(&event(
            TenantLogBody::Data {
                stream: TenantLogStream::Stdout,
                text: "no trailing newline".into(),
            },
            0,
        ));
        assert!(rendered.ends_with("no trailing newline"), "{rendered}");
    }

    #[tokio::test]
    async fn a_flood_of_lines_across_many_batches_all_land_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let sink = FileLogSink::new(directory.path().join("logs"));
        let mut sequence = 0u64;
        for _ in 0..200 {
            let batch: Vec<TenantLogEvent> = (0..50)
                .map(|_| {
                    let at = sequence;
                    sequence += 1;
                    event(
                        TenantLogBody::Data {
                            stream: TenantLogStream::Stdout,
                            text: format!("line {at}\n"),
                        },
                        at,
                    )
                })
                .collect();
            sink.publish(batch).await;
        }
        let written = std::fs::read_to_string(sink.path_for(&app_id())).unwrap();
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines.len(), 10_000);
        assert!(lines[0].ends_with("line 0"), "{}", lines[0]);
        assert!(lines[9_999].ends_with("line 9999"), "{}", lines[9_999]);
    }

    #[tokio::test]
    async fn the_handle_is_kept_open_so_a_later_batch_does_not_reopen_the_path() {
        let directory = tempfile::tempdir().unwrap();
        let logs = directory.path().join("logs");
        let sink = FileLogSink::new(logs.clone());
        sink.publish(vec![event(
            TenantLogBody::Data {
                stream: TenantLogStream::Stdout,
                text: "before\n".into(),
            },
            0,
        )])
        .await;

        // Move the file out of the way. A sink that reopened the path per batch would make a new
        // one here; a sink that holds the handle open keeps writing to the same inode.
        let moved = logs.join("app-1.log.moved");
        std::fs::rename(sink.path_for(&app_id()), &moved).unwrap();
        sink.publish(vec![event(
            TenantLogBody::Data {
                stream: TenantLogStream::Stdout,
                text: "after\n".into(),
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
        let sink = FileLogSink::new(logs.clone());
        sink.publish(vec![event(
            TenantLogBody::Data {
                stream: TenantLogStream::Stdout,
                text: "last words\n".into(),
            },
            0,
        )])
        .await;
        let path = sink.path_for(&app_id());
        drop(sink);
        assert!(std::fs::read_to_string(&path).unwrap().contains("last words"));
    }
}
