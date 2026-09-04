//! One reading of the wall clock, so a test can hold it still.

use protocol::Timestamp;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn now_timestamp() -> Timestamp {
    Timestamp::from_epoch_ms(now_ms())
}
