use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use protocol::{HranaValue, SQLITE_MAX_VALUE_BYTES};
use rusqlite::types::{Value, ValueRef};

use crate::QueryError;

fn mismatch() -> QueryError {
    QueryError::new(
        "SQLITE_MISMATCH",
        "The SQLite value cannot be represented in Hrana.",
    )
}

pub(crate) fn argument(value: &HranaValue) -> Result<Value, QueryError> {
    match value {
        HranaValue::Null => Ok(Value::Null),
        HranaValue::Integer { value } => value.parse().map(Value::Integer).map_err(|_| mismatch()),
        HranaValue::Float { value } if value.is_finite() => Ok(Value::Real(*value)),
        HranaValue::Text { value } if value.len() <= SQLITE_MAX_VALUE_BYTES => Ok(Value::Text(value.clone())),
        HranaValue::Blob { base64 } if base64.len() <= SQLITE_MAX_VALUE_BYTES.div_ceil(3) * 4 => {
            let bytes = STANDARD.decode(base64).map_err(|_| mismatch())?;
            if bytes.len() > SQLITE_MAX_VALUE_BYTES {
                return Err(QueryError::too_large());
            }
            Ok(Value::Blob(bytes))
        }
        _ => Err(mismatch()),
    }
}

pub(crate) fn measured(value: ValueRef<'_>) -> Result<HranaValue, QueryError> {
    match value {
        ValueRef::Null => Ok(HranaValue::Null),
        ValueRef::Integer(value) => Ok(HranaValue::Integer {
            value: value.to_string(),
        }),
        ValueRef::Real(value) if value.is_finite() => Ok(HranaValue::Float { value }),
        ValueRef::Text(bytes) if bytes.len() <= SQLITE_MAX_VALUE_BYTES => Ok(HranaValue::Text {
            value: std::str::from_utf8(bytes).map_err(|_| mismatch())?.into(),
        }),
        ValueRef::Blob(bytes) if bytes.len() <= SQLITE_MAX_VALUE_BYTES => Ok(HranaValue::Blob {
            base64: STANDARD.encode(bytes),
        }),
        _ => Err(mismatch()),
    }
}
