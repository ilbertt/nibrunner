//! The bytes the guest's filesystem process speaks. `apps/runtime/src/guest-filesystem.h` states
//! the format; this is the half that encodes and decodes it.
//!
//! Nothing here is text. A path goes out behind its own length and a name comes back behind one,
//! because the tenant's binary created these names and ext4 allows anything in them but `/` and
//! NUL. Length prefixes are what make quoting unnecessary rather than merely relaxed.

use protocol::{
    DirectoryListing, FilesystemEntry, FilesystemEntryKind, GuestPath, Timestamp, DIRECTORY_ENTRY_LIMIT,
};

pub const FRAME_MAGIC: &[u8; 4] = b"NBF1";
const CODE_OFFSET: usize = 4;
const LENGTH_OFFSET: usize = 5;
pub const FRAME_HEADER_BYTES: usize = 9;

/// What one frame's body may hold, which is the guest's ceiling and the reason it never allocates.
pub const BODY_MAX_BYTES: usize = 65_536;

/// A chunk a caller can read or write without having to work out how much room its own path
/// left. `fits_one_request` is what actually decides; this is the size that always does.
pub const GUEST_FILESYSTEM_CHUNK_BYTES: usize = 32_768;

const STATUS_OK: u8 = 0;
const TRUNCATE: u8 = 1;
const NO_FLAGS: u8 = 0;

/// What `mkfs.ext4` puts at the root of every filesystem it writes. A tenant did not create these
/// and has no use for them. At the root only: the same name deeper in the tree is a tenant's.
pub const MKFS_ROOT_ENTRIES: [&str; 1] = ["lost+found"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestFilesystemRequest {
    List {
        path: GuestPath,
    },
    Stat {
        path: GuestPath,
    },
    Read {
        path: GuestPath,
        offset: u64,
        length: u32,
    },
    Write {
        path: GuestPath,
        offset: u64,
        content: Vec<u8>,
        truncate: bool,
    },
    MakeDirectory {
        path: GuestPath,
    },
    Remove {
        path: GuestPath,
    },
    Move {
        path: GuestPath,
        destination: GuestPath,
    },
    Usage,
    Compute,
}

impl GuestFilesystemRequest {
    fn verb(&self) -> u8 {
        match self {
            Self::List { .. } => 1,
            Self::Stat { .. } => 2,
            Self::Read { .. } => 3,
            Self::Write { .. } => 4,
            Self::MakeDirectory { .. } => 5,
            Self::Remove { .. } => 6,
            Self::Move { .. } => 7,
            Self::Usage => 8,
            Self::Compute => 9,
        }
    }
}

/// Read as a sentence rather than as a code, because this is the half of a failure that reaches
/// whoever asked. None of them names the path.
pub fn refusal_for(status: u8) -> &'static str {
    match status {
        1 => "there is nothing at that path",
        2 => "what is there is not the kind of thing that asks for",
        3 => "something is there already",
        4 => "the directory still holds something",
        5 => "that path leads out of the volume",
        6 => "the guest could not read the request",
        7 => "the guest could not carry it out",
        _ => "it gave no reason this host understands",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the guest answered about its files with bytes this host cannot read: {reason}")]
pub struct MalformedGuestReply {
    pub reason: &'static str,
}

fn field(value: &[u8]) -> Vec<u8> {
    let mut encoded = (value.len() as u32).to_be_bytes().to_vec();
    encoded.extend_from_slice(value);
    encoded
}

fn body_of(request: &GuestFilesystemRequest) -> Vec<u8> {
    // The verbs that name no path, because what they answer about is the guest rather than a
    // place in the tenant's filesystem.
    match request {
        GuestFilesystemRequest::Usage | GuestFilesystemRequest::Compute => Vec::new(),
        GuestFilesystemRequest::Read { path, offset, length } => {
            let mut body = field(path.as_str().as_bytes());
            body.extend_from_slice(&offset.to_be_bytes());
            body.extend_from_slice(&length.to_be_bytes());
            body
        }
        GuestFilesystemRequest::Write {
            path,
            offset,
            content,
            truncate,
        } => {
            let mut body = field(path.as_str().as_bytes());
            body.extend_from_slice(&offset.to_be_bytes());
            body.push(if *truncate { TRUNCATE } else { NO_FLAGS });
            body.extend(field(content));
            body
        }
        GuestFilesystemRequest::Move { path, destination } => {
            let mut body = field(path.as_str().as_bytes());
            body.extend(field(destination.as_str().as_bytes()));
            body
        }
        GuestFilesystemRequest::List { path }
        | GuestFilesystemRequest::Stat { path }
        | GuestFilesystemRequest::MakeDirectory { path }
        | GuestFilesystemRequest::Remove { path } => field(path.as_str().as_bytes()),
    }
}

pub fn encode_request(request: &GuestFilesystemRequest) -> Vec<u8> {
    let body = body_of(request);
    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + body.len());
    frame.extend_from_slice(FRAME_MAGIC);
    frame.push(request.verb());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend(body);
    frame
}

/// Asked before sending rather than discovered afterwards: the guest answers an oversized frame
/// by hanging up, which would cost the connection as well as the request.
pub fn fits_one_request(request: &GuestFilesystemRequest) -> bool {
    body_of(request).len() <= BODY_MAX_BYTES
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyHeader {
    pub status: u8,
    pub body_length: usize,
}

pub fn decode_header(header: &[u8]) -> Result<ReplyHeader, MalformedGuestReply> {
    if header.len() != FRAME_HEADER_BYTES {
        return Err(MalformedGuestReply {
            reason: "the header is the wrong length",
        });
    }
    if &header[..4] != FRAME_MAGIC {
        return Err(MalformedGuestReply {
            reason: "invalid magic value",
        });
    }
    let body_length = u32::from_be_bytes([header[LENGTH_OFFSET], header[6], header[7], header[8]]) as usize;
    if body_length > BODY_MAX_BYTES {
        return Err(MalformedGuestReply {
            reason: "the body exceeds the limit",
        });
    }
    Ok(ReplyHeader {
        status: header[CODE_OFFSET],
        body_length,
    })
}

pub fn is_refusal(status: u8) -> bool {
    status != STATUS_OK
}

/// One kind byte, an unsigned size, then a signed instant in seconds.
const DETAILS_BYTES: usize = 17;
const UINT32_BYTES: usize = 4;
const UINT64_BYTES: usize = 8;

/// Everything a listing says about one entry except its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemDetails {
    pub kind: FilesystemEntryKind,
    pub size_bytes: u64,
    pub modified_at: Timestamp,
}

fn u64_at(body: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&body[offset..offset + 8]);
    u64::from_be_bytes(bytes)
}

/// ext4 dates every file it holds, but nothing stops the bytes describing one being nonsense, and
/// a name is worth more to whoever is looking than a date nobody set. An instant that cannot be
/// written down costs its own field rather than the entry carrying it.
fn timestamp_from(seconds: i64) -> Timestamp {
    seconds
        .checked_mul(1000)
        .and_then(chrono_seconds)
        .unwrap_or_else(|| Timestamp::parse("1970-01-01T00:00:00Z").expect("the epoch"))
}

fn chrono_seconds(epoch_ms: i64) -> Option<Timestamp> {
    // Through the seconds and no further, because seconds are all a filesystem stamps a file with.
    let rendered = Timestamp::from_epoch_ms(epoch_ms);
    let text = rendered.as_str();
    if text.starts_with("1970-01-01T00:00:00") && epoch_ms != 0 {
        return None;
    }
    Timestamp::parse(format!("{}Z", &text[..19])).ok()
}

fn details_at(body: &[u8], offset: usize) -> FilesystemDetails {
    FilesystemDetails {
        kind: match body[offset] {
            1 => FilesystemEntryKind::File,
            2 => FilesystemEntryKind::Directory,
            _ => FilesystemEntryKind::Other,
        },
        size_bytes: u64_at(body, offset + 1),
        modified_at: timestamp_from(u64_at(body, offset + 9) as i64),
    }
}

pub fn decode_details(body: &[u8]) -> Result<FilesystemDetails, MalformedGuestReply> {
    if body.len() < DETAILS_BYTES {
        return Err(MalformedGuestReply {
            reason: "the details are the wrong length",
        });
    }
    Ok(details_at(body, 0))
}

pub fn decode_written(body: &[u8]) -> Result<u32, MalformedGuestReply> {
    if body.len() < 4 {
        return Err(MalformedGuestReply {
            reason: "no count came back from a write",
        });
    }
    Ok(u32::from_be_bytes([body[0], body[1], body[2], body[3]]))
}

/// What the guest measured, without the moment it was measured, which is this end's to stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasuredBytes {
    pub total_bytes: u64,
    pub used_bytes: u64,
}

pub fn decode_usage(body: &[u8]) -> Result<MeasuredBytes, MalformedGuestReply> {
    if body.len() < 16 {
        return Err(MalformedGuestReply {
            reason: "the usage is the wrong length",
        });
    }
    Ok(MeasuredBytes {
        total_bytes: u64_at(body, 0),
        used_bytes: u64_at(body, 8),
    })
}

/// The ticks are cumulative since the guest booted and mean nothing on their own: a share is the
/// difference between two of these over the time between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasuredCompute {
    pub memory_total_bytes: u64,
    pub memory_used_bytes: u64,
    pub cpu_total_ticks: u64,
    pub cpu_busy_ticks: u64,
}

pub fn decode_compute(body: &[u8]) -> Result<MeasuredCompute, MalformedGuestReply> {
    if body.len() < 32 {
        return Err(MalformedGuestReply {
            reason: "the compute reading is the wrong length",
        });
    }
    Ok(MeasuredCompute {
        memory_total_bytes: u64_at(body, 0),
        memory_used_bytes: u64_at(body, 8),
        cpu_total_ticks: u64_at(body, 16),
        cpu_busy_ticks: u64_at(body, 24),
    })
}

/// `.` and `..` never arrive: the guest leaves out the filesystem's own bookkeeping. What is left
/// out here instead is what `mkfs.ext4` put at the root of the volume, because that is the host's
/// knowledge and not the guest's. `truncated` can come from either side.
pub fn decode_listing(body: &[u8], path: &GuestPath) -> Result<DirectoryListing, MalformedGuestReply> {
    if body.is_empty() {
        return Err(MalformedGuestReply {
            reason: "a listing came back with nothing in it",
        });
    }
    let at_root = path.as_str() == "/";
    let mut truncated = body[0] != 0;
    let mut entries = Vec::new();
    let mut offset = 1;
    while offset < body.len() {
        let name_length_at = offset + DETAILS_BYTES;
        if name_length_at >= body.len() {
            return Err(MalformedGuestReply {
                reason: "an entry was cut short",
            });
        }
        let name_length = body[name_length_at] as usize;
        let name_at = name_length_at + 1;
        if name_length == 0 || name_at + name_length > body.len() {
            return Err(MalformedGuestReply {
                reason: "an entry names nothing readable",
            });
        }
        let name = String::from_utf8_lossy(&body[name_at..name_at + name_length]).into_owned();
        if entries.len() == DIRECTORY_ENTRY_LIMIT {
            truncated = true;
            break;
        }
        if !(at_root && MKFS_ROOT_ENTRIES.contains(&name.as_str())) {
            let details = details_at(body, offset);
            entries.push(FilesystemEntry {
                name,
                kind: details.kind,
                size_bytes: details.size_bytes,
                modified_at: details.modified_at,
            });
        }
        offset = name_at + name_length;
    }
    Ok(DirectoryListing {
        path: path.clone(),
        entries,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: u8, size: u64, mtime: i64, name: &str) -> Vec<u8> {
        let mut bytes = vec![kind];
        bytes.extend_from_slice(&size.to_be_bytes());
        bytes.extend_from_slice(&mtime.to_be_bytes());
        bytes.push(name.len() as u8);
        bytes.extend_from_slice(name.as_bytes());
        bytes
    }

    #[test]
    fn a_list_request_is_the_magic_the_verb_a_length_and_a_length_prefixed_path() {
        let frame = encode_request(&GuestFilesystemRequest::List {
            path: GuestPath::parse("/a b").unwrap(),
        });
        assert_eq!(
            frame,
            [
                b"NBF1".as_slice(),
                &[1],
                &8u32.to_be_bytes(),
                &4u32.to_be_bytes(),
                b"/a b"
            ]
            .concat()
        );
        assert_eq!(
            encode_request(&GuestFilesystemRequest::Usage),
            [b"NBF1".as_slice(), &[8], &0u32.to_be_bytes()].concat()
        );
    }

    #[test]
    fn a_reply_header_is_checked_before_a_body_is_allocated() {
        let mut header = b"NBF1".to_vec();
        header.push(0);
        header.extend_from_slice(&5u32.to_be_bytes());
        assert_eq!(
            decode_header(&header).unwrap(),
            ReplyHeader {
                status: 0,
                body_length: 5
            }
        );
        let mut oversized = header.clone();
        oversized[5..9].copy_from_slice(&(BODY_MAX_BYTES as u32 + 1).to_be_bytes());
        assert!(decode_header(&oversized).is_err());
        assert!(decode_header(&header[..8]).is_err());
        assert!(!is_refusal(0));
        assert!(is_refusal(5));
        assert_eq!(refusal_for(5), "that path leads out of the volume");
    }

    #[test]
    fn a_listing_hides_lost_and_found_at_the_root_only_and_survives_odd_names() {
        let mut body = vec![0u8];
        body.extend(entry(2, 4096, 1_754_215_200, "lost+found"));
        body.extend(entry(1, 12, 1_754_215_200, "it's a \"file\"\n"));
        let root = decode_listing(&body, &GuestPath::root()).unwrap();
        assert_eq!(root.entries.len(), 1);
        assert_eq!(root.entries[0].name, "it's a \"file\"\n");
        assert_eq!(root.entries[0].kind, FilesystemEntryKind::File);
        assert_eq!(root.entries[0].modified_at.as_str(), "2025-08-03T10:00:00Z");
        let nested = decode_listing(&body, &GuestPath::parse("/x").unwrap()).unwrap();
        assert_eq!(nested.entries.len(), 2);
        assert!(!nested.truncated);
        let mut truncated = vec![1u8];
        truncated.extend(entry(3, 0, 0, "sock"));
        let listing = decode_listing(&truncated, &GuestPath::root()).unwrap();
        assert!(listing.truncated);
        assert_eq!(listing.entries[0].kind, FilesystemEntryKind::Other);
        assert!(decode_listing(&[], &GuestPath::root()).is_err());
        assert!(decode_listing(&[0, 1, 2], &GuestPath::root()).is_err());
    }

    #[test]
    fn usage_and_compute_are_big_endian_u64s_in_order() {
        let mut usage = 8_455_712_768u64.to_be_bytes().to_vec();
        usage.extend_from_slice(&1_503_238_553u64.to_be_bytes());
        assert_eq!(
            decode_usage(&usage).unwrap(),
            MeasuredBytes {
                total_bytes: 8_455_712_768,
                used_bytes: 1_503_238_553
            }
        );
        let mut compute = Vec::new();
        for value in [1_031_012_352u64, 412_401_664, 100_000, 18_000] {
            compute.extend_from_slice(&value.to_be_bytes());
        }
        assert_eq!(
            decode_compute(&compute).unwrap(),
            MeasuredCompute {
                memory_total_bytes: 1_031_012_352,
                memory_used_bytes: 412_401_664,
                cpu_total_ticks: 100_000,
                cpu_busy_ticks: 18_000
            }
        );
        assert!(decode_compute(&compute[..31]).is_err());
        assert_eq!(decode_written(&7u32.to_be_bytes()).unwrap(), 7);
    }

    #[test]
    fn a_write_that_does_not_fit_one_frame_is_refused_before_it_is_sent() {
        let small = GuestFilesystemRequest::Write {
            path: GuestPath::parse("/f").unwrap(),
            offset: 0,
            content: vec![0; GUEST_FILESYSTEM_CHUNK_BYTES],
            truncate: true,
        };
        assert!(fits_one_request(&small));
        let large = GuestFilesystemRequest::Write {
            path: GuestPath::parse("/f").unwrap(),
            offset: 0,
            content: vec![0; BODY_MAX_BYTES],
            truncate: false,
        };
        assert!(!fits_one_request(&large));
    }
}

// ---------------------------------------------------------------------------------------------
// The guest's half
//
// The same frame, read the other way round. Kept here rather than in the guest so that there is
// one definition of the format and not two that agree by hand: a round-trip test can only exist
// where both directions are in reach of each other, and a change to the wire that breaks one end
// fails to compile against the other.

/// A frame the guest could not read. The status it answers with is the one the host renders as
/// "the guest could not read the request", so the reason is for this side's log and never travels.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a request about the tenant's files could not be read: {reason}")]
pub struct MalformedRequest {
    pub reason: &'static str,
}

/// What the guest answers a request it could not read with.
pub const STATUS_MALFORMED_REQUEST: u8 = 6;

struct Cursor<'a> {
    body: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self { body, at: 0 }
    }

    fn take(&mut self, count: usize, reason: &'static str) -> Result<&'a [u8], MalformedRequest> {
        let end = self.at.checked_add(count).ok_or(MalformedRequest { reason })?;
        let taken = self.body.get(self.at..end).ok_or(MalformedRequest { reason })?;
        self.at = end;
        Ok(taken)
    }

    fn field(&mut self, reason: &'static str) -> Result<&'a [u8], MalformedRequest> {
        let length = u32::from_be_bytes(
            self.take(UINT32_BYTES, reason)?
                .try_into()
                .map_err(|_| MalformedRequest { reason })?,
        );
        self.take(length as usize, reason)
    }

    fn path(&mut self, reason: &'static str) -> Result<GuestPath, MalformedRequest> {
        let bytes = self.field(reason)?;
        // Parsed rather than taken as given. A path arrives from off this machine, and the one
        // thing the guest must never do is walk out of the volume because something asked it to;
        // the rest of the server can then treat a `GuestPath` as a path that has been checked.
        let text = std::str::from_utf8(bytes).map_err(|_| MalformedRequest { reason })?;
        GuestPath::parse(text).map_err(|_| MalformedRequest { reason })
    }

    fn offset(&mut self, reason: &'static str) -> Result<u64, MalformedRequest> {
        Ok(u64::from_be_bytes(
            self.take(UINT64_BYTES, reason)?
                .try_into()
                .map_err(|_| MalformedRequest { reason })?,
        ))
    }

    fn length(&mut self, reason: &'static str) -> Result<u32, MalformedRequest> {
        Ok(u32::from_be_bytes(
            self.take(UINT32_BYTES, reason)?
                .try_into()
                .map_err(|_| MalformedRequest { reason })?,
        ))
    }

    fn byte(&mut self, reason: &'static str) -> Result<u8, MalformedRequest> {
        Ok(self.take(1, reason)?[0])
    }
}

/// The header of a request, read by the guest. Separate from the body for the same reason the
/// host reads a reply in two goes: the length says how much more to wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestHeader {
    pub verb: u8,
    pub body_length: usize,
}

pub fn decode_request_header(header: &[u8]) -> Result<RequestHeader, MalformedRequest> {
    if header.len() != FRAME_HEADER_BYTES {
        return Err(MalformedRequest {
            reason: "the header is the wrong length",
        });
    }
    if &header[..FRAME_MAGIC.len()] != FRAME_MAGIC {
        return Err(MalformedRequest {
            reason: "invalid magic value",
        });
    }
    let body_length = u32::from_be_bytes([
        header[LENGTH_OFFSET],
        header[LENGTH_OFFSET + 1],
        header[LENGTH_OFFSET + 2],
        header[LENGTH_OFFSET + 3],
    ]) as usize;
    // Refused rather than allocated for. The ceiling is what lets the guest read into a buffer it
    // sized once at startup, which is the whole reason it never allocates per request.
    if body_length > BODY_MAX_BYTES {
        return Err(MalformedRequest {
            reason: "the body exceeds the limit",
        });
    }
    Ok(RequestHeader {
        verb: header[CODE_OFFSET],
        body_length,
    })
}

/// The request a verb and its body describe.
///
/// Every field is bounds-checked against the body it came from, so a truncated or lying frame is a
/// refusal rather than a read past the end of a buffer. Trailing bytes are not an error: a host
/// that learns a longer form of a verb this guest is older than should still be understood as far
/// as this guest goes.
pub fn decode_request(
    header: RequestHeader,
    body: &[u8],
) -> Result<GuestFilesystemRequest, MalformedRequest> {
    if body.len() != header.body_length {
        return Err(MalformedRequest {
            reason: "the body is not the length the header gave",
        });
    }
    let mut cursor = Cursor::new(body);
    match header.verb {
        1 => Ok(GuestFilesystemRequest::List {
            path: cursor.path("a list names no readable path")?,
        }),
        2 => Ok(GuestFilesystemRequest::Stat {
            path: cursor.path("a stat names no readable path")?,
        }),
        3 => {
            let path = cursor.path("a read names no readable path")?;
            Ok(GuestFilesystemRequest::Read {
                path,
                offset: cursor.offset("a read carries no offset")?,
                length: cursor.length("a read carries no length")?,
            })
        }
        4 => {
            let path = cursor.path("a write names no readable path")?;
            let offset = cursor.offset("a write carries no offset")?;
            let truncate = cursor.byte("a write carries no flags")? == TRUNCATE;
            Ok(GuestFilesystemRequest::Write {
                path,
                offset,
                truncate,
                content: cursor.field("a write carries no content")?.to_vec(),
            })
        }
        5 => Ok(GuestFilesystemRequest::MakeDirectory {
            path: cursor.path("a mkdir names no readable path")?,
        }),
        6 => Ok(GuestFilesystemRequest::Remove {
            path: cursor.path("a remove names no readable path")?,
        }),
        7 => {
            let path = cursor.path("a move names no readable path")?;
            Ok(GuestFilesystemRequest::Move {
                path,
                destination: cursor.path("a move names no readable destination")?,
            })
        }
        8 => Ok(GuestFilesystemRequest::Usage),
        9 => Ok(GuestFilesystemRequest::Compute),
        _ => Err(MalformedRequest {
            reason: "a verb this guest does not have",
        }),
    }
}

/// One reply. A refusal carries no body: the status is the whole of what travels, because the
/// sentence it becomes is the host's to render and a tenant's path is not an operator's to read.
pub fn encode_reply(status: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + body.len());
    frame.extend_from_slice(FRAME_MAGIC);
    frame.push(status);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(body);
    frame
}

pub fn encode_refusal(status: u8) -> Vec<u8> {
    encode_reply(status, &[])
}

/// What the guest has about one entry, which is what `stat` gave it.
///
/// The instant is seconds, not a `Timestamp`: seconds are all a filesystem stamps a file with, and
/// what to do about one that cannot be written down is the *host's* decision — it costs that field
/// rather than the entry carrying it. A guest that made that decision would be making it for a
/// renderer it cannot see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryDetails {
    pub kind: FilesystemEntryKind,
    pub size_bytes: u64,
    pub modified_seconds: i64,
}

fn details_bytes(details: &EntryDetails) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(DETAILS_BYTES);
    encoded.push(match details.kind {
        FilesystemEntryKind::File => 1,
        FilesystemEntryKind::Directory => 2,
        FilesystemEntryKind::Other => 0,
    });
    encoded.extend_from_slice(&details.size_bytes.to_be_bytes());
    encoded.extend_from_slice(&details.modified_seconds.to_be_bytes());
    encoded
}

pub fn encode_details(details: &EntryDetails) -> Vec<u8> {
    encode_reply(STATUS_OK, &details_bytes(details))
}

pub fn encode_written(count: u32) -> Vec<u8> {
    encode_reply(STATUS_OK, &count.to_be_bytes())
}

pub fn encode_usage(measured: &MeasuredBytes) -> Vec<u8> {
    let mut body = Vec::with_capacity(UINT64_BYTES * 2);
    body.extend_from_slice(&measured.total_bytes.to_be_bytes());
    body.extend_from_slice(&measured.used_bytes.to_be_bytes());
    encode_reply(STATUS_OK, &body)
}

pub fn encode_compute(measured: &MeasuredCompute) -> Vec<u8> {
    let mut body = Vec::with_capacity(UINT64_BYTES * 4);
    body.extend_from_slice(&measured.memory_total_bytes.to_be_bytes());
    body.extend_from_slice(&measured.memory_used_bytes.to_be_bytes());
    body.extend_from_slice(&measured.cpu_total_ticks.to_be_bytes());
    body.extend_from_slice(&measured.cpu_busy_ticks.to_be_bytes());
    encode_reply(STATUS_OK, &body)
}

/// One name per entry, behind a single length byte — which is what caps a name at 255 bytes on
/// this wire, well under what ext4 allows in the filesystem itself.
pub const MAX_ENTRY_NAME_BYTES: usize = 255;

/// A listing, filled until the frame is full rather than until the directory ends.
///
/// `truncated` is set here when the body would not hold another entry, which is the guest's half
/// of the same flag the host sets when its own limit is reached first. A name longer than one
/// length byte is skipped rather than truncated: half a filename is a name that points at nothing,
/// and one that cannot be shown is better left out than shown wrong.
pub fn encode_listing(entries: &[(String, EntryDetails)]) -> Vec<u8> {
    let mut body = vec![0u8];
    let mut truncated = false;
    for (name, details) in entries {
        let name = name.as_bytes();
        if name.is_empty() || name.len() > MAX_ENTRY_NAME_BYTES {
            continue;
        }
        let entry_bytes = DETAILS_BYTES + 1 + name.len();
        if body.len() + entry_bytes > BODY_MAX_BYTES {
            truncated = true;
            break;
        }
        body.extend(details_bytes(details));
        body.push(name.len() as u8);
        body.extend_from_slice(name);
    }
    body[0] = u8::from(truncated);
    encode_reply(STATUS_OK, &body)
}

#[cfg(test)]
mod both_ends {
    //! What having both halves here is for: a change to the wire that breaks one end fails
    //! against the other, rather than being discovered by a guest and a host disagreeing on a
    //! machine somewhere.

    use super::*;

    fn path(text: &str) -> GuestPath {
        GuestPath::parse(text).expect("a fixture is a valid guest path")
    }

    fn decoded(request: &GuestFilesystemRequest) -> GuestFilesystemRequest {
        let frame = encode_request(request);
        let header = decode_request_header(&frame[..FRAME_HEADER_BYTES]).expect("a header it wrote");
        decode_request(header, &frame[FRAME_HEADER_BYTES..]).expect("a body it wrote")
    }

    #[test]
    fn every_verb_the_host_sends_is_a_verb_the_guest_reads_back_unchanged() {
        let requests = [
            GuestFilesystemRequest::List { path: path("/") },
            GuestFilesystemRequest::Stat {
                path: path("/notes.txt"),
            },
            GuestFilesystemRequest::Read {
                path: path("/big.bin"),
                offset: 4_294_967_296,
                length: 32_768,
            },
            GuestFilesystemRequest::Write {
                path: path("/notes.txt"),
                offset: 17,
                content: b"some bytes".to_vec(),
                truncate: true,
            },
            GuestFilesystemRequest::Write {
                path: path("/notes.txt"),
                offset: 0,
                content: Vec::new(),
                truncate: false,
            },
            GuestFilesystemRequest::MakeDirectory {
                path: path("/uploads"),
            },
            GuestFilesystemRequest::Remove {
                path: path("/uploads"),
            },
            GuestFilesystemRequest::Move {
                path: path("/a"),
                destination: path("/b"),
            },
            GuestFilesystemRequest::Usage,
            GuestFilesystemRequest::Compute,
        ];
        for request in requests {
            assert_eq!(decoded(&request), request, "{request:?} did not survive the wire");
        }
    }

    /// A name is reported and a path is accepted, and the asymmetry is the design: the tenant's
    /// own binary created these names, so anything ext4 allows has to survive being *described* —
    /// while the direction that carries a request stays strict.
    ///
    /// The quote rule is the one that costs something. It is there because nibrun's path ends up
    /// in a command string its reader's tooling tokenises, so a directory named `it's` can be
    /// listed and never descended into. Nothing on *this* wire tokenises anything — the path goes
    /// out behind its own length — so the restriction buys this daemon nothing. Kept anyway,
    /// because the schema is the contract with the control plane and one end relaxing it alone
    /// would be a path this host accepts and that one refuses. See DECISIONS.md.
    #[test]
    fn a_name_may_hold_what_a_path_may_not() {
        for awkward in ["/-leading-dash", "/a b c", "/naïve", "/a.file", "/deep/er/still"] {
            let request = GuestFilesystemRequest::List { path: path(awkward) };
            assert_eq!(decoded(&request), request);
        }
        for refused in ["/it's", "/a \"quoted\" name", "/back\\slash", "/a\tb"] {
            assert!(
                GuestPath::parse(refused).is_err(),
                "{refused} was accepted as a path"
            );
        }
        // The same text, as a name in a listing that comes back: reported without complaint.
        let entries = [entry("it's a \"file\"", FilesystemEntryKind::File, 1)];
        let frame = encode_listing(&entries);
        let listing = decode_listing(&frame[FRAME_HEADER_BYTES..], &path("/data")).unwrap();
        assert_eq!(listing.entries[0].name, "it's a \"file\"");
    }

    /// A guest reads into a buffer it sized once at startup, so a header claiming more than the
    /// ceiling is refused before anything is allocated for it.
    #[test]
    fn a_header_claiming_more_than_one_frame_holds_is_refused_before_any_body_is_read() {
        let mut frame = encode_request(&GuestFilesystemRequest::Usage);
        frame[LENGTH_OFFSET..LENGTH_OFFSET + 4].copy_from_slice(&((BODY_MAX_BYTES + 1) as u32).to_be_bytes());
        let error = decode_request_header(&frame[..FRAME_HEADER_BYTES]).unwrap_err();
        assert_eq!(error.reason, "the body exceeds the limit");
    }

    /// A truncated or lying frame is a refusal, never a read past the end of the buffer.
    #[test]
    fn a_body_cut_short_is_refused_rather_than_read_past() {
        let frame = encode_request(&GuestFilesystemRequest::Read {
            path: path("/notes.txt"),
            offset: 0,
            length: 16,
        });
        let header = decode_request_header(&frame[..FRAME_HEADER_BYTES]).unwrap();
        for cut in 1..frame.len() - FRAME_HEADER_BYTES {
            let body = &frame[FRAME_HEADER_BYTES..frame.len() - cut];
            assert!(
                decode_request(header, body).is_err(),
                "a body {cut} bytes short was read anyway"
            );
        }
    }

    /// The one thing a guest must never do is walk out of the volume because something asked it
    /// to, so a path is parsed on the way in and the rest of the server can treat it as checked.
    #[test]
    fn a_path_that_leads_out_of_the_volume_never_becomes_a_request() {
        for escape in ["../etc/shadow", "/../etc/shadow", "relative", ""] {
            let mut body = (escape.len() as u32).to_be_bytes().to_vec();
            body.extend_from_slice(escape.as_bytes());
            let header = RequestHeader {
                verb: 1,
                body_length: body.len(),
            };
            assert!(
                decode_request(header, &body).is_err(),
                "{escape} was read as a path to list"
            );
        }
    }

    #[test]
    fn a_verb_this_guest_does_not_have_is_refused_rather_than_guessed_at() {
        let header = RequestHeader {
            verb: 200,
            body_length: 0,
        };
        assert_eq!(
            decode_request(header, &[]).unwrap_err().reason,
            "a verb this guest does not have"
        );
    }

    fn entry(name: &str, kind: FilesystemEntryKind, size: u64) -> (String, EntryDetails) {
        (
            name.to_string(),
            EntryDetails {
                kind,
                size_bytes: size,
                modified_seconds: 1_760_000_000,
            },
        )
    }

    #[test]
    fn a_listing_the_guest_writes_is_a_listing_the_host_reads() {
        let entries = [
            entry("notes.txt", FilesystemEntryKind::File, 42),
            entry("uploads", FilesystemEntryKind::Directory, 4096),
            entry("it's a \"file\"\nreally", FilesystemEntryKind::File, 7),
        ];
        let frame = encode_listing(&entries);
        let header = decode_header(&frame[..FRAME_HEADER_BYTES]).unwrap();
        assert!(!is_refusal(header.status));
        let listing = decode_listing(&frame[FRAME_HEADER_BYTES..], &path("/data")).unwrap();

        assert!(!listing.truncated);
        assert_eq!(listing.entries.len(), 3);
        assert_eq!(listing.entries[0].name, "notes.txt");
        assert_eq!(listing.entries[0].size_bytes, 42);
        assert_eq!(listing.entries[1].kind, FilesystemEntryKind::Directory);
        assert_eq!(listing.entries[2].name, "it's a \"file\"\nreally");
    }

    /// The guest sets `truncated` when the body would not hold another entry; the host sets the
    /// same flag when its own limit is reached first. Either way the reader is told.
    #[test]
    fn a_directory_that_outgrew_one_frame_says_so() {
        let many: Vec<(String, EntryDetails)> = (0..4000)
            .map(|index| entry(&format!("file-{index:040}"), FilesystemEntryKind::File, 1))
            .collect();
        let frame = encode_listing(&many);
        assert!(frame.len() <= FRAME_HEADER_BYTES + BODY_MAX_BYTES);
        let listing = decode_listing(&frame[FRAME_HEADER_BYTES..], &path("/data")).unwrap();
        assert!(listing.truncated);
        assert!(listing.entries.len() < many.len());
    }

    /// Half a filename is a name that points at nothing, so one too long for the wire is left out
    /// rather than cut down to fit.
    #[test]
    fn a_name_too_long_for_the_wire_is_left_out_rather_than_shown_wrong() {
        let long = "n".repeat(MAX_ENTRY_NAME_BYTES + 1);
        let entries = [
            entry(&long, FilesystemEntryKind::File, 1),
            entry("short", FilesystemEntryKind::File, 1),
        ];
        let frame = encode_listing(&entries);
        let listing = decode_listing(&frame[FRAME_HEADER_BYTES..], &path("/data")).unwrap();
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["short"]
        );
    }

    #[test]
    fn every_answer_the_guest_writes_is_one_the_host_reads() {
        let details = EntryDetails {
            kind: FilesystemEntryKind::File,
            size_bytes: 1234,
            modified_seconds: 1_760_000_000,
        };
        let frame = encode_details(&details);
        let read = decode_details(&frame[FRAME_HEADER_BYTES..]).unwrap();
        assert_eq!(read.kind, details.kind);
        assert_eq!(read.size_bytes, details.size_bytes);

        let frame = encode_written(4096);
        assert_eq!(decode_written(&frame[FRAME_HEADER_BYTES..]).unwrap(), 4096);

        let usage = MeasuredBytes {
            total_bytes: 8 * 1024 * 1024 * 1024,
            used_bytes: 1234,
        };
        let frame = encode_usage(&usage);
        assert_eq!(decode_usage(&frame[FRAME_HEADER_BYTES..]).unwrap(), usage);

        let compute = MeasuredCompute {
            memory_total_bytes: 268_435_456,
            memory_used_bytes: 1024,
            cpu_total_ticks: 90_000,
            cpu_busy_ticks: 1_200,
        };
        let frame = encode_compute(&compute);
        assert_eq!(decode_compute(&frame[FRAME_HEADER_BYTES..]).unwrap(), compute);
    }

    /// A refusal carries no body: the status is the whole of what travels, because the sentence it
    /// becomes is the host's to render and a tenant's path is not an operator's to read.
    #[test]
    fn a_refusal_carries_a_status_and_nothing_else() {
        let frame = encode_refusal(STATUS_MALFORMED_REQUEST);
        let header = decode_header(&frame).unwrap();
        assert!(is_refusal(header.status));
        assert_eq!(header.body_length, 0);
        assert_eq!(refusal_for(header.status), "the guest could not read the request");
    }
}
