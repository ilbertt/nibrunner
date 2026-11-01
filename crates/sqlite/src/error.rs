use protocol::{HranaError, SQLITE_MAX_ERROR_BYTES};
use rusqlite::ffi;

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct QueryError {
    pub code: &'static str,
    pub message: String,
}

impl QueryError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        let mut message = message.into();
        let mut end = message.len().min(SQLITE_MAX_ERROR_BYTES);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        Self { code, message }
    }

    pub fn wire(&self) -> HranaError {
        HranaError {
            code: Some(self.code.into()),
            message: self.message.clone(),
        }
    }

    pub fn too_large() -> Self {
        Self::new("SQLITE_TOOBIG", "The query exceeds this guest's resource limits.")
    }

    pub fn protocol() -> Self {
        Self::new("PROTO_ERROR", "The SQLite request does not match its stream.")
    }
}

impl From<rusqlite::Error> for QueryError {
    fn from(error: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(failure, message) = error {
            let code = match failure.extended_code & 0xff {
                ffi::SQLITE_READONLY => "SQLITE_READONLY",
                ffi::SQLITE_AUTH => "SQLITE_AUTH",
                ffi::SQLITE_CANTOPEN => "SQLITE_CANTOPEN",
                ffi::SQLITE_NOTADB => "SQLITE_NOTADB",
                ffi::SQLITE_INTERRUPT => "SQLITE_INTERRUPT",
                ffi::SQLITE_TOOBIG => "SQLITE_TOOBIG",
                ffi::SQLITE_RANGE => "SQLITE_RANGE",
                ffi::SQLITE_MISMATCH => "SQLITE_MISMATCH",
                ffi::SQLITE_NOMEM => "SQLITE_NOMEM",
                ffi::SQLITE_BUSY | ffi::SQLITE_LOCKED => "SQLITE_BUSY",
                _ => "SQLITE_ERROR",
            };
            return Self::new(code, message.unwrap_or_else(|| failure.to_string()));
        }
        Self::protocol()
    }
}
