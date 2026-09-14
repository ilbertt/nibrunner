use protocol::{StateMessage, TenantExit, TenantLogStream, TenantRestart};

pub const FRAME_MAGIC: &[u8; 4] = b"NBL1";
pub const FRAME_HEADER_BYTES: usize = 9;
const KIND_OFFSET: usize = 4;
const LENGTH_OFFSET: usize = 5;
pub const MAX_FRAME_PAYLOAD_BYTES: usize = 65_536;
const GAP_PAYLOAD_BYTES: usize = 8;

pub const KIND_STDOUT: u8 = 1;
pub const KIND_STDERR: u8 = 2;
pub const KIND_GAP: u8 = 3;
/// The guest's supervisor restarted the tenant. The payload is the restart's figures, big-endian,
/// with the sentence the guest printed for it after them: attempt `u32`, budget `u32`, how the
/// tenant ended as one byte of kind and an `i32`, the backoff in ms as a `u64`, then the reason.
pub const KIND_RESTART: u8 = 4;

const RESTART_HEADER_BYTES: usize = 21;
const EXIT_CODE: u8 = 0;
const EXIT_SIGNAL: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestLogFrame {
    Data { stream: TenantLogStream, bytes: Vec<u8> },
    Gap { dropped_bytes: u64 },
    Restart(TenantRestart),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the guest sent a log frame this host cannot read: {reason}")]
pub struct InvalidGuestLogFrame {
    pub reason: &'static str,
}

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
        KIND_RESTART => decode_restart(payload).map(GuestLogFrame::Restart),
        _ => Err(InvalidGuestLogFrame {
            reason: "unknown frame kind",
        }),
    }
}

fn decode_restart(payload: &[u8]) -> Result<TenantRestart, InvalidGuestLogFrame> {
    if payload.len() < RESTART_HEADER_BYTES {
        return Err(InvalidGuestLogFrame {
            reason: "invalid restart payload length",
        });
    }
    let u32_at = |offset: usize| {
        u32::from_be_bytes([
            payload[offset],
            payload[offset + 1],
            payload[offset + 2],
            payload[offset + 3],
        ])
    };
    let ended_with = u32_at(9) as i32;
    let exit = match payload[8] {
        EXIT_CODE => TenantExit::Code(ended_with),
        EXIT_SIGNAL => TenantExit::Signal(ended_with),
        _ => {
            return Err(InvalidGuestLogFrame {
                reason: "unknown tenant exit kind",
            })
        }
    };
    let mut backoff = [0u8; 8];
    backoff.copy_from_slice(&payload[13..RESTART_HEADER_BYTES]);
    let reason = std::str::from_utf8(&payload[RESTART_HEADER_BYTES..]).map_err(|_| InvalidGuestLogFrame {
        reason: "the restart reason is not text",
    })?;
    Ok(TenantRestart {
        attempt: u32_at(0),
        budget: u32_at(4),
        exit,
        reason: StateMessage::new(reason),
        backoff_ms: u64::from_be_bytes(backoff),
    })
}

pub fn kind_of(stream: TenantLogStream) -> u8 {
    match stream {
        TenantLogStream::Stdout => KIND_STDOUT,
        TenantLogStream::Stderr => KIND_STDERR,
    }
}

pub fn encode_gap(dropped_bytes: u64) -> Vec<u8> {
    encode_frame(KIND_GAP, &dropped_bytes.to_be_bytes())
}

pub fn encode_restart(restart: &TenantRestart) -> Vec<u8> {
    let (kind, ended_with) = match restart.exit {
        TenantExit::Code(code) => (EXIT_CODE, code),
        TenantExit::Signal(signal) => (EXIT_SIGNAL, signal),
    };
    let reason = restart.reason.as_str().as_bytes();
    let mut payload = Vec::with_capacity(RESTART_HEADER_BYTES + reason.len());
    payload.extend_from_slice(&restart.attempt.to_be_bytes());
    payload.extend_from_slice(&restart.budget.to_be_bytes());
    payload.push(kind);
    payload.extend_from_slice(&ended_with.to_be_bytes());
    payload.extend_from_slice(&restart.backoff_ms.to_be_bytes());
    payload.extend_from_slice(reason);
    encode_frame(KIND_RESTART, &payload)
}

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
pub const ENCODE_KIND_RESTART: u8 = KIND_RESTART;

#[cfg(test)]
mod tests {
    use super::*;

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

    fn oom_restart() -> TenantRestart {
        TenantRestart {
            attempt: 1,
            budget: 5,
            exit: TenantExit::Signal(9),
            reason: StateMessage::new(
                "the tenant exited (137): the kernel killed it for running out of memory at its ceiling of 198 MiB; restart 1 of 5 in 500ms",
            ),
            backoff_ms: 500,
        }
    }

    #[test]
    fn a_restart_round_trips_with_everything_the_guest_said_about_it() {
        let frame = encode_restart(&oom_restart());
        assert_eq!(&frame[..4], FRAME_MAGIC);
        assert_eq!(frame[KIND_OFFSET], ENCODE_KIND_RESTART);
        let (frames, rest) = decode_frames(&[], &frame).unwrap();
        assert_eq!(frames, vec![GuestLogFrame::Restart(oom_restart())]);
        assert!(rest.is_empty());

        let exited = TenantRestart {
            exit: TenantExit::Code(1),
            reason: StateMessage::new("the tenant exited (1); restart 2 of 5 in 1000ms"),
            ..oom_restart()
        };
        let (frames, _) = decode_frames(&[], &encode_restart(&exited)).unwrap();
        assert_eq!(frames, vec![GuestLogFrame::Restart(exited)]);
    }

    #[test]
    fn a_restart_payload_is_laid_out_the_way_the_header_says() {
        let frame = encode_restart(&oom_restart());
        let payload = &frame[FRAME_HEADER_BYTES..];
        assert_eq!(&payload[..4], &1u32.to_be_bytes(), "attempt");
        assert_eq!(&payload[4..8], &5u32.to_be_bytes(), "budget");
        assert_eq!(payload[8], EXIT_SIGNAL);
        assert_eq!(&payload[9..13], &9i32.to_be_bytes(), "signal");
        assert_eq!(&payload[13..21], &500u64.to_be_bytes(), "backoff");
        assert_eq!(
            std::str::from_utf8(&payload[21..]).unwrap(),
            oom_restart().reason.as_str()
        );
    }

    #[test]
    fn a_restart_this_host_cannot_read_is_refused_rather_than_guessed_at() {
        let short = encode_frame(ENCODE_KIND_RESTART, &[0u8; RESTART_HEADER_BYTES - 1]);
        assert_eq!(
            decode_frames(&[], &short).unwrap_err().reason,
            "invalid restart payload length"
        );
        let mut unknown_exit = encode_restart(&oom_restart());
        unknown_exit[FRAME_HEADER_BYTES + 8] = 7;
        assert_eq!(
            decode_frames(&[], &unknown_exit).unwrap_err().reason,
            "unknown tenant exit kind"
        );
        let mut not_text = [0u8; RESTART_HEADER_BYTES].to_vec();
        not_text.extend_from_slice(&[0xff, 0xfe]);
        assert_eq!(
            decode_frames(&[], &encode_frame(ENCODE_KIND_RESTART, &not_text))
                .unwrap_err()
                .reason,
            "the restart reason is not text"
        );
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
