use thiserror::Error;

use crate::ids::{DatabaseId, FieldId, Link, ReferenceId};
use crate::record::BlobRef;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DataLineError {
    #[error("database not found: {0}")]
    DatabaseNotFound(DatabaseId),
    #[error("field not found: {0}")]
    FieldNotFound(FieldId),
    #[error("record not found: {0}")]
    RecordNotFound(Link),
    #[error("reference not found: {0}")]
    ReferenceNotFound(ReferenceId),
    #[error("blob not found: {0}")]
    BlobNotFound(BlobRef),
    #[error("value for field '{field}' does not match its kind (expected {expected})")]
    ValueKindMismatch { field: FieldId, expected: &'static str },
    #[error("field '{field}' has kind '{kind}', which record value operations don't support yet")]
    UnsupportedFieldKindForValue { field: FieldId, kind: &'static str },
    #[error("field '{field}' points at database {expected}, but the given record belongs to {actual}")]
    ReferenceTargetDatabaseMismatch { field: FieldId, expected: DatabaseId, actual: DatabaseId },
    #[error("field '{0}' is a reference field; retyping is only supported between scalar kinds")]
    ReferenceFieldRetypeNotSupported(FieldId),
    /// Carries `rusqlite::Error`'s message as a plain `String` rather than the
    /// error itself, so `DataLineError` stays comparable (`PartialEq`/`Eq`) —
    /// convenient for tests and for callers that just want to match on kind.
    #[error("storage error: {0}")]
    Storage(String),
}

impl From<rusqlite::Error> for DataLineError {
    fn from(err: rusqlite::Error) -> Self {
        DataLineError::Storage(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, DataLineError>;
