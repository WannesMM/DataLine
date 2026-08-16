use std::collections::HashMap;
use std::fmt;

use chrono::{DateTime, NaiveDate, Utc};

use crate::ids::{DatabaseId, FieldId, Link};

/// A record's field values (Architecture.md §2). `Reference` values are read
/// and written through the reference resolution API, not through `set_value`
/// — a Reference field never gets a column on its owning table (§3), so
/// there's no raw column value to decode here.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Text(String),
    Number(f64),
    Boolean(bool),
    Selection(Vec<String>),
    Date(NaiveDate),
    Reference(Vec<Link>),
    Blob(BlobRef),
    /// Arbitrary structured data the engine stores and returns verbatim
    /// without interpreting it — the raw text of a JSON document (validated
    /// as parseable JSON on write, see `Store::set_value`), not a specific
    /// schema. For a caller-defined value shape with no single DataLine
    /// scalar kind that fits it — a generic escape hatch meant to be useful
    /// beyond any one consumer, not a BaseLine-specific accommodation.
    Json(String),
    Empty,
}

/// Content-addressed handle into the store's blob table (Architecture.md §5)
/// — the SHA-256 hash of the blob's bytes, hex-encoded, plus the filename it
/// was written with (if any — content, not filename, is what's deduplicated
/// by the hash, so two writes of identical bytes under different filenames
/// share one blob row and keep whichever filename was stored first).
/// Normally produced by `Store::write_blob`, whose caller already holds a
/// fresh, valid one; but a hash also has to survive round-tripping through
/// an external boundary (e.g. dataline-ffi's Swift bindings, which can't
/// share Rust's type-level guarantees across FFI) and come back as a
/// `BlobRef` a caller can hand to `set_value`. `from_hash`/
/// `from_hash_and_filename` accept any string, so the guarantee against a
/// dangling reference has to — and does — live in `Store::set_value` itself,
/// which checks the hash actually exists in the store's `blobs` table before
/// accepting it, rather than in this type's constructors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    pub(crate) hash: String,
    pub(crate) filename: Option<String>,
}

impl BlobRef {
    pub fn from_hash(hash: impl Into<String>) -> Self {
        Self { hash: hash.into(), filename: None }
    }

    pub fn from_hash_and_filename(hash: impl Into<String>, filename: Option<String>) -> Self {
        Self { hash: hash.into(), filename }
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }
}

impl fmt::Display for BlobRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.hash)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub link: Link,
    pub database_id: DatabaseId,
    pub values: HashMap<FieldId, Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
