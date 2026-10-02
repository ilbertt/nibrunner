use std::fmt;

pub const HEADER_BYTES: usize = 9;
pub const ACK: u8 = 0x06;
pub const MAX_REQUEST_BYTES: usize =
    protocol::MAX_CRONTAB_BYTES + (protocol::MAX_CRON_ENVIRONMENT_VARIABLES + 2) * 4;
pub const MAX_OUTPUT_BYTES: usize = 4096;

const MAGIC: &[u8; 4] = b"NBR1";
const MAX_EXIT_CODE: u32 = 255;
const MAX_SIGNAL: u32 = 64;

#[derive(Clone, PartialEq, Eq)]
pub struct ExecutionRequest {
    pub command: String,
    pub environment: Vec<String>,
}

impl fmt::Debug for ExecutionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExecutionRequest([REDACTED])")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    Malformed = 1,
    SpawnFailed = 2,
    Busy = 3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionFrame {
    Started,
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit { code: u32, signal: u32 },
    Rejected(Rejection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub code: u8,
    pub body_length: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the cron execution frame is malformed")]
pub struct MalformedExecution;

fn decode_header(bytes: &[u8]) -> Result<FrameHeader, MalformedExecution> {
    if bytes.len() != HEADER_BYTES || &bytes[..4] != MAGIC {
        return Err(MalformedExecution);
    }
    let length = u32::from_be_bytes(bytes[5..9].try_into().map_err(|_| MalformedExecution)?);
    Ok(FrameHeader {
        code: bytes[4],
        body_length: length as usize,
    })
}

pub fn decode_request_header(bytes: &[u8]) -> Result<FrameHeader, MalformedExecution> {
    let header = decode_header(bytes)?;
    if header.code != 0 || header.body_length > MAX_REQUEST_BYTES {
        return Err(MalformedExecution);
    }
    Ok(header)
}

pub fn decode_reply_header(bytes: &[u8]) -> Result<FrameHeader, MalformedExecution> {
    let header = decode_header(bytes)?;
    let valid = match header.code {
        1 => header.body_length == 0,
        2 | 3 => (1..=MAX_OUTPUT_BYTES).contains(&header.body_length),
        4 => header.body_length == 8,
        5 => header.body_length == 1,
        _ => false,
    };
    valid.then_some(header).ok_or(MalformedExecution)
}

fn frame(code: u8, body: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_BYTES + body.len());
    bytes.extend_from_slice(MAGIC);
    bytes.push(code);
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

fn environment_entry(entry: &str) -> bool {
    let Some((name, _)) = entry.split_once('=') else {
        return false;
    };
    !entry.contains('\0')
        && !name.is_empty()
        && name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic() || byte == b'_' || (index > 0 && byte.is_ascii_digit())
        })
}

fn valid_request(request: &ExecutionRequest) -> bool {
    !request.command.is_empty()
        && request.command.len() <= protocol::MAX_CRON_COMMAND_LENGTH
        && !request.command.contains('\0')
        && request.environment.len() <= protocol::MAX_CRON_ENVIRONMENT_VARIABLES
        && request.environment.iter().all(|entry| environment_entry(entry))
}

pub fn encode_request(request: &ExecutionRequest) -> Result<Vec<u8>, MalformedExecution> {
    if !valid_request(request) {
        return Err(MalformedExecution);
    }
    let length = 8
        + request.command.len()
        + request
            .environment
            .iter()
            .map(|entry| 4 + entry.len())
            .sum::<usize>();
    if length > MAX_REQUEST_BYTES {
        return Err(MalformedExecution);
    }
    let mut body = Vec::with_capacity(length);
    body.extend_from_slice(&(request.command.len() as u32).to_be_bytes());
    body.extend_from_slice(request.command.as_bytes());
    body.extend_from_slice(&(request.environment.len() as u32).to_be_bytes());
    for entry in &request.environment {
        body.extend_from_slice(&(entry.len() as u32).to_be_bytes());
        body.extend_from_slice(entry.as_bytes());
    }
    Ok(frame(0, &body))
}

fn take_length(bytes: &mut &[u8]) -> Result<usize, MalformedExecution> {
    if bytes.len() < 4 {
        return Err(MalformedExecution);
    }
    let length = u32::from_be_bytes(bytes[..4].try_into().map_err(|_| MalformedExecution)?);
    *bytes = &bytes[4..];
    Ok(length as usize)
}

fn take_string(bytes: &mut &[u8]) -> Result<String, MalformedExecution> {
    let length = take_length(bytes)?;
    if length > bytes.len() {
        return Err(MalformedExecution);
    }
    let value = std::str::from_utf8(&bytes[..length])
        .map_err(|_| MalformedExecution)?
        .to_string();
    *bytes = &bytes[length..];
    Ok(value)
}

pub fn decode_request(header: FrameHeader, body: &[u8]) -> Result<ExecutionRequest, MalformedExecution> {
    if header.code != 0 || header.body_length > MAX_REQUEST_BYTES || body.len() != header.body_length {
        return Err(MalformedExecution);
    }
    let mut rest = body;
    let command = take_string(&mut rest)?;
    let count = take_length(&mut rest)?;
    if count > protocol::MAX_CRON_ENVIRONMENT_VARIABLES {
        return Err(MalformedExecution);
    }
    let mut environment = Vec::with_capacity(count);
    for _ in 0..count {
        environment.push(take_string(&mut rest)?);
    }
    let request = ExecutionRequest { command, environment };
    if !rest.is_empty() || !valid_request(&request) {
        return Err(MalformedExecution);
    }
    Ok(request)
}

pub fn encode_reply(reply: &ExecutionFrame) -> Result<Vec<u8>, MalformedExecution> {
    if matches!(reply, ExecutionFrame::Stdout(bytes) | ExecutionFrame::Stderr(bytes) if !(1..=MAX_OUTPUT_BYTES).contains(&bytes.len()))
        || matches!(reply, ExecutionFrame::Exit {code,signal} if *code > MAX_EXIT_CODE || *signal > MAX_SIGNAL || (*code != 0 && *signal != 0))
    {
        return Err(MalformedExecution);
    }
    let bytes = match reply {
        ExecutionFrame::Started => frame(1, &[]),
        ExecutionFrame::Stdout(bytes) => frame(2, bytes),
        ExecutionFrame::Stderr(bytes) => frame(3, bytes),
        ExecutionFrame::Exit { code, signal } => {
            let body = [code.to_be_bytes(), signal.to_be_bytes()].concat();
            frame(4, &body)
        }
        ExecutionFrame::Rejected(reason) => frame(5, &[*reason as u8]),
    };
    decode_reply_header(&bytes[..HEADER_BYTES])?;
    Ok(bytes)
}

pub fn decode_reply(header: FrameHeader, body: &[u8]) -> Result<ExecutionFrame, MalformedExecution> {
    if body.len() != header.body_length {
        return Err(MalformedExecution);
    }
    match (header.code, body) {
        (1, []) => Ok(ExecutionFrame::Started),
        (2 | 3, bytes) if (1..=MAX_OUTPUT_BYTES).contains(&bytes.len()) => Ok(if header.code == 2 {
            ExecutionFrame::Stdout(bytes.to_vec())
        } else {
            ExecutionFrame::Stderr(bytes.to_vec())
        }),
        (4, bytes) if bytes.len() == 8 => {
            let code = u32::from_be_bytes(bytes[..4].try_into().map_err(|_| MalformedExecution)?);
            let signal = u32::from_be_bytes(bytes[4..].try_into().map_err(|_| MalformedExecution)?);
            if code > MAX_EXIT_CODE || signal > MAX_SIGNAL || (signal != 0 && code != 0) {
                return Err(MalformedExecution);
            }
            Ok(ExecutionFrame::Exit { code, signal })
        }
        (5, [reason]) => Ok(ExecutionFrame::Rejected(match reason {
            1 => Rejection::Malformed,
            2 => Rejection::SpawnFailed,
            3 => Rejection::Busy,
            _ => return Err(MalformedExecution),
        })),
        _ => Err(MalformedExecution),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_matches_the_c_protocol_byte_fixture() {
        let request = ExecutionRequest {
            command: "echo hi".into(),
            environment: vec!["A=b".into()],
        };
        let expected = b"NBR1\x00\x00\x00\x00\x16\x00\x00\x00\x07echo hi\x00\x00\x00\x01\x00\x00\x00\x03A=b";
        assert_eq!(encode_request(&request).unwrap(), expected);
        let header = decode_request_header(&expected[..HEADER_BYTES]).unwrap();
        assert_eq!(
            decode_request(header, &expected[HEADER_BYTES..]).unwrap(),
            request
        );
        assert!(!format!("{request:?}").contains("echo hi"));
    }

    #[test]
    fn exit_frames_accept_only_linux_exit_codes_and_signals() {
        for (code, signal) in [(256u32, 0u32), (0, 65), (1, 9), (u32::MAX, 0), (0, u32::MAX)] {
            let body = [code.to_be_bytes(), signal.to_be_bytes()].concat();
            assert!(decode_reply(
                FrameHeader {
                    code: 4,
                    body_length: 8
                },
                &body
            )
            .is_err());
            assert!(encode_reply(&ExecutionFrame::Exit { code, signal }).is_err());
        }
        for (code, signal) in [(255, 0), (0, 64)] {
            let reply = ExecutionFrame::Exit { code, signal };
            let frame = encode_reply(&reply).unwrap();
            let header = decode_reply_header(&frame[..HEADER_BYTES]).unwrap();
            assert_eq!(decode_reply(header, &frame[HEADER_BYTES..]).unwrap(), reply);
        }
    }

    #[test]
    fn invalid_requests_never_allocate_an_unbounded_body() {
        let mut header = *b"NBR1\x00\xff\xff\xff\xff";
        assert!(decode_request_header(&header).is_err());
        header[4] = 1;
        assert!(decode_request_header(&header).is_err());
        for entry in ["1NAME=x", "=x", "NAME", "A\0=x"] {
            assert!(encode_request(&ExecutionRequest {
                command: "true".into(),
                environment: vec![entry.into()]
            })
            .is_err());
        }
        let request = ExecutionRequest {
            command: "true".into(),
            environment: vec!["A=x".into(); 257],
        };
        assert!(encode_request(&request).is_err());
    }

    #[test]
    fn execution_limits_count_utf8_bytes_and_accept_the_maximum_environment() {
        let mut request = ExecutionRequest {
            command: "é".repeat(protocol::MAX_CRON_COMMAND_LENGTH / 2),
            environment: (0..protocol::MAX_CRON_ENVIRONMENT_VARIABLES)
                .map(|index| format!("ENTRY_{index}=value_{index}"))
                .collect(),
        };
        let bytes = encode_request(&request).unwrap();
        let header = decode_request_header(&bytes[..HEADER_BYTES]).unwrap();
        assert_eq!(decode_request(header, &bytes[HEADER_BYTES..]).unwrap(), request);
        request.command.push('é');
        assert!(encode_request(&request).is_err());
        request.command = "true".into();
        request.environment = vec![format!("A={}", "x".repeat(MAX_REQUEST_BYTES - 18))];
        assert_eq!(
            encode_request(&request).unwrap().len(),
            HEADER_BYTES + MAX_REQUEST_BYTES
        );
        request.environment[0].push('x');
        assert!(encode_request(&request).is_err());
    }

    #[test]
    fn output_and_exit_frames_round_trip_without_text_assumptions() {
        for reply in [
            ExecutionFrame::Started,
            ExecutionFrame::Stdout(vec![0, 255]),
            ExecutionFrame::Stderr(b"oops".to_vec()),
            ExecutionFrame::Exit { code: 7, signal: 0 },
            ExecutionFrame::Exit { code: 0, signal: 9 },
            ExecutionFrame::Rejected(Rejection::Busy),
        ] {
            let bytes = encode_reply(&reply).unwrap();
            let header = decode_reply_header(&bytes[..HEADER_BYTES]).unwrap();
            assert_eq!(decode_reply(header, &bytes[HEADER_BYTES..]).unwrap(), reply);
        }
        assert!(encode_reply(&ExecutionFrame::Stdout(vec![0; MAX_OUTPUT_BYTES + 1])).is_err());
        assert!(decode_reply(
            FrameHeader {
                code: 4,
                body_length: 8
            },
            &[0, 0, 0, 1, 0, 0, 0, 9]
        )
        .is_err());
    }
}
