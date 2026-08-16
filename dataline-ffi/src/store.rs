//! The FFI-exposed `Store` object. Every method here does the same three
//! things, in order: parse incoming id/hash strings into `dataline-core`
//! types, delegate to the matching `dataline-core::Store` method, convert
//! the result back into an FFI-facing type (`types.rs`) or a plain
//! string/primitive. No business logic lives here — see the module docs in
//! `lib.rs`.
//!
//! `dataline_core::Store`'s mutating methods take `&mut self`, but a UniFFI
//! `Object` is always shared behind `Arc<Self>` — Swift can call into it
//! from anywhere, concurrently. The `Mutex` is what makes `&self` methods
//! here able to mutate the wrapped store safely; it also means calls
//! serialize against each other rather than running in parallel, which is
//! the same constraint a single SQLite connection already implies.

use std::sync::{Arc, Mutex};

use uuid::Uuid;

use crate::error::DataLineError;
use crate::types::{self, Database, FieldKind, Record, Reference, RetypeReport, Value};

pub(crate) fn parse_uuid(s: &str) -> Result<Uuid, DataLineError> {
    Uuid::parse_str(s).map_err(|e| DataLineError::Storage { message: format!("invalid id '{s}': {e}") })
}

pub(crate) fn parse_database_id(s: &str) -> Result<dataline_core::DatabaseId, DataLineError> {
    Ok(dataline_core::DatabaseId::from_uuid(parse_uuid(s)?))
}

pub(crate) fn parse_field_id(s: &str) -> Result<dataline_core::FieldId, DataLineError> {
    Ok(dataline_core::FieldId::from_uuid(parse_uuid(s)?))
}

pub(crate) fn parse_link(s: &str) -> Result<dataline_core::Link, DataLineError> {
    Ok(dataline_core::Link::from_uuid(parse_uuid(s)?))
}

pub(crate) fn parse_reference_id(s: &str) -> Result<dataline_core::ReferenceId, DataLineError> {
    Ok(dataline_core::ReferenceId::from_uuid(parse_uuid(s)?))
}

pub(crate) fn blob_ref_from_hash(hash: String, filename: Option<String>) -> dataline_core::BlobRef {
    dataline_core::BlobRef::from_hash_and_filename(hash, filename)
}

#[derive(uniffi::Object)]
pub struct Store {
    inner: Mutex<dataline_core::Store>,
}

#[uniffi::export]
impl Store {
    #[uniffi::constructor]
    pub fn open(path: String) -> Result<Arc<Self>, DataLineError> {
        let store = dataline_core::Store::open(path)?;
        Ok(Arc::new(Self { inner: Mutex::new(store) }))
    }

    /// Flushes the WAL back into the main database file — call before
    /// copying the store file for a backup snapshot, so the copy is
    /// self-consistent without also needing the `-wal`/`-shm` sidecars.
    pub fn checkpoint(&self) -> Result<(), DataLineError> {
        let inner = self.inner.lock().unwrap();
        inner.checkpoint()?;
        Ok(())
    }

    // ---- Schema management ----

    pub fn create_database(&self, name: String) -> Result<String, DataLineError> {
        let mut inner = self.inner.lock().unwrap();
        Ok(inner.create_database(name)?.to_string())
    }

    pub fn duplicate_database_schema(&self, database_id: String, new_name: String) -> Result<String, DataLineError> {
        let mut inner = self.inner.lock().unwrap();
        Ok(inner.duplicate_database_schema(parse_database_id(&database_id)?, new_name)?.to_string())
    }

    pub fn delete_database(&self, database_id: String) -> Result<(), DataLineError> {
        let mut inner = self.inner.lock().unwrap();
        inner.delete_database(parse_database_id(&database_id)?)?;
        Ok(())
    }

    pub fn add_field(&self, database_id: String, name: String, kind: FieldKind) -> Result<String, DataLineError> {
        let mut inner = self.inner.lock().unwrap();
        let kind = types::field_kind_to_core(kind)?;
        Ok(inner.add_field(parse_database_id(&database_id)?, name, kind)?.to_string())
    }

    pub fn rename_field(&self, database_id: String, field_id: String, name: String) -> Result<(), DataLineError> {
        let mut inner = self.inner.lock().unwrap();
        inner.rename_field(parse_database_id(&database_id)?, parse_field_id(&field_id)?, name)?;
        Ok(())
    }

    pub fn remove_field(&self, database_id: String, field_id: String) -> Result<(), DataLineError> {
        let mut inner = self.inner.lock().unwrap();
        inner.remove_field(parse_database_id(&database_id)?, parse_field_id(&field_id)?)?;
        Ok(())
    }

    pub fn retype_field(
        &self,
        database_id: String,
        field_id: String,
        kind: FieldKind,
    ) -> Result<RetypeReport, DataLineError> {
        let mut inner = self.inner.lock().unwrap();
        let kind = types::field_kind_to_core(kind)?;
        let report = inner.retype_field(parse_database_id(&database_id)?, parse_field_id(&field_id)?, kind)?;
        Ok(types::retype_report_from_core(report))
    }

    pub fn get_database(&self, database_id: String) -> Result<Database, DataLineError> {
        let inner = self.inner.lock().unwrap();
        Ok(types::database_from_core(inner.get_database(parse_database_id(&database_id)?)?))
    }

    pub fn list_databases(&self) -> Vec<Database> {
        let inner = self.inner.lock().unwrap();
        inner.list_databases().into_iter().map(types::database_from_core).collect()
    }

    // ---- Record operations ----

    pub fn create_record(&self, database_id: String) -> Result<String, DataLineError> {
        let mut inner = self.inner.lock().unwrap();
        Ok(inner.create_record(parse_database_id(&database_id)?)?.to_string())
    }

    pub fn get_record(&self, link: String) -> Result<Record, DataLineError> {
        let inner = self.inner.lock().unwrap();
        Ok(types::record_from_core(inner.get_record(parse_link(&link)?)?))
    }

    pub fn set_value(&self, link: String, field_id: String, value: Value) -> Result<(), DataLineError> {
        let mut inner = self.inner.lock().unwrap();
        let value = types::value_to_core(value)?;
        inner.set_value(parse_link(&link)?, parse_field_id(&field_id)?, value)?;
        Ok(())
    }

    pub fn delete_record(&self, link: String) -> Result<(), DataLineError> {
        let mut inner = self.inner.lock().unwrap();
        inner.delete_record(parse_link(&link)?)?;
        Ok(())
    }

    pub fn list_records(&self, database_id: String) -> Result<Vec<Record>, DataLineError> {
        let inner = self.inner.lock().unwrap();
        let records = inner.list_records(parse_database_id(&database_id)?)?;
        Ok(records.into_iter().map(types::record_from_core).collect())
    }

    // ---- Reference resolution ----

    pub fn create_reference(
        &self,
        source_link: String,
        source_field: String,
        target_link: String,
    ) -> Result<Reference, DataLineError> {
        let mut inner = self.inner.lock().unwrap();
        let reference = inner.create_reference(
            parse_link(&source_link)?,
            parse_field_id(&source_field)?,
            parse_link(&target_link)?,
        )?;
        Ok(types::reference_from_core(reference))
    }

    pub fn delete_reference(&self, reference_id: String, field: String) -> Result<(), DataLineError> {
        let mut inner = self.inner.lock().unwrap();
        inner.delete_reference(parse_reference_id(&reference_id)?, parse_field_id(&field)?)?;
        Ok(())
    }

    pub fn list_references(&self, link: String, field: String) -> Result<Vec<Reference>, DataLineError> {
        let inner = self.inner.lock().unwrap();
        let refs = inner.list_references(parse_link(&link)?, parse_field_id(&field)?)?;
        Ok(refs.into_iter().map(types::reference_from_core).collect())
    }

    // ---- Querying ----

    pub fn find_by_value(
        &self,
        database_id: String,
        field_id: String,
        value: Value,
    ) -> Result<Vec<Record>, DataLineError> {
        let inner = self.inner.lock().unwrap();
        let value = types::value_to_core(value)?;
        let records = inner.find_by_value(parse_database_id(&database_id)?, parse_field_id(&field_id)?, value)?;
        Ok(records.into_iter().map(types::record_from_core).collect())
    }

    pub fn search_text(&self, database_id: String, query: String) -> Result<Vec<Record>, DataLineError> {
        let inner = self.inner.lock().unwrap();
        let records = inner.search_text(parse_database_id(&database_id)?, &query)?;
        Ok(records.into_iter().map(types::record_from_core).collect())
    }

    pub fn related_records(&self, link: String) -> Result<Vec<String>, DataLineError> {
        let inner = self.inner.lock().unwrap();
        let links = inner.related_records(parse_link(&link)?)?;
        Ok(links.into_iter().map(|l| l.to_string()).collect())
    }

    // ---- Blobs ----

    pub fn write_blob(&self, bytes: Vec<u8>, filename: Option<String>) -> Result<String, DataLineError> {
        let mut inner = self.inner.lock().unwrap();
        Ok(inner.write_blob(&bytes, filename.as_deref())?.hash().to_string())
    }

    pub fn read_blob(&self, hash: String) -> Result<Vec<u8>, DataLineError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.read_blob(&blob_ref_from_hash(hash, None))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The bulk of this crate's real verification is the end-to-end Swift
    // smoke test in xcframework/Sources/DataLineSmokeTest — this crate is a
    // thin, mechanical adapter, and that test exercises the actual FFI
    // boundary rather than just Rust-side conversions. This covers the one
    // path that test can't reach: a malformed id string, which a real Swift
    // caller could pass by mistake but which valid usage never produces.
    #[test]
    fn parsing_an_invalid_id_string_errors_clearly_instead_of_panicking() {
        match parse_database_id("not-a-uuid") {
            Err(DataLineError::Storage { message }) => assert!(message.contains("not-a-uuid")),
            other => panic!("expected a Storage error naming the bad id, got {other:?}"),
        }
    }

    #[test]
    fn store_open_on_an_unwritable_path_errors_instead_of_panicking() {
        match Store::open("/nonexistent-directory/store.sqlite".to_string()) {
            Err(DataLineError::Storage { .. }) => {}
            Err(other) => panic!("expected a Storage error, got {other:?}"),
            Ok(_) => panic!("expected opening an unwritable path to fail"),
        }
    }
}
