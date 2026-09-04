//! Tenant stdout and stderr, from the guest's own vsock connection to wherever they are kept.

pub mod file_sink;
pub mod receiver;

pub use file_sink::FileLogSink;
