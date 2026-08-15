//! FFI-facing mirror types.
//!
//! `dataline-core`'s ids (`DatabaseId`, `FieldId`, `Link`, `ReferenceId`) wrap
//! `uuid::Uuid`, and its `Record`/`Value` carry `chrono` timestamps and
//! dates — none of that is a UniFFI-native type. Rather than teach UniFFI
//! about `Uuid`/`chrono` via custom-type converters, every id, content hash,
//! and timestamp crosses the boundary as a plain `String` (ids via their
//! existing `Display` impl, which is already the UUID string; dates as
//! `"YYYY-MM-DD"`, timestamps as RFC3339). That matches how BaseLine's own
//! Swift models already key everything off `UUID`, which parses trivially
//! from a string on the Swift side. Store's methods do the parsing at the
//! boundary — see `store.rs`.

use std::collections::HashMap;

use crate::error::DataLineError;

#[derive(uniffi::Record, Clone, Debug)]
pub struct SelectionOption {
    pub name: String,
    pub color: String,
}

#[derive(uniffi::Enum, Clone, Debug)]
pub enum FieldKind {
    Text,
    Number,
    Boolean,
    Selection { multi: bool, options: Vec<SelectionOption> },
    Date,
    /// `target_database` is a database id string. `paired_field` is ignored
    /// on the way in — `dataline-core::SchemaRegistry::add_field` always
    /// establishes pairing itself, the same way it ignores a caller-supplied
    /// value for this today (see that method's tests).
    Reference { target_database: String, paired_field: Option<String> },
    Blob,
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct FieldDefinition {
    pub id: String,
    pub name: String,
    pub kind: FieldKind,
    pub ordering: i32,
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct Database {
    pub id: String,
    pub name: String,
    pub fields: Vec<FieldDefinition>,
}

#[derive(uniffi::Enum, Clone, Debug)]
pub enum Value {
    Text(String),
    Number(f64),
    Boolean(bool),
    Selection(Vec<String>),
    /// `"YYYY-MM-DD"`.
    Date(String),
    /// Links, as id strings.
    Reference(Vec<String>),
    /// A blob's content hash, as returned by `Store::write_blob`.
    Blob(String),
    Empty,
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct Record {
    pub link: String,
    pub database_id: String,
    pub values: HashMap<String, Value>,
    /// RFC3339.
    pub created_at: String,
    /// RFC3339.
    pub updated_at: String,
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct Reference {
    pub id: String,
    pub source_link: String,
    pub source_field: String,
    pub target_link: String,
    pub target_field: String,
}

/// Every link whose value was cleared by a `retypeField` call because it
/// wasn't representable under the new kind — never silent data loss, per
/// Architecture.md §7.
#[derive(uniffi::Record, Clone, Debug)]
pub struct RetypeReport {
    pub cleared_links: Vec<String>,
}

pub(crate) fn retype_report_from_core(report: dataline_core::RetypeReport) -> RetypeReport {
    RetypeReport { cleared_links: report.cleared_links.iter().map(|l| l.to_string()).collect() }
}

pub(crate) fn selection_option_from_core(o: dataline_core::SelectionOption) -> SelectionOption {
    SelectionOption { name: o.name, color: o.color }
}

pub(crate) fn selection_option_to_core(o: SelectionOption) -> dataline_core::SelectionOption {
    dataline_core::SelectionOption { name: o.name, color: o.color }
}

pub(crate) fn field_kind_from_core(kind: dataline_core::FieldKind) -> FieldKind {
    match kind {
        dataline_core::FieldKind::Text => FieldKind::Text,
        dataline_core::FieldKind::Number => FieldKind::Number,
        dataline_core::FieldKind::Boolean => FieldKind::Boolean,
        dataline_core::FieldKind::Date => FieldKind::Date,
        dataline_core::FieldKind::Blob => FieldKind::Blob,
        dataline_core::FieldKind::Selection { multi, options } => {
            FieldKind::Selection { multi, options: options.into_iter().map(selection_option_from_core).collect() }
        }
        dataline_core::FieldKind::Reference { target_database, paired_field } => FieldKind::Reference {
            target_database: target_database.to_string(),
            paired_field: paired_field.map(|f| f.to_string()),
        },
    }
}

pub(crate) fn field_kind_to_core(kind: FieldKind) -> Result<dataline_core::FieldKind, DataLineError> {
    Ok(match kind {
        FieldKind::Text => dataline_core::FieldKind::Text,
        FieldKind::Number => dataline_core::FieldKind::Number,
        FieldKind::Boolean => dataline_core::FieldKind::Boolean,
        FieldKind::Date => dataline_core::FieldKind::Date,
        FieldKind::Blob => dataline_core::FieldKind::Blob,
        FieldKind::Selection { multi, options } => dataline_core::FieldKind::Selection {
            multi,
            options: options.into_iter().map(selection_option_to_core).collect(),
        },
        FieldKind::Reference { target_database, .. } => dataline_core::FieldKind::Reference {
            target_database: crate::store::parse_database_id(&target_database)?,
            // Always None going in — dataline-core establishes pairing itself.
            paired_field: None,
        },
    })
}

pub(crate) fn field_definition_from_core(f: dataline_core::FieldDefinition) -> FieldDefinition {
    FieldDefinition { id: f.id.to_string(), name: f.name, kind: field_kind_from_core(f.kind), ordering: f.ordering }
}

pub(crate) fn database_from_core(db: &dataline_core::Database) -> Database {
    Database {
        id: db.id.to_string(),
        name: db.name.clone(),
        fields: db.fields.iter().cloned().map(field_definition_from_core).collect(),
    }
}

pub(crate) fn value_from_core(value: dataline_core::Value) -> Value {
    match value {
        dataline_core::Value::Text(s) => Value::Text(s),
        dataline_core::Value::Number(n) => Value::Number(n),
        dataline_core::Value::Boolean(b) => Value::Boolean(b),
        dataline_core::Value::Selection(names) => Value::Selection(names),
        dataline_core::Value::Date(d) => Value::Date(d.to_string()),
        dataline_core::Value::Reference(links) => Value::Reference(links.iter().map(|l| l.to_string()).collect()),
        dataline_core::Value::Blob(blob_ref) => Value::Blob(blob_ref.hash().to_string()),
        dataline_core::Value::Empty => Value::Empty,
    }
}

pub(crate) fn value_to_core(value: Value) -> Result<dataline_core::Value, DataLineError> {
    Ok(match value {
        Value::Text(s) => dataline_core::Value::Text(s),
        Value::Number(n) => dataline_core::Value::Number(n),
        Value::Boolean(b) => dataline_core::Value::Boolean(b),
        Value::Selection(names) => dataline_core::Value::Selection(names),
        Value::Date(s) => {
            let date = s.parse().map_err(|_| DataLineError::Storage { message: format!("invalid date: {s}") })?;
            dataline_core::Value::Date(date)
        }
        Value::Reference(links) => {
            let links =
                links.iter().map(|l| crate::store::parse_link(l)).collect::<Result<Vec<_>, DataLineError>>()?;
            dataline_core::Value::Reference(links)
        }
        // Only ever reachable via a value round-tripped from `get_record` and
        // set back through `set_value` — `set_value` itself always rejects a
        // Blob field's value unless it came from `write_blob`, but that
        // check lives in dataline-core, not here.
        Value::Blob(hash) => dataline_core::Value::Blob(crate::store::blob_ref_from_hash(hash)),
        Value::Empty => dataline_core::Value::Empty,
    })
}

pub(crate) fn record_from_core(record: dataline_core::Record) -> Record {
    Record {
        link: record.link.to_string(),
        database_id: record.database_id.to_string(),
        values: record.values.into_iter().map(|(k, v)| (k.to_string(), value_from_core(v))).collect(),
        created_at: record.created_at.to_rfc3339(),
        updated_at: record.updated_at.to_rfc3339(),
    }
}

pub(crate) fn reference_from_core(r: dataline_core::Reference) -> Reference {
    Reference {
        id: r.id.to_string(),
        source_link: r.source_link.to_string(),
        source_field: r.source_field.to_string(),
        target_link: r.target_link.to_string(),
        target_field: r.target_field.to_string(),
    }
}
