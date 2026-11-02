use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::client::conn::http1::SendRequest;
use hyper::{Method, Request};
use protocol::{GuestPath, HranaError, HranaPipelineRequest, HranaPipelineResponse, SQLITE_MAX_BODY_BYTES};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HANDSHAKE_BYTES: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum GuestSqliteError {
    #[error("the guest database service is unavailable")]
    Unavailable,
    #[error("the guest database service answered with an invalid response")]
    Malformed,
    #[error("{message}")]
    Refused { code: String, message: String },
}

pub struct GuestSqlite {
    sender: SendRequest<Full<Bytes>>,
    connection: tokio::task::JoinHandle<()>,
    prefix: String,
}

impl Drop for GuestSqlite {
    fn drop(&mut self) {
        self.connection.abort();
    }
}

impl GuestSqlite {
    pub async fn connect(
        socket: &Path,
        root: &GuestPath,
        path: &GuestPath,
    ) -> Result<Self, GuestSqliteError> {
        tokio::time::timeout(RESPONSE_TIMEOUT, async {
            let mut stream = crate::unix_socket::connect(socket)
                .await
                .map_err(|_| GuestSqliteError::Unavailable)?;
            stream
                .write_all(
                    guest_contract::vsock::connect_request(guest_contract::vsock::GUEST_SQLITE_VSOCK_PORT)
                        .as_bytes(),
                )
                .await
                .map_err(|_| GuestSqliteError::Unavailable)?;
            let mut reply = Vec::new();
            loop {
                let byte = stream
                    .read_u8()
                    .await
                    .map_err(|_| GuestSqliteError::Unavailable)?;
                if byte == b'\n' {
                    break;
                }
                if reply.len() >= MAX_HANDSHAKE_BYTES {
                    return Err(GuestSqliteError::Malformed);
                }
                reply.push(byte);
            }
            let reply = std::str::from_utf8(&reply).map_err(|_| GuestSqliteError::Malformed)?;
            guest_contract::vsock::read_connect_reply(reply, guest_contract::vsock::GUEST_SQLITE_VSOCK_PORT)
                .map_err(|_| GuestSqliteError::Unavailable)?;
            let (sender, connection) =
                hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                    .await
                    .map_err(|_| GuestSqliteError::Unavailable)?;
            let connection = tokio::spawn(async move {
                let _ = connection.await;
            });
            let mut client = Self {
                sender,
                connection,
                prefix: guest_contract::sqlite::database_route(root, path),
            };
            client
                .exchange(Method::GET, client.prefix.clone(), Vec::new())
                .await?;
            Ok(client)
        })
        .await
        .map_err(|_| GuestSqliteError::Unavailable)?
    }

    async fn exchange(
        &mut self,
        method: Method,
        path: String,
        body: Vec<u8>,
    ) -> Result<Bytes, GuestSqliteError> {
        if body.len() > SQLITE_MAX_BODY_BYTES {
            return Err(GuestSqliteError::Malformed);
        }
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("content-length", body.len())
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| GuestSqliteError::Malformed)?;
        let response = self
            .sender
            .send_request(request)
            .await
            .map_err(|_| GuestSqliteError::Unavailable)?;
        let status = response.status();
        let body = Limited::new(response.into_body(), SQLITE_MAX_BODY_BYTES)
            .collect()
            .await
            .map_err(|_| GuestSqliteError::Malformed)?
            .to_bytes();
        if !status.is_success() {
            let error: HranaError = serde_json::from_slice(&body).map_err(|_| GuestSqliteError::Malformed)?;
            let code = error
                .code
                .filter(|value| value.len() <= 128)
                .ok_or(GuestSqliteError::Malformed)?;
            let mut message = error.message;
            if message.len() > protocol::SQLITE_MAX_ERROR_BYTES {
                let mut end = protocol::SQLITE_MAX_ERROR_BYTES;
                while !message.is_char_boundary(end) {
                    end -= 1;
                }
                message.truncate(end);
            }
            return Err(GuestSqliteError::Refused { code, message });
        }
        Ok(body)
    }

    pub async fn pipeline(
        &mut self,
        request: &HranaPipelineRequest,
    ) -> Result<HranaPipelineResponse, GuestSqliteError> {
        tokio::time::timeout(RESPONSE_TIMEOUT, async {
            let encoded = serde_json::to_vec(request).map_err(|_| GuestSqliteError::Malformed)?;
            let bytes = self
                .exchange(
                    Method::POST,
                    format!("{}{}", self.prefix, guest_contract::sqlite::PIPELINE_SUFFIX),
                    encoded,
                )
                .await?;
            let result: HranaPipelineResponse =
                serde_json::from_slice(&bytes).map_err(|_| GuestSqliteError::Malformed)?;
            if result.base_url.is_some() || result.results.len() != request.requests.len() {
                return Err(GuestSqliteError::Malformed);
            }
            Ok(result)
        })
        .await
        .map_err(|_| GuestSqliteError::Unavailable)?
    }
}
