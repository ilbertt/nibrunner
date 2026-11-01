use protocol::SQLITE_MAX_BODY_BYTES;
use serde::Serialize;

use crate::QueryError;

pub(crate) struct Budget {
    remaining: usize,
}

impl Budget {
    pub(crate) fn new() -> Self {
        Self {
            remaining: SQLITE_MAX_BODY_BYTES,
        }
    }

    pub(crate) fn take(&mut self, bytes: usize) -> Result<(), QueryError> {
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .ok_or_else(QueryError::too_large)?;
        Ok(())
    }

    pub(crate) fn charge(&mut self, value: &impl Serialize) -> Result<(), QueryError> {
        self.take(
            serde_json::to_vec(value)
                .map_err(|_| QueryError::protocol())?
                .len(),
        )
    }
}
