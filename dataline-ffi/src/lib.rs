//! UniFFI boundary crate (Architecture.md §10, step 8).
//!
//! Wraps `dataline-core` behind a single opaque `Store` object, organized
//! around the four capability groups described in Architecture.md §6. This
//! is deliberately a thin adapter: every method here parses/stringifies ids
//! at the boundary and delegates straight to the matching `dataline-core`
//! method — no logic of its own lives here. See `types.rs` for why ids,
//! hashes, and timestamps all cross the boundary as plain `String`s rather
//! than `dataline-core`'s richer Rust types.

mod error;
mod store;
mod types;

pub use error::DataLineError;
pub use store::Store;
pub use types::{Database, FieldDefinition, FieldKind, Record, Reference, RetypeReport, SelectionOption, Value};

uniffi::setup_scaffolding!();

/// This crate's version (`dataline-ffi`'s own semver, which tracks
/// `dataline-core`'s). Doesn't need a `Store` open — pure identity, callable
/// before touching any store file. Architecture.md §8 calls this out as
/// something Line's future plugin contract will want for free; it costs
/// nothing to expose today even with no Line to hand it to yet.
#[uniffi::export]
pub fn dataline_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
