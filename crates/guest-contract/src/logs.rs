//! Tenant stdout and stderr, as `apps/runtime/src/guest-logs.c` frames them: `NBL1`, a kind byte,
//! a big-endian u32 payload length, then the payload. Kind 3 is a gap carrying the bytes the guest
//! could not deliver as a big-endian u64.

use protocol::TenantLogStream;

pub const FRAME_MAGIC: &[u8; 4] = b"NBL1";
pub const FRAME_HEADER_BYTES: usize = 9;
const KIND_OFFSET: usize = 4;
const LENGTH_OFFSET: usize = 5;
pub const MAX_FRAME_PAYLOAD_BYTES: usize = 65_536;
const GAP_PAYLOAD_BYTES: usize = 8;

pub const KIND_STDOUT: u8 = 1;
pub const KIND_STDERR: u8 = 2;
pub const KIND_GAP: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestLogFrame {
    Data { stream: TenantLogStream, bytes: Vec<u8> },
    Gap { dropped_bytes: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the guest sent a log frame this host cannot read: {reason}")]
pub struct InvalidGuestLogFrame {
    pub reason: &'static str,
}

/// Frames survive arbitrary transport chunking: whatever is left over is returned as the buffer
/// to carry into the next call.
pub fn decode_frames(
    buffered: &[u8],
    chunk: &[u8],
) -> Result<(Vec<GuestLogFrame>, Vec<u8>), InvalidGuestLogFrame> {
    let mut rest = Vec::with_capacity(buffered.len() + chunk.len());
    rest.extend_from_slice(buffered);
    rest.extend_from_slice(chunk);
    let mut frames = Vec::new();
    let mut offset = 0;
    while rest.len() - offset >= FRAME_HEADER_BYTES {
        let header = &rest[offset..offset + FRAME_HEADER_BYTES];
        if &header[..4] != FRAME_MAGIC {
            return Err(InvalidGuestLogFrame {
                reason: "invalid magic value",
            });
        }
        let payload_length =
            u32::from_be_bytes([header[LENGTH_OFFSET], header[6], header[7], header[8]]) as usize;
        if payload_length > MAX_FRAME_PAYLOAD_BYTES {
            return Err(InvalidGuestLogFrame {
                reason: "payload exceeds the limit",
            });
        }
        let frame_length = FRAME_HEADER_BYTES + payload_length;
        if rest.len() - offset < frame_length {
            break;
        }
        let payload = &rest[offset + FRAME_HEADER_BYTES..offset + frame_length];
        frames.push(frame_from(header[KIND_OFFSET], payload)?);
        offset += frame_length;
    }
    rest.drain(..offset);
    Ok((frames, rest))
}

fn frame_from(kind: u8, payload: &[u8]) -> Result<GuestLogFrame, InvalidGuestLogFrame> {
    match kind {
        KIND_STDOUT => Ok(GuestLogFrame::Data {
            stream: TenantLogStream::Stdout,
            bytes: payload.to_vec(),
        }),
        KIND_STDERR => Ok(GuestLogFrame::Data {
            stream: TenantLogStream::Stderr,
            bytes: payload.to_vec(),
        }),
        KIND_GAP => {
            if payload.len() != GAP_PAYLOAD_BYTES {
                return Err(InvalidGuestLogFrame {
                    reason: "invalid gap payload length",
                });
            }
            let mut encoded = [0u8; 8];
            encoded.copy_from_slice(payload);
            Ok(GuestLogFrame::Gap {
                dropped_bytes: u64::from_be_bytes(encoded),
            })
        }
        _ => Err(InvalidGuestLogFrame {
            reason: "unknown frame kind",
        }),
    }
}

pub fn kind_of(stream: TenantLogStream) -> u8 {
    match stream {
        TenantLogStream::Stdout => KIND_STDOUT,
        TenantLogStream::Stderr => KIND_STDERR,
    }
}

/// What the guest sends when it had to drop a tenant's output: a count rather than the bytes,
/// because the bytes are gone and a buffer would have been guest memory the tenant was not given.
pub fn encode_gap(dropped_bytes: u64) -> Vec<u8> {
    encode_frame(KIND_GAP, &dropped_bytes.to_be_bytes())
}

/// The wire format restated rather than derived from the decoder, so the tests check the codec
/// against the bytes on the wire and not against themselves.
///
/// `kind` rather than a `TenantLogStream`, because a gap is a frame too and is not a stream: it is
/// the guest saying how much of a tenant's output it had to drop.
pub fn encode_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
    frame.extend_from_slice(FRAME_MAGIC);
    frame.push(kind);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

pub const ENCODE_KIND_STDOUT: u8 = KIND_STDOUT;
pub const ENCODE_KIND_STDERR: u8 = KIND_STDERR;
pub const ENCODE_KIND_GAP: u8 = KIND_GAP;

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly the bytes `send_frame` in guest-logs.c produces for a four-byte stdout payload.
    const STDOUT_FIXTURE: [u8; 13] = [b'N', b'B', b'L', b'1', 1, 0, 0, 0, 4, b'o', b'n', b'e', b'\n'];

    #[test]
    fn a_fixture_taken_from_the_c_framing_decodes() {
        let (frames, rest) = decode_frames(&[], &STDOUT_FIXTURE).unwrap();
        assert_eq!(
            frames,
            vec![GuestLogFrame::Data {
                stream: TenantLogStream::Stdout,
                bytes: b"one\n".to_vec()
            }]
        );
        assert!(rest.is_empty());
        assert_eq!(encode_frame(ENCODE_KIND_STDOUT, b"one\n"), STDOUT_FIXTURE);
    }

    #[test]
    fn arbitrary_transport_chunks_preserve_stdout_and_stderr_boundaries() {
        let mut bytes = encode_frame(ENCODE_KIND_STDOUT, b"one\n");
        bytes.extend(encode_frame(ENCODE_KIND_STDERR, b"two\n"));
        let (first, rest) = decode_frames(&[], &bytes[..7]).unwrap();
        assert!(first.is_empty());
        let (second, rest) = decode_frames(&rest, &bytes[7..]).unwrap();
        assert!(rest.is_empty());
        assert_eq!(
            second,
            vec![
                GuestLogFrame::Data {
                    stream: TenantLogStream::Stdout,
                    bytes: b"one\n".to_vec()
                },
                GuestLogFrame::Data {
                    stream: TenantLogStream::Stderr,
                    bytes: b"two\n".to_vec()
                },
            ]
        );
    }

    #[test]
    fn a_gap_carries_the_byte_count_the_guest_could_not_deliver() {
        let gap = encode_frame(ENCODE_KIND_GAP, &42u64.to_be_bytes());
        let (frames, _) = decode_frames(&[], &gap).unwrap();
        assert_eq!(frames, vec![GuestLogFrame::Gap { dropped_bytes: 42 }]);
    }

    #[test]
    fn an_invalid_peer_cannot_make_the_parser_allocate_an_unbounded_payload() {
        let mut frame = encode_frame(ENCODE_KIND_STDOUT, b"text");
        frame[5..9].copy_from_slice(&1_048_576u32.to_be_bytes());
        assert_eq!(
            decode_frames(&[], &frame).unwrap_err().reason,
            "payload exceeds the limit"
        );
        let mut bad_magic = encode_frame(ENCODE_KIND_STDOUT, b"text");
        bad_magic[0] = b'X';
        assert_eq!(
            decode_frames(&[], &bad_magic).unwrap_err().reason,
            "invalid magic value"
        );
    }
}
