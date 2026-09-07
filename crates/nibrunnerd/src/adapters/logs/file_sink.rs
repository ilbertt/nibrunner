use std::io::Write;
use std::path::PathBuf;

use async_trait::async_trait;
use protocol::AppId;

use crate::json_store::make_directory;
use crate::ports::{LogSink, TenantLogBody, TenantLogEvent};

const LOG_DIR_MODE: u32 = 0o700;

const GAP_MESSAGE: &str = "tenant output dropped by host buffering";

pub struct FileLogSink {
    directory: PathBuf,
}

impl FileLogSink {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
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
        if let Err(error) = make_directory(&self.directory, LOG_DIR_MODE) {
            tracing::warn!(%error, "tenant logs have nowhere to go");
            return;
        }
        for event in events {
            let path = self.path_for(&event.app_id);
            let opened = std::fs::OpenOptions::new().create(true).append(true).open(&path);
            match opened {
                Ok(mut file) => {
                    if let Err(error) = file.write_all(Self::render(&event).as_bytes()) {
                        tracing::warn!(app_id = %event.app_id, %error, "tenant output could not be written");
                    }
                }
                Err(error) => {
                    tracing::warn!(app_id = %event.app_id, %error, "tenant output could not be written");
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
}
