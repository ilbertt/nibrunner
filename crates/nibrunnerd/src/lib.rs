//! One daemon that turns a Linux machine with `/dev/kvm` into a nibrun app host.
//!
//! Level-triggered and never commanded: the daemon converges on a `HostDesiredState` document
//! and reports what it observes. There is no start or stop endpoint. The only local verbs are
//! `wake` and a filesystem read, both non-durable reflexes.

pub mod api;
pub mod backoff;
pub mod clock;
pub mod config;
pub mod control;
pub mod health;
pub mod json_store;
pub mod logs;
pub mod net;
pub mod proxy;
pub mod reconcile;
pub mod report;
pub mod services;
pub mod state;
#[cfg(test)]
pub mod test_support;
pub mod vm;
pub mod volumes;
