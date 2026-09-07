//! What this daemon decides, as opposed to what it does about it.
//!
//! Everything here reasons: the reconcile pass that turns a document and an observation into work,
//! the waker that brings an app back for a request, the health machine, the report builder, and
//! the export and browse paths. None of it touches the host directly — it acts through the traits
//! in `ports`, which is what lets the whole of it be tested on a machine with no kernel.
//!
//! Persistence is the same arrangement one layer down: `repositories` is the only place SQL is
//! written, and nothing here knows there is a database.

pub mod backoff;
pub mod exports;
pub mod filesystem;
pub mod health;
pub mod reconcile;
pub mod report;
pub mod waker;
