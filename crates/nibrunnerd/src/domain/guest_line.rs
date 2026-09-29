use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::UnixStream;

// Every line a guest port answers with is a word or two: Firecracker's `OK <port>`, init's `OK`.
// The guest is not trusted to end one, and what a line it never ends costs the host is this much
// rather than whatever the guest can send before the timeout.
const MAX_LINE_BYTES: u64 = 256;

/// The next line the guest sent, without its ending; nothing if it hung up, stayed silent past
/// `timeout` or sent something that is not text.
pub async fn read(wire: &mut BufReader<UnixStream>, timeout: Duration) -> Option<String> {
    let mut line = String::new();
    let mut bounded = (&mut *wire).take(MAX_LINE_BYTES);
    match tokio::time::timeout(timeout, bounded.read_line(&mut line)).await {
        Ok(Ok(read)) if read > 0 => Some(line.trim_end().to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    const PATIENT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn a_line_the_guest_never_ends_is_read_no_further_than_the_limit() {
        let (host, mut guest) = UnixStream::pair().unwrap();
        tokio::spawn(async move {
            let endless = [b'x'; 4096];
            while guest.write_all(&endless).await.is_ok() {}
        });

        let line = read(&mut BufReader::new(host), PATIENT).await.unwrap();

        assert_eq!(line.len() as u64, MAX_LINE_BYTES);
    }

    #[tokio::test]
    async fn what_the_guest_sent_after_a_line_is_left_for_the_next_one() {
        let (host, mut guest) = UnixStream::pair().unwrap();
        guest.write_all(b"OK 1234\nOK\n").await.unwrap();
        let mut wire = BufReader::new(host);

        assert_eq!(read(&mut wire, PATIENT).await.as_deref(), Some("OK 1234"));
        assert_eq!(read(&mut wire, PATIENT).await.as_deref(), Some("OK"));
    }

    #[tokio::test]
    async fn a_guest_that_hung_up_has_no_line_to_read() {
        let (host, guest) = UnixStream::pair().unwrap();
        drop(guest);

        assert_eq!(read(&mut BufReader::new(host), PATIENT).await, None);
    }
}
