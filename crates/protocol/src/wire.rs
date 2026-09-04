//! The scalar types every document resolves to. Each is a validated newtype, so a value that
//! reached a struct has already passed the check the TypeBox schema on the other side applies.

use std::fmt;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct InvalidValue(String);

impl InvalidValue {
    pub(crate) fn new(what: &str, rule: &str) -> Self {
        Self(format!("{what} is not {rule}"))
    }

    /// A refusal whose text is the whole message, for the checks that read better as a sentence.
    pub(crate) fn new_public(message: &str) -> Self {
        Self(message.to_string())
    }
}

/// The same newtype, refused with a sentence of its own rather than "<what> is not <rule>".
macro_rules! validated_string_public {
    ($(#[$meta:meta])* $name:ident, $rule:expr, $check:expr) => {
        validated_string!($(#[$meta])* $name, stringify!($name), $rule, $check);
    };
}

macro_rules! validated_string {
    ($(#[$meta:meta])* $name:ident, $what:expr, $rule:expr, $check:expr) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, InvalidValue> {
                let value: String = value.into();
                let check: fn(&str) -> bool = $check;
                if check(&value) {
                    Ok(Self(value))
                } else {
                    Err(InvalidValue::new($what, $rule))
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidValue;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = InvalidValue;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, "{}({:?})", stringify!($name), self.0)
            }
        }
    };
}

const MAX_IDENTIFIER_LENGTH: usize = 63;

/// `^[0-9A-Za-z][0-9A-Za-z_-]{0,62}$`
pub fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    value.len() <= MAX_IDENTIFIER_LENGTH && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

macro_rules! identifier {
    ($(#[$meta:meta])* $name:ident) => {
        validated_string!($(#[$meta])* $name, stringify!($name), "an identifier", is_identifier);
    };
}

identifier!(
    /// The account an app belongs to.
    OwnerId
);
identifier!(
    /// A tenant app, and the microVM running it: an app runs one, so the two never differ.
    AppId
);
identifier!(
    /// One uploaded binary.
    ArtifactId
);
identifier!(
    /// One artifact plus the configuration it was launched with.
    DeploymentId
);
identifier!(
    /// One app host.
    HostId
);
identifier!(
    /// An app's persistent filesystem.
    VolumeId
);
identifier!(
    /// A point-in-time view of a volume, readable while the owning host still has it open.
    CheckpointId
);
identifier!(
    /// One request for a downloadable copy of an app.
    ExportId
);
identifier!(
    /// One read of one directory, alive only while its answer is still awaited.
    FilesystemQueryId
);

const SHA256_HEX_LENGTH: usize = 64;

fn is_sha256_hex(value: &str) -> bool {
    value.len() == SHA256_HEX_LENGTH
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

validated_string!(
    /// Lowercase hex SHA-256, unprefixed.
    Sha256Digest,
    "digest",
    "a lowercase hex sha-256",
    is_sha256_hex
);

const MAX_TIMESTAMP_LENGTH: usize = 35;

/// `^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d{1,9})?(Z|[+-]\d{2}:\d{2})$`. The offset is
/// mandatory: a local time with no offset is the failure this exists to catch, and it is the one
/// a lenient parser turns into a silently wrong instant. Calendar validity is not checked.
fn is_timestamp(value: &str) -> bool {
    if value.len() > MAX_TIMESTAMP_LENGTH {
        return false;
    }
    let bytes = value.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes
            .get(range)
            .is_some_and(|slice| slice.iter().all(u8::is_ascii_digit))
    };
    let at = |index: usize, expected: u8| bytes.get(index) == Some(&expected);
    if !(digits(0..4)
        && at(4, b'-')
        && digits(5..7)
        && at(7, b'-')
        && digits(8..10)
        && at(10, b'T')
        && digits(11..13)
        && at(13, b':')
        && digits(14..16)
        && at(16, b':')
        && digits(17..19))
    {
        return false;
    }
    let mut cursor = 19;
    if at(cursor, b'.') {
        let start = cursor + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let fraction = end - start;
        if fraction == 0 || fraction > 9 {
            return false;
        }
        cursor = end;
    }
    match bytes.get(cursor) {
        Some(b'Z') => cursor + 1 == bytes.len(),
        Some(b'+') | Some(b'-') => {
            digits(cursor + 1..cursor + 3)
                && at(cursor + 3, b':')
                && digits(cursor + 4..cursor + 6)
                && cursor + 6 == bytes.len()
        }
        _ => false,
    }
}

validated_string!(
    /// ISO 8601 instant with a mandatory UTC offset.
    Timestamp,
    "timestamp",
    "an ISO 8601 instant with an offset",
    is_timestamp
);

impl Timestamp {
    /// The instant as JavaScript's `toISOString` writes it: milliseconds, and `Z`.
    pub fn from_epoch_ms(epoch_ms: i64) -> Self {
        let instant = chrono::DateTime::from_timestamp_millis(epoch_ms)
            .unwrap_or_else(|| chrono::DateTime::from_timestamp_millis(0).expect("the epoch"));
        Self(instant.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
    }

    pub fn now() -> Self {
        Self::from_epoch_ms(chrono::Utc::now().timestamp_millis())
    }

    /// `Date.parse` for the instants this type admits.
    pub fn epoch_ms(&self) -> i64 {
        chrono::DateTime::parse_from_rfc3339(&self.0)
            .map(|instant| instant.timestamp_millis())
            .unwrap_or(0)
    }
}

pub const MAX_DNS_LABEL_LENGTH: usize = 63;

/// `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`
fn is_dns_label(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_DNS_LABEL_LENGTH {
        return false;
    }
    let inner = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-';
    let edge = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    edge(&bytes[0]) && edge(&bytes[bytes.len() - 1]) && bytes.iter().all(inner)
}

validated_string!(DnsLabel, "label", "a DNS label", is_dns_label);

const MAX_HOSTNAME_LENGTH: usize = 253;

/// At least two labels, each a DNS label.
fn is_hostname(value: &str) -> bool {
    value.len() <= MAX_HOSTNAME_LENGTH && {
        let labels: Vec<&str> = value.split('.').collect();
        labels.len() >= 2 && labels.iter().all(|label| is_dns_label(label))
    }
}

validated_string!(Hostname, "hostname", "a hostname", is_hostname);

const MAX_IPV4_LENGTH: usize = 15;

fn is_ipv4(value: &str) -> bool {
    value.len() <= MAX_IPV4_LENGTH
        && value.split('.').count() == 4
        && value
            .split('.')
            .all(|octet| !octet.is_empty() && octet.len() <= 3 && octet.parse::<u8>().is_ok())
        && value
            .split('.')
            .all(|octet| octet == "0" || !octet.starts_with('0'))
}

validated_string!(Ipv4Address, "address", "an IPv4 address", is_ipv4);

impl Ipv4Address {
    pub fn addr(&self) -> Ipv4Addr {
        self.0.parse().expect("validated on construction")
    }
}

impl From<Ipv4Addr> for Ipv4Address {
    fn from(value: Ipv4Addr) -> Self {
        Self(value.to_string())
    }
}

const MAX_OBJECT_KEY_LENGTH: usize = 1024;

validated_string!(
    /// Key within a bucket. Which bucket is deploy configuration, not protocol.
    ObjectKey,
    "object key",
    "between 1 and 1024 characters",
    |value| !value.is_empty() && value.len() <= MAX_OBJECT_KEY_LENGTH
);

const MAX_FILENAME_LENGTH: usize = 127;

/// `^[0-9A-Za-z][0-9A-Za-z._-]{0,126}$`: a single path segment and nothing else. Requiring the
/// first character to be alphanumeric excludes `.`, `..` and a leading dash that would read as a
/// flag to whatever unpacks it.
fn is_filename(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphanumeric())
        && value.len() <= MAX_FILENAME_LENGTH
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

validated_string!(
    /// One path segment, safe to use as a name inside an archive.
    Filename,
    "filename",
    "one path segment",
    is_filename
);

pub const MAX_STATE_MESSAGE_LENGTH: usize = 512;

/// Operator-facing detail about why something is in the state it is in. Never the tenant's own
/// output. Built rather than parsed, so it is cut to the wire's ceiling instead of refused.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StateMessage(String);

impl StateMessage {
    pub fn new(message: impl Into<String>) -> Self {
        let message: String = message.into();
        Self(truncate_chars(message, MAX_STATE_MESSAGE_LENGTH))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for StateMessage {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl fmt::Display for StateMessage {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for StateMessage {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for StateMessage {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

pub fn truncate_chars(mut value: String, max_chars: usize) -> String {
    if let Some((index, _)) = value.char_indices().nth(max_chars) {
        value.truncate(index);
    }
    value
}

macro_rules! port {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "u32", into = "u32")]
        pub struct $name(u16);

        impl $name {
            pub const fn new(port: u16) -> Result<Self, InvalidValue> {
                if port == 0 {
                    Err(InvalidValue(String::new()))
                } else {
                    Ok(Self(port))
                }
            }

            pub const fn get(self) -> u16 {
                self.0
            }
        }

        impl TryFrom<u32> for $name {
            type Error = InvalidValue;
            fn try_from(value: u32) -> Result<Self, Self::Error> {
                u16::try_from(value)
                    .ok()
                    .filter(|port| *port > 0)
                    .map(Self)
                    .ok_or_else(|| InvalidValue::new(stringify!($name), "a port between 1 and 65535"))
            }
        }

        impl From<$name> for u32 {
            fn from(value: $name) -> u32 {
                value.0 as u32
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

port!(
    /// HTTP port the tenant binary listens on inside the guest. Branded apart from the host port
    /// so that assigning one to the other is a type error rather than a routing bug.
    HttpPort
);
port!(
    /// Port on the app host that forwards to an instance. Allocated by the host.
    HostPort
);

pub const DEFAULT_HTTP_PORT: HttpPort = HttpPort(3000);

pub const MAX_SECRET_LENGTH: usize = 32_768;

/// A tenant's own value, which is a secret wherever it is typed. Serialised in full, because it
/// travels to the config drive; never printed, because `Debug` is what ends up in a log line.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SecretString(String);

pub const REDACTED: &str = "[redacted]";

impl SecretString {
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidValue> {
        let value: String = value.into();
        if value.len() > MAX_SECRET_LENGTH {
            return Err(InvalidValue::new("secret", "within the length limit"));
        }
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SecretString {
    type Error = InvalidValue;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<SecretString> for String {
    fn from(value: SecretString) -> String {
        value.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        f.write_str(REDACTED)
    }
}
