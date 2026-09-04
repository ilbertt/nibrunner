//! One daemon that turns a Linux machine with `/dev/kvm` into a nibrun app host.
//!
//! Level-triggered and never commanded: the daemon converges on a `HostDesiredState` document
//! and reports what it observes. There is no start or stop endpoint, and no command surface at
//! all — the input is one JSON file this daemon watches, the output is one JSON file it writes,
//! and waking is a reflex a request triggers rather than a verb anybody calls.
//!
//! A remote control plane is an addon rather than a second input: it polls its endpoint and
//! writes the same file, so the reconciler still has exactly one source and cannot learn which
//! of them produced the document it converged on.

// A test that unwraps and panics *is* its own failure report, and one written to avoid saying
// so reads worse than the assertion it replaced. The lint stays on for everything else.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::panic, clippy::expect_used))]

pub mod artifact_store;
pub mod backoff;
pub mod clock;
pub mod config;
pub mod control;
pub mod desired;
pub mod exec;
pub mod exports;
pub mod filesystem;
pub mod health;
pub mod host;
pub mod json_store;
pub mod logs;
pub mod net;
pub mod proxy;
pub mod reconcile;
pub mod report;
pub mod run;
pub mod services;
pub mod state;
#[cfg(test)]
pub mod test_support;
pub mod vm;
pub mod volumes;
pub mod waker;

/// rustls is built here without a default cryptography provider, because the one it would pick is
/// aws-lc-rs — whose `aws-lc-sys` needs cmake and a C toolchain for the target, which is what
/// would stop this being a static cross-compiled binary. Ring is installed instead, once, before
/// anything builds a client that would otherwise refuse to be built at all.
pub fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
