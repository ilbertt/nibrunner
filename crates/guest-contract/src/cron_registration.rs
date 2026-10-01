use protocol::{Crontab, MAX_CRONTAB_BYTES, REDACTED};

pub const PORT: u32 = 51003;
pub const HEADER_BYTES: usize = 9;
pub const STATUS_OK: u8 = 0;
pub const STATUS_REJECTED: u8 = 1;
const MAGIC: &[u8; 4] = b"NBC1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationRequest {
    Replace(Crontab),
    List,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestHeader {
    pub verb: u8,
    pub body_length: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyHeader {
    pub status: u8,
    pub body_length: usize,
}

#[derive(Clone, PartialEq, Eq)]
pub struct RegistrationReply {
    pub status: u8,
    pub text: String,
}

impl std::fmt::Debug for RegistrationReply {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegistrationReply")
            .field("status", &self.status)
            .field("text", &REDACTED)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the guest sent a malformed cron registration frame")]
pub struct MalformedRegistration;

fn decode_header(bytes: &[u8]) -> Result<(u8, usize), MalformedRegistration> {
    if bytes.len() != HEADER_BYTES || &bytes[..4] != MAGIC {
        return Err(MalformedRegistration);
    }
    let length = u32::from_be_bytes(bytes[5..9].try_into().map_err(|_| MalformedRegistration)?) as usize;
    if length > MAX_CRONTAB_BYTES {
        return Err(MalformedRegistration);
    }
    Ok((bytes[4], length))
}

fn encode(code: u8, text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_BYTES + text.len());
    bytes.extend_from_slice(MAGIC);
    bytes.push(code);
    bytes.extend_from_slice(&(text.len() as u32).to_be_bytes());
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

pub fn encode_request(request: &RegistrationRequest) -> Vec<u8> {
    match request {
        RegistrationRequest::Replace(text) => encode(1, text.expose()),
        RegistrationRequest::List => encode(2, ""),
    }
}

pub fn decode_request_header(bytes: &[u8]) -> Result<RequestHeader, MalformedRegistration> {
    let (verb, body_length) = decode_header(bytes)?;
    if !matches!(verb, 1 | 2) || (verb == 2 && body_length != 0) {
        return Err(MalformedRegistration);
    }
    Ok(RequestHeader { verb, body_length })
}

pub fn decode_request(
    header: RequestHeader,
    body: &[u8],
) -> Result<RegistrationRequest, MalformedRegistration> {
    if body.len() != header.body_length || body.len() > MAX_CRONTAB_BYTES {
        return Err(MalformedRegistration);
    }
    match header.verb {
        1 => {
            let text = std::str::from_utf8(body).map_err(|_| MalformedRegistration)?;
            Ok(RegistrationRequest::Replace(
                Crontab::parse(text).map_err(|_| MalformedRegistration)?,
            ))
        }
        2 if body.is_empty() => Ok(RegistrationRequest::List),
        _ => Err(MalformedRegistration),
    }
}

pub fn encode_reply(status: u8, text: &str) -> Vec<u8> {
    encode(status, text)
}

pub fn decode_reply_header(bytes: &[u8]) -> Result<ReplyHeader, MalformedRegistration> {
    let (status, body_length) = decode_header(bytes)?;
    if !matches!(status, STATUS_OK | STATUS_REJECTED) {
        return Err(MalformedRegistration);
    }
    Ok(ReplyHeader { status, body_length })
}

pub fn decode_reply(header: ReplyHeader, body: &[u8]) -> Result<RegistrationReply, MalformedRegistration> {
    if body.len() != header.body_length
        || body.len() > MAX_CRONTAB_BYTES
        || !matches!(header.status, STATUS_OK | STATUS_REJECTED)
    {
        return Err(MalformedRegistration);
    }
    let text = std::str::from_utf8(body)
        .map_err(|_| MalformedRegistration)?
        .to_owned();
    Ok(RegistrationReply {
        status: header.status,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_bytes_match_the_guest_c_header() {
        let request = RegistrationRequest::Replace(Crontab::parse("* * * * * echo ok\n").unwrap());
        let frame = encode_request(&request);
        assert_eq!(&frame[..HEADER_BYTES], b"NBC1\x01\x00\x00\x00\x12");
        assert_eq!(
            decode_request(
                decode_request_header(&frame[..HEADER_BYTES]).unwrap(),
                &frame[HEADER_BYTES..]
            )
            .unwrap(),
            request
        );
        assert_eq!(
            encode_request(&RegistrationRequest::List),
            b"NBC1\x02\x00\x00\x00\x00"
        );
    }

    #[test]
    fn removing_a_table_is_replacing_it_with_empty_text() {
        let request = RegistrationRequest::Replace(Crontab::parse("").unwrap());
        assert_eq!(encode_request(&request), b"NBC1\x01\x00\x00\x00\x00");
    }

    #[test]
    fn a_registration_never_exposes_its_text_in_debug_output() {
        let request = RegistrationRequest::Replace(Crontab::parse("tenant-secret").unwrap());
        let reply = RegistrationReply {
            status: STATUS_OK,
            text: "tenant-secret".to_owned(),
        };
        assert!(!format!("{request:?} {reply:?}").contains("tenant-secret"));
    }

    #[test]
    fn malformed_headers_are_rejected_before_a_body_is_allocated() {
        for frame in [
            b"XXX1\x01\x00\x00\x00\x00".to_vec(),
            b"NBC1\x03\x00\x00\x00\x00".to_vec(),
            b"NBC1\x02\x00\x00\x00\x01".to_vec(),
            b"NBC1\x01\x00\x01\x00\x01".to_vec(),
            vec![],
        ] {
            assert!(decode_request_header(&frame).is_err());
        }
    }

    #[test]
    fn a_truncated_body_invalid_utf8_and_nul_are_rejected() {
        for body in [b"a\0".as_slice(), b"\xff\xfe", b"a"] {
            assert!(decode_request(
                RequestHeader {
                    verb: 1,
                    body_length: 2
                },
                body
            )
            .is_err());
        }
    }

    #[test]
    fn a_reply_round_trips_utf8_and_a_status() {
        for status in [STATUS_OK, STATUS_REJECTED] {
            let frame = encode_reply(status, "echo 🔑");
            let reply = decode_reply(
                decode_reply_header(&frame[..HEADER_BYTES]).unwrap(),
                &frame[HEADER_BYTES..],
            )
            .unwrap();
            assert_eq!(reply.status, status);
            assert_eq!(reply.text, "echo 🔑");
        }
    }
}
