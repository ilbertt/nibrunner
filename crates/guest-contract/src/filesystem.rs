//! The bytes the guest's filesystem process speaks. `apps/runtime/src/guest-filesystem.h` states
//! the format; this is the half that encodes and decodes it.
//!
//! Nothing here is text. A path goes out behind its own length and a name comes back behind one,
//! because the tenant's binary created these names and ext4 allows anything in them but `/` and
//! NUL. Length prefixes are what make quoting unnecessary rather than merely relaxed.

use protocol::{DirectoryListing, FilesystemEntry, FilesystemEntryKind, GuestPath, Timestamp, DIRECTORY_ENTRY_LIMIT};

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
    List { path: GuestPath },
    Stat { path: GuestPath },
    Read { path: GuestPath, offset: u64, length: u32 },
    Write { path: GuestPath, offset: u64, content: Vec<u8>, truncate: bool },
    MakeDirectory { path: GuestPath },
    Remove { path: GuestPath },
    Move { path: GuestPath, destination: GuestPath },
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
        GuestFilesystemRequest::Write { path, offset, content, truncate } => {
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
        return Err(MalformedGuestReply { reason: "the header is the wrong length" });
    }
    if &header[..4] != FRAME_MAGIC {
        return Err(MalformedGuestReply { reason: "invalid magic value" });
    }
    let body_length = u32::from_be_bytes([header[LENGTH_OFFSET], header[6], header[7], header[8]]) as usize;
    if body_length > BODY_MAX_BYTES {
        return Err(MalformedGuestReply { reason: "the body exceeds the limit" });
    }
    Ok(ReplyHeader { status: header[CODE_OFFSET], body_length })
}

pub fn is_refusal(status: u8) -> bool {
    status != STATUS_OK
}

/// One kind byte, an unsigned size, then a signed instant in seconds.
const DETAILS_BYTES: usize = 17;

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
        return Err(MalformedGuestReply { reason: "the details are the wrong length" });
    }
    Ok(details_at(body, 0))
}

pub fn decode_written(body: &[u8]) -> Result<u32, MalformedGuestReply> {
    if body.len() < 4 {
        return Err(MalformedGuestReply { reason: "no count came back from a write" });
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
        return Err(MalformedGuestReply { reason: "the usage is the wrong length" });
    }
    Ok(MeasuredBytes { total_bytes: u64_at(body, 0), used_bytes: u64_at(body, 8) })
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
        return Err(MalformedGuestReply { reason: "the compute reading is the wrong length" });
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
        return Err(MalformedGuestReply { reason: "a listing came back with nothing in it" });
    }
    let at_root = path.as_str() == "/";
    let mut truncated = body[0] != 0;
    let mut entries = Vec::new();
    let mut offset = 1;
    while offset < body.len() {
        let name_length_at = offset + DETAILS_BYTES;
        if name_length_at >= body.len() {
            return Err(MalformedGuestReply { reason: "an entry was cut short" });
        }
        let name_length = body[name_length_at] as usize;
        let name_at = name_length_at + 1;
        if name_length == 0 || name_at + name_length > body.len() {
            return Err(MalformedGuestReply { reason: "an entry names nothing readable" });
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
    Ok(DirectoryListing { path: path.clone(), entries, truncated })
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
        let frame = encode_request(&GuestFilesystemRequest::List { path: GuestPath::parse("/a b").unwrap() });
        assert_eq!(frame, [b"NBF1".as_slice(), &[1], &8u32.to_be_bytes(), &4u32.to_be_bytes(), b"/a b"].concat());
        assert_eq!(encode_request(&GuestFilesystemRequest::Usage), [b"NBF1".as_slice(), &[8], &0u32.to_be_bytes()].concat());
    }

    #[test]
    fn a_reply_header_is_checked_before_a_body_is_allocated() {
        let mut header = b"NBF1".to_vec();
        header.push(0);
        header.extend_from_slice(&5u32.to_be_bytes());
        assert_eq!(decode_header(&header).unwrap(), ReplyHeader { status: 0, body_length: 5 });
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
        assert_eq!(decode_usage(&usage).unwrap(), MeasuredBytes { total_bytes: 8_455_712_768, used_bytes: 1_503_238_553 });
        let mut compute = Vec::new();
        for value in [1_031_012_352u64, 412_401_664, 100_000, 18_000] {
            compute.extend_from_slice(&value.to_be_bytes());
        }
        assert_eq!(
            decode_compute(&compute).unwrap(),
            MeasuredCompute { memory_total_bytes: 1_031_012_352, memory_used_bytes: 412_401_664, cpu_total_ticks: 100_000, cpu_busy_ticks: 18_000 }
        );
        assert!(decode_compute(&compute[..31]).is_err());
        assert_eq!(decode_written(&7u32.to_be_bytes()).unwrap(), 7);
    }

    #[test]
    fn a_write_that_does_not_fit_one_frame_is_refused_before_it_is_sent() {
        let small = GuestFilesystemRequest::Write { path: GuestPath::parse("/f").unwrap(), offset: 0, content: vec![0; GUEST_FILESYSTEM_CHUNK_BYTES], truncate: true };
        assert!(fits_one_request(&small));
        let large = GuestFilesystemRequest::Write { path: GuestPath::parse("/f").unwrap(), offset: 0, content: vec![0; BODY_MAX_BYTES], truncate: false };
        assert!(!fits_one_request(&large));
    }
}
