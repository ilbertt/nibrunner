//! The host's end of the guest's log vsock.
//!
//! Firecracker turns a guest-initiated connection on port 51000 into a connection to
//! `<uds_path>_51000` on the host, so this listens on that path for as long as the microVM has a
//! slot — through a sleep and a wake, because the guest reconnects on its own and a listener torn
//! down between them would lose the first thing it said.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use protocol::{AppId, DeploymentId, TenantLogStream};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::clock::now_timestamp;
use crate::services::{LogSink, TenantLogBody, TenantLogEvent};
use guest_contract::logs::{decode_frames, GuestLogFrame};

/// The peer is a kernel the tenant controls, and every accepted socket costs the host a decoder.
const MAX_GUEST_CONNECTIONS: usize = 4;
const PRIVATE_SOCKET_MODE: u32 = 0o600;

struct Attachment {
    source: Arc<Mutex<(AppId, DeploymentId)>>,
    socket_path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

/// Which apps this host is listening for, and on which path.
#[derive(Default)]
pub struct TenantLogReceiver {
    attachments: Mutex<BTreeMap<AppId, Attachment>>,
}

impl TenantLogReceiver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A listener for one app's guest. Re-attaching the same path only updates which deployment
    /// the output is stamped with, so a redeploy does not drop a connection the guest still holds.
    pub async fn attach(
        self: &Arc<Self>,
        app_id: AppId,
        deployment_id: DeploymentId,
        socket_path: PathBuf,
        sink: Arc<dyn LogSink>,
    ) -> std::io::Result<()> {
        let mut attachments = self.attachments.lock().await;
        if let Some(existing) = attachments.get(&app_id) {
            if existing.socket_path == socket_path {
                *existing.source.lock().await = (app_id, deployment_id);
                return Ok(());
            }
        }
        if let Some(previous) = attachments.remove(&app_id) {
            previous.task.abort();
            let _ = std::fs::remove_file(&previous.socket_path);
        }
        if let Some(parent) = socket_path.parent() {
            crate::json_store::make_directory(parent, 0o700)?;
        }
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(PRIVATE_SOCKET_MODE))?;
        }
        let source = Arc::new(Mutex::new((app_id.clone(), deployment_id)));
        let task = tokio::spawn(serve(listener, source.clone(), sink));
        attachments.insert(
            app_id,
            Attachment {
                source,
                socket_path,
                task,
            },
        );
        Ok(())
    }

    pub async fn detach(&self, app_id: &AppId) {
        let mut attachments = self.attachments.lock().await;
        if let Some(attachment) = attachments.remove(app_id) {
            attachment.task.abort();
            let _ = std::fs::remove_file(&attachment.socket_path);
        }
    }

    pub async fn attached(&self) -> Vec<AppId> {
        self.attachments.lock().await.keys().cloned().collect()
    }
}

async fn serve(listener: UnixListener, source: Arc<Mutex<(AppId, DeploymentId)>>, sink: Arc<dyn LogSink>) {
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_GUEST_CONNECTIONS));
    // One identity per listener rather than per connection: a gap in the sequence within it means
    // buffering dropped records, and a new one means this receiver restarted.
    let source_id = uuid::Uuid::new_v4().to_string();
    let sequence = Arc::new(std::sync::atomic::AtomicU64::new(0));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            // Over the cap: opened to be closed, because a decoder is what each one costs.
            drop(stream);
            continue;
        };
        let source = source.clone();
        let sink = sink.clone();
        let source_id = source_id.clone();
        let sequence = sequence.clone();
        tokio::spawn(async move {
            let _permit = permit;
            pump(stream, source, sink, source_id, sequence).await;
        });
    }
}

async fn pump(
    mut stream: UnixStream,
    source: Arc<Mutex<(AppId, DeploymentId)>>,
    sink: Arc<dyn LogSink>,
    source_id: String,
    sequence: Arc<std::sync::atomic::AtomicU64>,
) {
    let mut buffered: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    // One decoder per stream, held across chunks: a multi-byte character split across two frames
    // is half a glyph until the rest of it arrives.
    let mut carried: BTreeMap<&'static str, Vec<u8>> = BTreeMap::new();
    loop {
        let Ok(read) = stream.read(&mut chunk).await else {
            return;
        };
        if read == 0 {
            return;
        }
        let decoded = match decode_frames(&buffered, &chunk[..read]) {
            Ok(decoded) => decoded,
            Err(error) => {
                tracing::warn!(%error, "a guest sent log frames this host cannot read");
                return;
            }
        };
        buffered = decoded.1;
        let mut events = Vec::new();
        for frame in decoded.0 {
            let (app_id, deployment_id) = source.lock().await.clone();
            let body = match frame {
                GuestLogFrame::Gap { dropped_bytes } => Some(TenantLogBody::Gap { dropped_bytes }),
                GuestLogFrame::Data { stream, bytes } => {
                    let name = match stream {
                        TenantLogStream::Stdout => "stdout",
                        TenantLogStream::Stderr => "stderr",
                    };
                    let held = carried.entry(name).or_default();
                    held.extend_from_slice(&bytes);
                    let text = match std::str::from_utf8(held) {
                        Ok(text) => {
                            let text = text.to_string();
                            held.clear();
                            text
                        }
                        Err(error) => {
                            let good = error.valid_up_to();
                            let text = String::from_utf8_lossy(&held[..good]).into_owned();
                            held.drain(..good);
                            text
                        }
                    };
                    if text.is_empty() {
                        None
                    } else {
                        Some(TenantLogBody::Data { stream, text })
                    }
                }
            };
            if let Some(body) = body {
                events.push(TenantLogEvent {
                    app_id,
                    deployment_id,
                    source_id: source_id.clone(),
                    sequence: sequence.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                    observed_at: now_timestamp(),
                    body,
                });
            }
        }
        if !events.is_empty() {
            sink.publish(events).await;
        }
    }
}

/// Where Firecracker delivers a guest's own connection on the tenant log port.
pub fn tenant_log_socket_path(working_dir: &Path) -> PathBuf {
    working_dir.join(guest_contract::vsock::tenant_log_socket_name())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::RecordingLogSink;
    use crate::test_support::{app_id, deployment_id};
    use guest_contract::logs::{encode_frame, ENCODE_KIND_GAP, ENCODE_KIND_STDOUT};
    use tokio::io::AsyncWriteExt;

    async fn until(condition: impl Fn() -> bool) {
        for _ in 0..200 {
            if condition() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the receiver never saw what the guest wrote");
    }

    #[tokio::test]
    async fn what_a_guest_writes_arrives_as_events_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = tenant_log_socket_path(directory.path());
        let sink = RecordingLogSink::new();
        let receiver = TenantLogReceiver::new();
        receiver
            .attach(app_id(), deployment_id(), socket_path.clone(), sink.clone())
            .await
            .unwrap();

        let mut guest = UnixStream::connect(&socket_path).await.unwrap();
        // Split across two writes, which is what a transport does to a frame.
        let frame = encode_frame(ENCODE_KIND_STDOUT, "listening\n".as_bytes());
        guest.write_all(&frame[..5]).await.unwrap();
        guest.write_all(&frame[5..]).await.unwrap();
        guest
            .write_all(&encode_frame(ENCODE_KIND_GAP, &4096u64.to_be_bytes()))
            .await
            .unwrap();
        guest.flush().await.unwrap();

        until(|| sink.events().len() == 2).await;
        let events = sink.events();
        assert_eq!(events[0].app_id, app_id());
        assert_eq!(
            events[0].body,
            TenantLogBody::Data {
                stream: TenantLogStream::Stdout,
                text: "listening\n".into()
            }
        );
        assert_eq!(events[1].body, TenantLogBody::Gap { dropped_bytes: 4096 });
        // One source across the connection, and a sequence that says where a record sat in it.
        assert_eq!(events[0].source_id, events[1].source_id);
        assert_eq!((events[0].sequence, events[1].sequence), (0, 1));
    }

    /// A character split across two frames is held until the rest of it arrives: an event
    /// carrying half a glyph is one nobody can read.
    #[tokio::test]
    async fn a_character_split_across_frames_is_not_emitted_in_halves() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = tenant_log_socket_path(directory.path());
        let sink = RecordingLogSink::new();
        let receiver = TenantLogReceiver::new();
        receiver
            .attach(app_id(), deployment_id(), socket_path.clone(), sink.clone())
            .await
            .unwrap();

        let mut guest = UnixStream::connect(&socket_path).await.unwrap();
        let snowman = "☃".as_bytes();
        guest
            .write_all(&encode_frame(ENCODE_KIND_STDOUT, &snowman[..1]))
            .await
            .unwrap();
        guest
            .write_all(&encode_frame(ENCODE_KIND_STDOUT, &snowman[1..]))
            .await
            .unwrap();
        guest.flush().await.unwrap();

        until(|| !sink.events().is_empty()).await;
        assert_eq!(
            sink.events()[0].body,
            TenantLogBody::Data {
                stream: TenantLogStream::Stdout,
                text: "☃".into()
            }
        );
    }

    #[tokio::test]
    async fn attaching_the_same_path_again_only_restamps_the_deployment() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = tenant_log_socket_path(directory.path());
        let sink = RecordingLogSink::new();
        let receiver = TenantLogReceiver::new();
        receiver
            .attach(app_id(), deployment_id(), socket_path.clone(), sink.clone())
            .await
            .unwrap();
        let guest = UnixStream::connect(&socket_path).await.unwrap();
        let newer = DeploymentId::parse("dep-2").unwrap();
        receiver
            .attach(app_id(), newer.clone(), socket_path.clone(), sink.clone())
            .await
            .unwrap();
        // The guest's connection survived, which is what a redeploy of the daemon must not break.
        drop(guest);
        let mut second = UnixStream::connect(&socket_path).await.unwrap();
        second
            .write_all(&encode_frame(ENCODE_KIND_STDOUT, b"after\n"))
            .await
            .unwrap();
        second.flush().await.unwrap();
        until(|| !sink.events().is_empty()).await;
        assert_eq!(sink.events()[0].deployment_id, newer);

        receiver.detach(&app_id()).await;
        assert!(receiver.attached().await.is_empty());
        assert!(UnixStream::connect(&socket_path).await.is_err());
    }
}
