//! Mirrors `dataline_core::DataLineError`, with every id/hash carried as a
//! plain `String` (via the core error's `Display` impl) instead of the core
//! id types — same reasoning as `types.rs`.

#[derive(uniffi::Error, Debug, Clone, PartialEq, Eq)]
pub enum DataLineError {
    DatabaseNotFound { id: String },
    FieldNotFound { id: String },
    RecordNotFound { id: String },
    ReferenceNotFound { id: String },
    BlobNotFound { hash: String },
    ValueKindMismatch { field: String, expected: String },
    UnsupportedFieldKindForValue { field: String, kind: String },
    ReferenceTargetDatabaseMismatch { field: String, expected: String, actual: String },
    ReferenceFieldRetypeNotSupported { field: String },
    Storage { message: String },
}

impl std::fmt::Display for DataLineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DatabaseNotFound { id } => write!(f, "database not found: {id}"),
            Self::FieldNotFound { id } => write!(f, "field not found: {id}"),
            Self::RecordNotFound { id } => write!(f, "record not found: {id}"),
            Self::ReferenceNotFound { id } => write!(f, "reference not found: {id}"),
            Self::BlobNotFound { hash } => write!(f, "blob not found: {hash}"),
            Self::ValueKindMismatch { field, expected } => {
                write!(f, "value for field '{field}' does not match its kind (expected {expected})")
            }
            Self::UnsupportedFieldKindForValue { field, kind } => {
                write!(f, "field '{field}' has kind '{kind}', which record value operations don't support yet")
            }
            Self::ReferenceTargetDatabaseMismatch { field, expected, actual } => {
                write!(f, "field '{field}' points at database {expected}, but the given record belongs to {actual}")
            }
            Self::ReferenceFieldRetypeNotSupported { field } => {
                write!(f, "field '{field}' is a reference field; retyping is only supported between scalar kinds")
            }
            Self::Storage { message } => write!(f, "storage error: {message}"),
        }
    }
}

impl From<dataline_core::DataLineError> for DataLineError {
    fn from(err: dataline_core::DataLineError) -> Self {
        use dataline_core::DataLineError as Core;
        match err {
            Core::DatabaseNotFound(id) => Self::DatabaseNotFound { id: id.to_string() },
            Core::FieldNotFound(id) => Self::FieldNotFound { id: id.to_string() },
            Core::RecordNotFound(id) => Self::RecordNotFound { id: id.to_string() },
            Core::ReferenceNotFound(id) => Self::ReferenceNotFound { id: id.to_string() },
            Core::BlobNotFound(blob_ref) => Self::BlobNotFound { hash: blob_ref.hash().to_string() },
            Core::ValueKindMismatch { field, expected } => {
                Self::ValueKindMismatch { field: field.to_string(), expected: expected.to_string() }
            }
            Core::UnsupportedFieldKindForValue { field, kind } => {
                Self::UnsupportedFieldKindForValue { field: field.to_string(), kind: kind.to_string() }
            }
            Core::ReferenceTargetDatabaseMismatch { field, expected, actual } => Self::ReferenceTargetDatabaseMismatch {
                field: field.to_string(),
                expected: expected.to_string(),
                actual: actual.to_string(),
            },
            Core::ReferenceFieldRetypeNotSupported(field) => {
                Self::ReferenceFieldRetypeNotSupported { field: field.to_string() }
            }
            Core::Storage(message) => Self::Storage { message },
        }
    }
}
