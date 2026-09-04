//! The host's end of the guest's filesystem port.
//!
//! Ported from `apps/agent/src/lib/filesystem/client.ts`. Every answer comes from a `readdir`, a
//! `pread` or a `pwrite` *inside* the microVM, against the filesystem the tenant has mounted — so
//! a listing is what is there rather than what had reached the block device by the last flush, and
//! a write is possible at all, which it never was from outside a mount the guest holds read-write.
//!
//! One connection serves as many requests as the caller makes: browsing is many small reads and an
//! upload is many more, and the guest holds nothing between them. The connection *is* the
//! resource, though — the guest gives a worker to whoever holds one and takes it back when the
//! socket closes, so a caller that leaks one costs the tenant a process until it times out.

use std::path::Path;
use std::time::Duration;

use guest_contract::filesystem::{
    decode_compute, decode_details, decode_header, decode_listing, decode_usage, decode_written,
    encode_request, fits_one_request, is_refusal, refusal_for, FilesystemDetails, GuestFilesystemRequest,
    MeasuredBytes, MeasuredCompute, FRAME_HEADER_BYTES, GUEST_FILESYSTEM_CHUNK_BYTES,
};
use protocol::{AppId, DirectoryListing, GuestPath};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// A listing is read while somebody waits for it, so this is bounded well below the export path's
/// hour.
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum GuestFilesystemError {
    #[error("no microVM is running on this host for {app_id}, so its files cannot be reached")]
    Unreachable { app_id: AppId },
    #[error("the guest running {app_id} took a request about its files and never answered")]
    Silent { app_id: AppId },
    /// Read as a sentence rather than as a code, because this is the half of a failure that
    /// reaches whoever asked. None of them names the path: what a tenant keeps in their own
    /// filesystem is theirs to know and not an operator's.
    #[error("the guest running {app_id} would not do that with its files: {refusal}")]
    Refused { app_id: AppId, refusal: &'static str },
    #[error("more was asked of the guest running {app_id} at once than one request carries")]
    TooLarge { app_id: AppId },
    #[error("the guest answered about its files with bytes this host cannot read: {reason}")]
    Malformed { reason: String },
}

impl GuestFilesystemError {
    pub fn message(&self) -> String {
        self.to_string()
    }
}

pub struct GuestFilesystem {
    app_id: AppId,
    wire: BufReader<UnixStream>,
}

impl GuestFilesystem {
    /// Dialled and connected through to the filesystem port. A guest that is not running has no
    /// socket to dial, which is the ordinary answer for an app that is asleep or stopped.
    pub async fn dial(app_id: &AppId, vsock_path: &Path) -> Result<Self, GuestFilesystemError> {
        let unreachable = || GuestFilesystemError::Unreachable {
            app_id: app_id.clone(),
        };
        let stream = UnixStream::connect(vsock_path).await.map_err(|_| unreachable())?;
        let mut client = Self {
            app_id: app_id.clone(),
            wire: BufReader::new(stream),
        };
        let port = guest_contract::vsock::GUEST_FILESYSTEM_VSOCK_PORT;
        client
            .send(guest_contract::vsock::connect_request(port).as_bytes())
            .await?;
        let reply = client.receive_line().await?;
        guest_contract::vsock::read_connect_reply(&reply, port).map_err(|_| unreachable())?;
        Ok(client)
    }

    pub async fn list(&mut self, path: &GuestPath) -> Result<DirectoryListing, GuestFilesystemError> {
        let body = self
            .exchange(&GuestFilesystemRequest::List { path: path.clone() })
            .await?;
        decode_listing(&body, path).map_err(|error| self.malformed(&error))
    }

    pub async fn stat(&mut self, path: &GuestPath) -> Result<FilesystemDetails, GuestFilesystemError> {
        let body = self
            .exchange(&GuestFilesystemRequest::Stat { path: path.clone() })
            .await?;
        decode_details(&body).map_err(|error| self.malformed(&error))
    }

    /// How full the volume is. No path, because the volume is one filesystem all the way down.
    pub async fn usage(&mut self) -> Result<MeasuredBytes, GuestFilesystemError> {
        let body = self.exchange(&GuestFilesystemRequest::Usage).await?;
        decode_usage(&body).map_err(|error| self.malformed(&error))
    }

    /// What the guest is spending. No path either: this one is not about the filesystem at all.
    pub async fn compute(&mut self) -> Result<MeasuredCompute, GuestFilesystemError> {
        let body = self.exchange(&GuestFilesystemRequest::Compute).await?;
        decode_compute(&body).map_err(|error| self.malformed(&error))
    }

    /// Short of `length` is the end of the file, which is how a reader in chunks learns to stop.
    pub async fn read(
        &mut self,
        path: &GuestPath,
        offset: u64,
        length: u32,
    ) -> Result<Vec<u8>, GuestFilesystemError> {
        self.exchange(&GuestFilesystemRequest::Read {
            path: path.clone(),
            offset,
            length: length.min(GUEST_FILESYSTEM_CHUNK_BYTES as u32),
        })
        .await
    }

    /// `truncate` cuts the file at `offset` first, so a replacement leaves none of the old tail.
    pub async fn write(
        &mut self,
        path: &GuestPath,
        offset: u64,
        content: Vec<u8>,
        truncate: bool,
    ) -> Result<u32, GuestFilesystemError> {
        let body = self
            .exchange(&GuestFilesystemRequest::Write {
                path: path.clone(),
                offset,
                content,
                truncate,
            })
            .await?;
        decode_written(&body).map_err(|error| self.malformed(&error))
    }

    pub async fn make_directory(&mut self, path: &GuestPath) -> Result<(), GuestFilesystemError> {
        self.exchange(&GuestFilesystemRequest::MakeDirectory { path: path.clone() })
            .await
            .map(|_| ())
    }

    /// One entry, never a tree: a directory that still holds something is refused.
    pub async fn remove(&mut self, path: &GuestPath) -> Result<(), GuestFilesystemError> {
        self.exchange(&GuestFilesystemRequest::Remove { path: path.clone() })
            .await
            .map(|_| ())
    }

    pub async fn move_entry(
        &mut self,
        path: &GuestPath,
        destination: &GuestPath,
    ) -> Result<(), GuestFilesystemError> {
        self.exchange(&GuestFilesystemRequest::Move {
            path: path.clone(),
            destination: destination.clone(),
        })
        .await
        .map(|_| ())
    }

    async fn exchange(&mut self, request: &GuestFilesystemRequest) -> Result<Vec<u8>, GuestFilesystemError> {
        if !fits_one_request(request) {
            return Err(GuestFilesystemError::TooLarge {
                app_id: self.app_id.clone(),
            });
        }
        self.send(&encode_request(request)).await?;
        let header = self.receive(FRAME_HEADER_BYTES).await?;
        let header = decode_header(&header).map_err(|error| self.malformed(&error))?;
        let body = if header.body_length == 0 {
            Vec::new()
        } else {
            self.receive(header.body_length).await?
        };
        if is_refusal(header.status) {
            return Err(GuestFilesystemError::Refused {
                app_id: self.app_id.clone(),
                refusal: refusal_for(header.status),
            });
        }
        Ok(body)
    }

    fn malformed(&self, error: &guest_contract::filesystem::MalformedGuestReply) -> GuestFilesystemError {
        GuestFilesystemError::Malformed {
            reason: error.to_string(),
        }
    }

    fn silent(&self) -> GuestFilesystemError {
        GuestFilesystemError::Silent {
            app_id: self.app_id.clone(),
        }
    }

    async fn send(&mut self, bytes: &[u8]) -> Result<(), GuestFilesystemError> {
        self.wire
            .get_mut()
            .write_all(bytes)
            .await
            .map_err(|_| self.silent())
    }

    async fn receive(&mut self, count: usize) -> Result<Vec<u8>, GuestFilesystemError> {
        let mut buffer = vec![0u8; count];
        match tokio::time::timeout(REPLY_TIMEOUT, self.wire.read_exact(&mut buffer)).await {
            Ok(Ok(_)) => Ok(buffer),
            _ => Err(self.silent()),
        }
    }

    async fn receive_line(&mut self) -> Result<String, GuestFilesystemError> {
        let mut line = String::new();
        match tokio::time::timeout(REPLY_TIMEOUT, self.wire.read_line(&mut line)).await {
            Ok(Ok(read)) if read > 0 => Ok(line.trim_end().to_string()),
            _ => Err(self.silent()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::app_id;
    use guest_contract::filesystem::FRAME_MAGIC;
    use protocol::FilesystemEntryKind;
    use tokio::net::UnixListener;

    /// A guest that answers the connect, then replies to each request with the bytes given. The
    /// same encoders the guest itself uses, which is the point of the shared crate: a listing this
    /// host decodes is one something encoded with the function it decodes against.
    async fn guest_answering(replies: Vec<(u8, Vec<u8>)>) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("guest.vsock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let mut wire = BufReader::new(stream);
            let mut connect = String::new();
            let _ = wire.read_line(&mut connect).await;
            let _ = wire.get_mut().write_all(b"OK 1234\n").await;
            for (status, body) in replies {
                let mut header = Vec::new();
                if wire.read_exact(&mut [0u8; FRAME_HEADER_BYTES]).await.is_err() {
                    return;
                }
                header.extend_from_slice(FRAME_MAGIC);
                header.push(status);
                header.extend_from_slice(&(body.len() as u32).to_be_bytes());
                let _ = wire.get_mut().write_all(&header).await;
                let _ = wire.get_mut().write_all(&body).await;
            }
            std::future::pending::<()>().await;
        });
        (directory, path)
    }

    fn usage_body(total: u64, used: u64) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&total.to_be_bytes());
        body.extend_from_slice(&used.to_be_bytes());
        body
    }

    fn listing_body(entries: &[(&str, u8, u64, i64)], truncated: bool) -> Vec<u8> {
        let mut body = vec![u8::from(truncated)];
        for (name, kind, size, modified) in entries {
            body.push(*kind);
            body.extend_from_slice(&size.to_be_bytes());
            body.extend_from_slice(&modified.to_be_bytes());
            body.push(name.len() as u8);
            body.extend_from_slice(name.as_bytes());
        }
        body
    }

    #[tokio::test]
    async fn a_guest_that_is_not_running_cannot_be_browsed() {
        let directory = tempfile::tempdir().unwrap();
        let Err(error) = GuestFilesystem::dial(&app_id(), &directory.path().join("nothing.vsock")).await
        else {
            panic!("a guest that is not there is not browsable");
        };
        assert!(
            matches!(error, GuestFilesystemError::Unreachable { .. }),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_name_a_tokeniser_would_have_choked_on_comes_back_whole() {
        let awkward = "it's a \"file\"\nreally";
        let body = listing_body(&[(awkward, 1, 42, 1_760_000_000)], false);
        let (_directory, path) = guest_answering(vec![(0, body)]).await;

        let mut client = GuestFilesystem::dial(&app_id(), &path).await.unwrap();
        let listing = client.list(&GuestPath::parse("/").unwrap()).await.unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, awkward);
        assert_eq!(listing.entries[0].kind, FilesystemEntryKind::File);
        assert_eq!(listing.entries[0].size_bytes, 42);
        assert!(!listing.truncated);
    }

    #[tokio::test]
    async fn what_mke2fs_left_at_the_root_is_not_a_tenants_to_see() {
        let entries = [("lost+found", 2u8, 0u64, 0i64), ("notes.txt", 1, 10, 0)];
        let (_directory, path) = guest_answering(vec![
            (0, listing_body(&entries, false)),
            (0, listing_body(&entries, false)),
        ])
        .await;

        let mut client = GuestFilesystem::dial(&app_id(), &path).await.unwrap();
        let root = client.list(&GuestPath::parse("/").unwrap()).await.unwrap();
        assert_eq!(
            root.entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["notes.txt"]
        );
        let deeper = client.list(&GuestPath::parse("/data").unwrap()).await.unwrap();
        assert_eq!(
            deeper.entries.len(),
            2,
            "a tenant's own directory of that name is theirs"
        );
    }

    #[tokio::test]
    async fn a_refusal_reads_as_a_sentence_and_never_names_the_path() {
        let (_directory, path) = guest_answering(vec![(1, Vec::new())]).await;
        let mut client = GuestFilesystem::dial(&app_id(), &path).await.unwrap();
        let Err(error) = client.list(&GuestPath::parse("/secrets").unwrap()).await else {
            panic!("a refusal is not a listing");
        };
        assert!(
            error.message().contains("there is nothing at that path"),
            "{error}"
        );
        assert!(!error.message().contains("/secrets"), "{error}");
    }

    #[tokio::test]
    async fn more_than_one_frame_carries_is_refused_before_the_connection_pays_for_it() {
        let (_directory, path) = guest_answering(vec![(0, usage_body(1_000, 400))]).await;
        let mut client = GuestFilesystem::dial(&app_id(), &path).await.unwrap();
        let Err(error) = client
            .write(
                &GuestPath::parse("/big").unwrap(),
                0,
                vec![0u8; guest_contract::filesystem::BODY_MAX_BYTES + 1],
                false,
            )
            .await
        else {
            panic!("a body past the ceiling is not sent");
        };
        assert!(matches!(error, GuestFilesystemError::TooLarge { .. }), "{error}");

        // The connection is still good, which is the whole point of asking first.
        assert!(client.usage().await.is_ok());
    }

    #[tokio::test]
    async fn one_connection_serves_as_many_requests_as_the_caller_makes() {
        let (_directory, path) = guest_answering(vec![
            (0, listing_body(&[("a", 1, 1, 0)], false)),
            (0, listing_body(&[("b", 1, 2, 0)], false)),
            (0, usage_body(1_000, 400)),
        ])
        .await;

        let mut client = GuestFilesystem::dial(&app_id(), &path).await.unwrap();
        assert_eq!(
            client
                .list(&GuestPath::parse("/one").unwrap())
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
        assert_eq!(
            client
                .list(&GuestPath::parse("/two").unwrap())
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
        let measured = client.usage().await.unwrap();
        assert_eq!(measured.total_bytes, 1_000);
        assert_eq!(measured.used_bytes, 400);
    }
}
