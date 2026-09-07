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

//! # How this crate is arranged
//!
//! Five layers, and the direction of every dependency between them is downwards:
//!
//! | | |
//! | --- | --- |
//! | `controllers` | The loops. They own timing and nothing else. |
//! | `services` | What this daemon decides: the reconcile pass, the waker, health, exports, the browse. |
//! | `repositories` | The only place SQL is written. |
//! | `ports` | The traits everything above acts through, and the recording doubles a test fills them with. |
//! | `adapters` | What fills those traits on a real machine: the hypervisor, the kernel's tables, a device, a store. |
//!
//! `host` is the aggregate the layers are handed, `state` is what it holds in memory, and `config`
//! is what it was told at startup. Nothing in `services` knows there is a database, and nothing in
//! it touches the host except through `ports` — which is why the whole of this daemon's reasoning
//! is exercised by tests on a laptop with no kernel, no hypervisor and no network.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::panic, clippy::expect_used))]

pub mod adapters;
pub mod clock;
pub mod config;
pub mod control;
pub mod controllers;
pub mod desired;
pub mod host;
pub mod json_store;
pub mod ports;
pub mod repositories;
pub mod run;
pub mod services;
pub mod state;
#[cfg(test)]
pub mod test_support;

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
