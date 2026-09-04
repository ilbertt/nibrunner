//! The per-VM API socket. The boot config is read once and never again, so everything after boot
//! — pausing, snapshotting, restoring — happens here.
//!
//! Shapes are Firecracker 1.16.1's, off the swagger that release ships. Two of them are worth
//! naming because a neighbouring version differs: `/vm` mounts `PATCH` and only `PATCH`, for
//! `Paused` and `Resumed` alike, and `sync_snapshot_files` does not exist here — 1.16.1 always
//! syncs. A body Firecracker does not recognise is rejected outright, so a field invented for a
//! later version fails the call rather than being ignored.

use std::path::{Path, PathBuf};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde::Serialize;

use crate::services::VmError;

const VM_PATH: &str = "/vm";
const SNAPSHOT_CREATE_PATH: &str = "/snapshot/create";
const SNAPSHOT_LOAD_PATH: &str = "/snapshot/load";

const MAX_DETAIL_LENGTH: usize = 200;

/// A 256 MiB guest measures ~1.7s to snapshot, and is paused for every millisecond of it.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Firecracker binds its API socket after `exec`, so the first call always races the bind, and
/// whatever is left of an interval when the socket appears is added to the wake. Measured against
/// the guest console, the socket is up ~3ms after the process starts and a 10ms grid spent ~8ms
/// of the wake waiting to ask again.
const BIND_POLL_INTERVAL: Duration = Duration::from_millis(2);
const BIND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize)]
struct VmState {
    state: &'static str,
}

#[derive(Serialize)]
struct CreateSnapshot<'a> {
    snapshot_path: &'a str,
    mem_file_path: &'a str,
    snapshot_type: &'static str,
}

#[derive(Serialize)]
struct MemBackend<'a> {
    backend_type: &'static str,
    backend_path: &'a str,
}

#[derive(Serialize)]
struct LoadSnapshot<'a> {
    snapshot_path: &'a str,
    mem_backend: MemBackend<'a>,
    /// Not a refinement. Firecracker emulates no RTC on x86_64, so kvmclock is the guest's only
    /// source of wall time, and the default of `false` resumes the clock where the snapshot left
    /// it — a guest that slept an hour wakes an hour in the past and its first outbound TLS
    /// handshake fails on a certificate that is not yet valid. A wrong answer from a
    /// healthy-looking app rather than a microVM that visibly did not come up.
    clock_realtime: bool,
}

pub struct FirecrackerApi {
    socket_path: PathBuf,
}

impl FirecrackerApi {
    pub fn at(socket_path: impl Into<PathBuf>) -> Self {
        Self { socket_path: socket_path.into() }
    }

    async fn call<B: Serialize>(&self, method: Method, path: &str, body: &B) -> Result<(), VmError> {
        let unreachable = |reason: String| VmError::Unreachable {
            socket_path: self.socket_path.display().to_string(),
            reason,
        };
        let rendered = serde_json::to_vec(body).map_err(|error| unreachable(error.to_string()))?;
        let stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|error| unreachable(error.to_string()))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| unreachable(error.to_string()))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = Request::builder()
            .method(method)
            .uri(path)
            // Firecracker's API is HTTP/1.1 over a unix socket, so the authority is a formality
            // and only the path is read.
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Full::new(bytes::Bytes::from(rendered)))
            .map_err(|error| unreachable(error.to_string()))?;
        let response = tokio::time::timeout(CALL_TIMEOUT, sender.send_request(request))
            .await
            .map_err(|_| unreachable(format!("{path} did not answer within {}s", CALL_TIMEOUT.as_secs())))?
            .map_err(|error| unreachable(error.to_string()))?;
        let status = response.status();
        if status == hyper::StatusCode::NO_CONTENT {
            return Ok(());
        }
        // Every refusal is a `fault_message`, and a body that is not one is still the only
        // account of itself the VMM gave.
        let detail = response
            .into_body()
            .collect()
            .await
            .map(|body| String::from_utf8_lossy(&body.to_bytes()).trim().to_string())
            .unwrap_or_default();
        Err(VmError::Rejected {
            path: path.to_string(),
            status: status.as_u16(),
            detail: protocol::truncate_chars(detail, MAX_DETAIL_LENGTH),
        })
    }

    /// A call to a Firecracker that has only just been started has to be prepared to find nothing
    /// listening yet. Only a connection that never completed is repeated: a refusal is the VMM's
    /// answer, and asking again does not change it.
    async fn until_bound<B: Serialize>(&self, method: Method, path: &str, body: &B) -> Result<(), VmError> {
        let deadline = std::time::Instant::now() + BIND_TIMEOUT;
        loop {
            match self.call(method.clone(), path, body).await {
                Err(VmError::Unreachable { .. }) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(BIND_POLL_INTERVAL).await;
                }
                outcome => return outcome,
            }
        }
    }

    pub async fn pause(&self) -> Result<(), VmError> {
        self.call(Method::PATCH, VM_PATH, &VmState { state: "Paused" }).await
    }

    pub async fn resume(&self) -> Result<(), VmError> {
        self.call(Method::PATCH, VM_PATH, &VmState { state: "Resumed" }).await
    }

    /// Both files are created or truncated by this, and the microVM has to be paused already.
    pub async fn create_snapshot(&self, state_path: &Path, memory_path: &Path) -> Result<(), VmError> {
        self.call(
            Method::PUT,
            SNAPSHOT_CREATE_PATH,
            &CreateSnapshot {
                snapshot_path: &state_path.display().to_string(),
                mem_file_path: &memory_path.display().to_string(),
                snapshot_type: "Full",
            },
        )
        .await
    }

    /// Accepted only by a Firecracker that has configured nothing, which is why a restore is
    /// started without a config file. It leaves the microVM paused; resuming it is a call of its
    /// own.
    ///
    /// `mem_backend` rather than the `mem_file_path` beside it: that spelling is deprecated
    /// upstream and the two are mutually exclusive, so sending both is a rejection rather than a
    /// preference.
    pub async fn load_snapshot(&self, state_path: &Path, memory_path: &Path) -> Result<(), VmError> {
        self.until_bound(
            Method::PUT,
            SNAPSHOT_LOAD_PATH,
            &LoadSnapshot {
                snapshot_path: &state_path.display().to_string(),
                mem_backend: MemBackend { backend_type: "File", backend_path: &memory_path.display().to_string() },
                clock_realtime: true,
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A listening socket speaking Firecracker's own shapes, because what is being checked is the
    /// bytes this end puts on the wire.
    struct FakeVmm {
        seen: Arc<Mutex<Vec<(String, String, String)>>>,
        status: hyper::StatusCode,
        body: &'static str,
    }

    impl FakeVmm {
        async fn listening(directory: &Path, status: hyper::StatusCode, body: &'static str) -> (PathBuf, Arc<Mutex<Vec<(String, String, String)>>>) {
            let socket_path = directory.join("firecracker.sock");
            let seen = Arc::new(Mutex::new(Vec::new()));
            let vmm = FakeVmm { seen: seen.clone(), status, body };
            let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let seen = vmm.seen.clone();
                    let status = vmm.status;
                    let body = vmm.body;
                    tokio::spawn(async move {
                        let service = hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                            let seen = seen.clone();
                            async move {
                                let method = request.method().to_string();
                                let path = request.uri().path().to_string();
                                let payload = request.into_body().collect().await.map(|body| String::from_utf8_lossy(&body.to_bytes()).into_owned()).unwrap_or_default();
                                seen.lock().unwrap().push((method, path, payload));
                                Ok::<_, std::convert::Infallible>(
                                    hyper::Response::builder()
                                        .status(status)
                                        .body(Full::new(bytes::Bytes::from(body)))
                                        .unwrap(),
                                )
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            });
            (socket_path, seen)
        }
    }

    #[tokio::test]
    async fn pausing_and_resuming_are_both_a_patch_on_the_vm_path() {
        let directory = tempfile::tempdir().unwrap();
        let (socket_path, seen) = FakeVmm::listening(directory.path(), hyper::StatusCode::NO_CONTENT, "").await;
        let api = FirecrackerApi::at(&socket_path);
        api.pause().await.unwrap();
        api.resume().await.unwrap();
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls[0], ("PATCH".into(), "/vm".into(), r#"{"state":"Paused"}"#.into()));
        assert_eq!(calls[1], ("PATCH".into(), "/vm".into(), r#"{"state":"Resumed"}"#.into()));
    }

    #[tokio::test]
    async fn a_snapshot_is_created_full_and_loaded_through_mem_backend_with_the_clock_advanced() {
        let directory = tempfile::tempdir().unwrap();
        let (socket_path, seen) = FakeVmm::listening(directory.path(), hyper::StatusCode::NO_CONTENT, "").await;
        let api = FirecrackerApi::at(&socket_path);
        api.create_snapshot(Path::new("/snap/vmstate"), Path::new("/snap/memory")).await.unwrap();
        api.load_snapshot(Path::new("/snap/vmstate"), Path::new("/snap/memory")).await.unwrap();
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls[0].0, "PUT");
        assert_eq!(calls[0].1, "/snapshot/create");
        assert_eq!(
            calls[0].2,
            r#"{"snapshot_path":"/snap/vmstate","mem_file_path":"/snap/memory","snapshot_type":"Full"}"#
        );
        assert_eq!(calls[1].1, "/snapshot/load");
        // `mem_file_path` and `mem_backend` are mutually exclusive upstream, so only one travels.
        assert!(!calls[1].2.contains("mem_file_path"));
        assert_eq!(
            calls[1].2,
            r#"{"snapshot_path":"/snap/vmstate","mem_backend":{"backend_type":"File","backend_path":"/snap/memory"},"clock_realtime":true}"#
        );
    }

    #[tokio::test]
    async fn a_refusal_carries_the_account_the_vmm_gave_of_itself() {
        let directory = tempfile::tempdir().unwrap();
        let (socket_path, _) = FakeVmm::listening(
            directory.path(),
            hyper::StatusCode::BAD_REQUEST,
            r#"{"fault_message":"Load snapshot error: Cannot restore"}"#,
        )
        .await;
        let error = FirecrackerApi::at(&socket_path).pause().await.unwrap_err();
        match error {
            VmError::Rejected { path, status, detail } => {
                assert_eq!(path, "/vm");
                assert_eq!(status, 400);
                assert!(detail.contains("Cannot restore"));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A socket nothing is listening on is the VMM not being there, which is a different thing
    /// from one that answered — and only the first is worth asking again.
    #[tokio::test]
    async fn a_socket_nothing_answers_is_unreachable_rather_than_a_refusal() {
        let directory = tempfile::tempdir().unwrap();
        let api = FirecrackerApi::at(directory.path().join("absent.sock"));
        assert!(matches!(api.pause().await, Err(VmError::Unreachable { .. })));
    }
}
