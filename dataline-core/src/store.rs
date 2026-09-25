//! SQLite-backed persistence (Architecture.md §3). `Store` wraps an in-memory
//! [`SchemaRegistry`] with a single open SQLite connection: every schema
//! mutation runs through the same tested registry logic first, then gets
//! diffed against the pre-mutation snapshot and persisted in one transaction.
//! If persistence fails, the in-memory registry is rolled back to the
//! snapshot, so the two can never disagree.
//!
//! Records (this module's other half, Architecture.md §10 step 4) are not
//! mirrored in memory the way schema is — they're expected to scale well
//! past what's reasonable to hold entirely in RAM, so every record operation
//! talks to SQLite directly. A global `links` table maps each record's
//! [`Link`](crate::ids::Link) to the database it lives in, so `get_record`,
//! `set_value`, and `delete_record` can resolve a record from its link alone
//! (Architecture.md §6's API shape), without the caller already knowing
//! which database it came from.
//!
//! Blob values are deliberately not settable yet — that needs the blob table
//! (step 7) this module doesn't build. `set_value` rejects the kind with a
//! clear error rather than silently accepting something it can't actually
//! persist correctly.
//!
//! Reference fields (step 5) get their own join table per relationship —
//! `ref_<hex>`, named after whichever of the pair's two field ids sorts
//! lower, with `link_a`/`link_b` columns holding the two sides' links. That
//! canonical naming (rather than one table per field) is what keeps a single
//! row representing the *whole* bidirectional relationship: deleting it
//! removes both directions at once, and there's nothing to keep in sync.
//! `reference_side` maps a specific field back to whichever physical column
//! (`link_a` or `link_b`) its own database's link sits in for that table.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{DataLineError, Result};
use crate::ids::{DatabaseId, FieldId, Link, ReferenceId};
use crate::record::{BlobRef, Record, Value};
use crate::reference::Reference;
use crate::schema::{Database, FieldDefinition, FieldKind, SchemaRegistry, SelectionOption};

const SCHEMA_VERSION: i64 = 3;

/// What happened to existing records' values when [`Store::retype_field`]
/// changed a field's kind. Per Architecture.md §7, a retype never silently
/// drops data — every link whose value was cleared because it wasn't
/// representable under the new kind is listed here, so the caller can
/// surface that rather than have values quietly vanish.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RetypeReport {
    pub cleared_links: Vec<Link>,
}

pub struct Store {
    conn: Connection,
    registry: SchemaRegistry,
}

impl Store {
    /// Opens a store at `path`, creating it (and its schema tables) if it
    /// doesn't exist yet, or rehydrating the in-memory registry from an
    /// existing store's schema rows otherwise.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // WAL keeps writers and readers from blocking each other and survives a
        // crash mid-write (the WAL replays on next open); NORMAL sync is safe under
        // WAL because a crash can only lose the last few not-yet-checkpointed
        // commits, never corrupt the database file itself.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        if table_exists(&conn, "_dataline_meta")? {
            check_schema_version(&mut conn)?;
        } else {
            create_schema_tables(&conn)?;
        }

        let registry = load_registry(&conn)?;

        Ok(Self { conn, registry })
    }

    /// Flushes the WAL back into the main database file so a plain file copy
    /// of the store (e.g. for a backup snapshot) is self-consistent without
    /// also having to copy the `-wal`/`-shm` sidecar files.
    pub fn checkpoint(&self) -> Result<()> {
        self.conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        Ok(())
    }

    pub fn get_database(&self, id: DatabaseId) -> Result<&Database> {
        self.registry.get_database(id)
    }

    pub fn list_databases(&self) -> Vec<&Database> {
        self.registry.list_databases()
    }

    pub fn create_database(&mut self, name: impl Into<String>) -> Result<DatabaseId> {
        let name = name.into();
        self.with_persisted_mutation(|registry| Ok(registry.create_database(name)))
    }

    pub fn add_field(
        &mut self,
        database_id: DatabaseId,
        name: impl Into<String>,
        kind: FieldKind,
    ) -> Result<FieldId> {
        let name = name.into();
        self.with_persisted_mutation(|registry| registry.add_field(database_id, name, kind))
    }

    pub fn rename_field(
        &mut self,
        database_id: DatabaseId,
        field_id: FieldId,
        name: impl Into<String>,
    ) -> Result<()> {
        let name = name.into();
        self.with_persisted_mutation(|registry| registry.rename_field(database_id, field_id, name))
    }

    pub fn remove_field(&mut self, database_id: DatabaseId, field_id: FieldId) -> Result<()> {
        self.with_persisted_mutation(|registry| registry.remove_field(database_id, field_id))
    }

    /// Changes a field's kind, migrating every existing record's value for
    /// it in the same transaction as the schema change (Architecture.md §7).
    /// Rejects `Reference` on either side — see
    /// `SchemaRegistry::retype_field`'s doc comment for why that's a
    /// separate, unbuilt piece of scope rather than folded in here.
    ///
    /// SQLite's column affinity turned out not to be the purely cosmetic
    /// hint it was assumed to be while designing this (Architecture.md §3):
    /// it actively coerces an inserted value toward a column's declared
    /// affinity — a `TEXT` value written into a `REAL`-affinity column comes
    /// back out as a number, silently undoing `encode_scalar_value`'s
    /// intended representation. So this *does* replace the physical column,
    /// not just its metadata row: SQLite has no `ALTER COLUMN TYPE`, so it
    /// adds a new column with the new affinity, writes every migrated value
    /// into it, drops the old column, and renames the new one into its
    /// place — the field's column name (`field_<uuid>`, keyed by id, not by
    /// kind) ends up unchanged either way.
    pub fn retype_field(
        &mut self,
        database_id: DatabaseId,
        field_id: FieldId,
        new_kind: FieldKind,
    ) -> Result<RetypeReport> {
        let old_kind = {
            let db = self.registry.get_database(database_id)?;
            db.field(field_id).ok_or(DataLineError::FieldNotFound(field_id))?.kind.clone()
        };

        let snapshot = self.registry.clone();
        match self.retype_field_and_migrate(database_id, field_id, &old_kind, new_kind) {
            Ok(report) => Ok(report),
            Err(err) => {
                self.registry = snapshot;
                Err(err)
            }
        }
    }

    fn retype_field_and_migrate(
        &mut self,
        database_id: DatabaseId,
        field_id: FieldId,
        old_kind: &FieldKind,
        new_kind: FieldKind,
    ) -> Result<RetypeReport> {
        // Validates the Reference rejection and swaps the in-memory kind;
        // rolled back by the caller if anything below fails.
        self.registry.retype_field(database_id, field_id, new_kind.clone())?;

        let table = database_table_name(database_id);
        let column = field_column_name(field_id);
        let temp_column = format!("{column}_retype_tmp");
        let new_sql_type = scalar_sql_type(&new_kind)
            .expect("retype_field rejects Reference on either side, so the new kind always has a column type");

        let existing: Vec<(String, rusqlite::types::Value)> = {
            let mut stmt = self.conn.prepare(&format!("SELECT link, \"{column}\" FROM \"{table}\""))?;
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, rusqlite::types::Value>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let tx = self.conn.transaction()?;
        let mut cleared_links = Vec::new();

        tx.execute(&format!("ALTER TABLE \"{table}\" ADD COLUMN \"{temp_column}\" {new_sql_type}"), [])?;

        for (link_str, sql_value) in existing {
            let old_value = decode_scalar_value(old_kind, sql_value)?;
            let new_value = coerce_value(&old_value, &new_kind);
            if !matches!(old_value, Value::Empty) && matches!(new_value, Value::Empty) {
                cleared_links.push(Link::from_uuid(parse_uuid(&link_str)?));
            }
            let encoded = encode_scalar_value(field_id, &new_kind, &new_value)?;
            tx.execute(&format!("UPDATE \"{table}\" SET \"{temp_column}\" = ?1 WHERE link = ?2"), params![
                encoded, link_str
            ])?;
        }

        // SQLite has no ALTER COLUMN TYPE — replace the column instead of
        // altering it in place, then rename the replacement back to the
        // stable, id-keyed name every other query expects.
        tx.execute(&format!("ALTER TABLE \"{table}\" DROP COLUMN \"{column}\""), [])?;
        tx.execute(&format!("ALTER TABLE \"{table}\" RENAME COLUMN \"{temp_column}\" TO \"{column}\""), [])?;

        update_field_kind_row(&tx, field_id, &new_kind)?;
        tx.commit()?;

        Ok(RetypeReport { cleared_links })
    }

    pub fn duplicate_database_schema(
        &mut self,
        database_id: DatabaseId,
        new_name: impl Into<String>,
    ) -> Result<DatabaseId> {
        let new_name = new_name.into();
        self.with_persisted_mutation(|registry| registry.duplicate_database_schema(database_id, new_name))
    }

    /// Removes a database, its records, and every field on any other
    /// database that referenced it — see
    /// [`SchemaRegistry::remove_database`]. Its own record table (and any
    /// `links` rows for records that lived in it) are dropped in the same
    /// transaction as the schema change; join tables for reference fields
    /// it owned or was pointed at by are dropped the same way an ordinary
    /// `remove_field` drops one (`persist_diff`'s field-removal pass covers
    /// both).
    pub fn delete_database(&mut self, database_id: DatabaseId) -> Result<()> {
        self.with_persisted_mutation(|registry| registry.remove_database(database_id))
    }

    /// Creates a new, empty record in `database_id` and returns its `Link`.
    pub fn create_record(&mut self, database_id: DatabaseId) -> Result<Link> {
        self.registry.get_database(database_id)?;

        let link = Link::new();
        let now = Utc::now().to_rfc3339();

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO links (link, database_id) VALUES (?1, ?2)",
            params![link.as_uuid().to_string(), database_id.as_uuid().to_string()],
        )?;
        tx.execute(
            &format!(
                "INSERT INTO \"{}\" (link, created_at, updated_at) VALUES (?1, ?2, ?2)",
                database_table_name(database_id)
            ),
            params![link.as_uuid().to_string(), now],
        )?;
        tx.commit()?;

        Ok(link)
    }

    /// Fetches a record by its link alone — the link's owning database is
    /// resolved via the `links` index, not supplied by the caller. Reference
    /// field values are resolved through their join tables, one query per
    /// field — simple and correct; worth revisiting with a bulk join if it
    /// ever shows up as a real bottleneck, but not before then.
    pub fn get_record(&self, link: Link) -> Result<Record> {
        let database_id = self.resolve_database_id(link)?;
        let (scalar_fields, reference_fields) = self.field_lists(database_id)?;

        let mut columns = vec!["created_at".to_string(), "updated_at".to_string()];
        columns.extend(scalar_fields.iter().map(|f| format!("\"{}\"", field_column_name(f.id))));

        let sql =
            format!("SELECT {} FROM \"{}\" WHERE link = ?1", columns.join(", "), database_table_name(database_id));
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params![link.as_uuid().to_string()])?;
        let row = rows.next()?.ok_or(DataLineError::RecordNotFound(link))?;

        let created_at = parse_rfc3339(&row.get::<_, String>(0)?)?;
        let updated_at = parse_rfc3339(&row.get::<_, String>(1)?)?;

        let mut values = HashMap::new();
        for (i, field) in scalar_fields.iter().enumerate() {
            let sql_value: rusqlite::types::Value = row.get(i + 2)?;
            values.insert(field.id, decode_scalar_value(&field.kind, sql_value)?);
        }
        drop(rows);
        drop(stmt);
        for value in values.values_mut() {
            self.hydrate_blob_filename(value)?;
        }

        for field_id in reference_fields {
            let refs = self.list_references(link, field_id)?;
            values.insert(field_id, Value::Reference(refs.into_iter().map(|r| r.target_link).collect()));
        }

        Ok(Record { link, database_id, values, created_at, updated_at })
    }

    /// Lists every record in `database_id`, ordered by creation time. See
    /// `get_record` for how Reference field values are resolved.
    pub fn list_records(&self, database_id: DatabaseId) -> Result<Vec<Record>> {
        let (scalar_fields, reference_fields) = self.field_lists(database_id)?;

        let mut columns = vec!["link".to_string(), "created_at".to_string(), "updated_at".to_string()];
        columns.extend(scalar_fields.iter().map(|f| format!("\"{}\"", field_column_name(f.id))));

        let sql = format!(
            "SELECT {} FROM \"{}\" ORDER BY created_at",
            columns.join(", "),
            database_table_name(database_id)
        );

        let mut links_and_scalars = Vec::new();
        {
            let mut stmt = self.conn.prepare(&sql)?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let link = Link::from_uuid(parse_uuid(&row.get::<_, String>(0)?)?);
                let created_at = parse_rfc3339(&row.get::<_, String>(1)?)?;
                let updated_at = parse_rfc3339(&row.get::<_, String>(2)?)?;

                let mut values = HashMap::new();
                for (i, field) in scalar_fields.iter().enumerate() {
                    let sql_value: rusqlite::types::Value = row.get(i + 3)?;
                    values.insert(field.id, decode_scalar_value(&field.kind, sql_value)?);
                }
                links_and_scalars.push((link, created_at, updated_at, values));
            }
        }

        let mut records = Vec::with_capacity(links_and_scalars.len());
        for (link, created_at, updated_at, mut values) in links_and_scalars {
            for &field_id in &reference_fields {
                let refs = self.list_references(link, field_id)?;
                values.insert(field_id, Value::Reference(refs.into_iter().map(|r| r.target_link).collect()));
            }
            for value in values.values_mut() {
                self.hydrate_blob_filename(value)?;
            }
            records.push(Record { link, database_id, values, created_at, updated_at });
        }

        Ok(records)
    }

    /// Fills in a decoded `Value::Blob`'s filename from the `blobs` table —
    /// `decode_scalar_value` is a pure function with no database access, so
    /// it can only produce a bare-hash `BlobRef`; this is the one place that
    /// hydrates it into the real thing `get_record`/`list_records` return.
    fn hydrate_blob_filename(&self, value: &mut Value) -> Result<()> {
        match value {
            Value::Blob(blob_ref) => blob_ref.filename = self.blob_filename(&blob_ref.hash)?,
            Value::CustomBlob { blob, .. } => blob.filename = self.blob_filename(&blob.hash)?,
            _ => {}
        }
        Ok(())
    }

    fn field_lists(&self, database_id: DatabaseId) -> Result<(Vec<FieldDefinition>, Vec<FieldId>)> {
        let db = self.registry.get_database(database_id)?;
        let scalar_fields =
            db.fields.iter().filter(|f| scalar_sql_type(&f.kind).is_some()).cloned().collect();
        let reference_fields = db
            .fields
            .iter()
            .filter(|f| matches!(f.kind, FieldKind::Reference { .. }))
            .map(|f| f.id)
            .collect();
        Ok((scalar_fields, reference_fields))
    }

    /// Sets a single field's value on a record. Rejects `Reference` field
    /// kinds outright (see module docs) and rejects a `Value` whose variant
    /// doesn't match the field's kind — a scalar field's SQL column has a
    /// fixed type affinity, so a mismatched value would either fail
    /// confusingly at the SQL layer or silently coerce; neither is
    /// acceptable for data this is supposed to keep reliable.
    ///
    /// A `Value::Blob` is checked against the `blobs` table before being
    /// accepted — `BlobRef::from_hash` can be constructed from any string
    /// (a Rust caller could always bypass a type-only guard anyway, and an
    /// FFI caller has no choice but to reconstruct one from a hash it was
    /// handed), so this is where a dangling blob reference actually gets
    /// caught, not at construction.
    pub fn set_value(&mut self, link: Link, field_id: FieldId, value: Value) -> Result<()> {
        let database_id = self.resolve_database_id(link)?;
        let field = {
            let db = self.registry.get_database(database_id)?;
            db.field(field_id).cloned().ok_or(DataLineError::FieldNotFound(field_id))?
        };

        if let FieldKind::Reference { .. } = &field.kind {
            return Err(DataLineError::UnsupportedFieldKindForValue { field: field_id, kind: kind_label(&field.kind) });
        }
        let blob_ref_to_check: Option<&BlobRef> = match (&field.kind, &value) {
            (FieldKind::Blob, Value::Blob(blob_ref)) => Some(blob_ref),
            (FieldKind::CustomBlob { .. }, Value::CustomBlob { blob, .. }) => Some(blob),
            _ => None,
        };
        if let Some(blob_ref) = blob_ref_to_check {
            let exists: bool = self
                .conn
                .query_row("SELECT EXISTS(SELECT 1 FROM blobs WHERE hash = ?1)", params![blob_ref.hash()], |row| {
                    row.get(0)
                })?;
            if !exists {
                return Err(DataLineError::BlobNotFound(blob_ref.clone()));
            }
        }

        let sql_value = encode_scalar_value(field_id, &field.kind, &value)?;
        let now = Utc::now().to_rfc3339();

        let affected = self.conn.execute(
            &format!(
                "UPDATE \"{}\" SET \"{}\" = ?1, updated_at = ?2 WHERE link = ?3",
                database_table_name(database_id),
                field_column_name(field_id)
            ),
            params![sql_value, now, link.as_uuid().to_string()],
        )?;

        if affected == 0 {
            return Err(DataLineError::RecordNotFound(link));
        }
        Ok(())
    }

    /// Deletes a record by its link alone, first removing every reference
    /// row that touches it — outbound or inbound, on any Reference field.
    /// Every relationship touching this database is guaranteed to have a
    /// field *on* this database representing it (Reference fields are always
    /// auto-paired), so scanning this database's own Reference fields is
    /// enough to find every join table that could hold this link — there's
    /// no relationship reachable only through some other database's schema.
    /// This is the same guarantee BaseLine's own
    /// `ReferenceService.deleteAllReferences` provides today, just enforced
    /// structurally instead of by convention at the call site.
    pub fn delete_record(&mut self, link: Link) -> Result<()> {
        let database_id = self.resolve_database_id(link)?;
        let reference_fields: Vec<(FieldId, FieldId)> = {
            let db = self.registry.get_database(database_id)?;
            db.fields
                .iter()
                .filter_map(|f| match &f.kind {
                    FieldKind::Reference { paired_field: Some(paired), .. } => Some((f.id, *paired)),
                    _ => None,
                })
                .collect()
        };

        let tx = self.conn.transaction()?;
        for (field_id, paired_field_id) in reference_fields {
            let table = reference_table_name(field_id, paired_field_id);
            let column = match reference_side(field_id, paired_field_id) {
                ReferenceSide::A => "link_a",
                ReferenceSide::B => "link_b",
            };
            tx.execute(&format!("DELETE FROM \"{table}\" WHERE {column} = ?1"), params![link.as_uuid().to_string()])?;
        }
        tx.execute(
            &format!("DELETE FROM \"{}\" WHERE link = ?1", database_table_name(database_id)),
            params![link.as_uuid().to_string()],
        )?;
        tx.execute("DELETE FROM links WHERE link = ?1", params![link.as_uuid().to_string()])?;
        tx.commit()?;

        Ok(())
    }

    /// Creates a reference from `source_link` (through `source_field`) to
    /// `target_link`. `source_field`'s paired field is resolved
    /// automatically, so the caller never names the back-reference field
    /// explicitly. Rejects `target_link` outright if it doesn't actually
    /// belong to the database `source_field` points at — a reference into
    /// the wrong database is exactly the kind of silent corruption this
    /// engine exists to prevent.
    pub fn create_reference(&mut self, source_link: Link, source_field: FieldId, target_link: Link) -> Result<Reference> {
        let source_database = self.resolve_database_id(source_link)?;
        let field = {
            let db = self.registry.get_database(source_database)?;
            db.field(source_field).cloned().ok_or(DataLineError::FieldNotFound(source_field))?
        };
        let (target_database, target_field) = reference_pairing(&field)?;

        let actual_target_database = self.resolve_database_id(target_link)?;
        if actual_target_database != target_database {
            return Err(DataLineError::ReferenceTargetDatabaseMismatch {
                field: source_field,
                expected: target_database,
                actual: actual_target_database,
            });
        }

        let reference_id = ReferenceId::new();
        let table = reference_table_name(source_field, target_field);
        let (link_a, link_b) = match reference_side(source_field, target_field) {
            ReferenceSide::A => (source_link, target_link),
            ReferenceSide::B => (target_link, source_link),
        };

        self.conn.execute(
            &format!("INSERT INTO \"{table}\" (id, link_a, link_b) VALUES (?1, ?2, ?3)"),
            params![reference_id.as_uuid().to_string(), link_a.as_uuid().to_string(), link_b.as_uuid().to_string()],
        )?;

        Ok(Reference { id: reference_id, source_link, source_field, target_link, target_field })
    }

    /// Deletes one specific reference relationship. `field` can be either
    /// side of the pair (whichever the caller has on hand — typically
    /// `source_field` or `target_field` from the `Reference` this id came
    /// from); both resolve to the same underlying row.
    pub fn delete_reference(&mut self, reference_id: ReferenceId, field: FieldId) -> Result<()> {
        let target_field = {
            let (_, field_def) = self.registry.find_field(field).ok_or(DataLineError::FieldNotFound(field))?;
            reference_pairing(field_def)?.1
        };
        let table = reference_table_name(field, target_field);

        let affected = self
            .conn
            .execute(&format!("DELETE FROM \"{table}\" WHERE id = ?1"), params![reference_id.as_uuid().to_string()])?;
        if affected == 0 {
            return Err(DataLineError::ReferenceNotFound(reference_id));
        }
        Ok(())
    }

    /// Lists every reference relationship attached to `link` through
    /// `field`. Each result presents `link`/`field` as the source side and
    /// the other end as the target side, regardless of which physical join
    /// table column they happen to live in — that framing is purely
    /// call-site convenience, not something the storage layer tracks.
    pub fn list_references(&self, link: Link, field: FieldId) -> Result<Vec<Reference>> {
        let database_id = self.resolve_database_id(link)?;
        let field_def = {
            let db = self.registry.get_database(database_id)?;
            db.field(field).cloned().ok_or(DataLineError::FieldNotFound(field))?
        };
        let (_, target_field) = reference_pairing(&field_def)?;
        let table = reference_table_name(field, target_field);
        let (my_column, other_column) = match reference_side(field, target_field) {
            ReferenceSide::A => ("link_a", "link_b"),
            ReferenceSide::B => ("link_b", "link_a"),
        };

        let mut stmt =
            self.conn.prepare(&format!("SELECT id, {other_column} FROM \"{table}\" WHERE {my_column} = ?1"))?;
        let rows = stmt.query_map(params![link.as_uuid().to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut references = Vec::new();
        for row in rows {
            let (id_str, other_str) = row?;
            references.push(Reference {
                id: ReferenceId::from_uuid(parse_uuid(&id_str)?),
                source_link: link,
                source_field: field,
                target_link: Link::from_uuid(parse_uuid(&other_str)?),
                target_field,
            });
        }
        Ok(references)
    }

    /// Records in `database_id` whose `field_id` value equals `value`
    /// exactly. Rejects `Reference` fields, same as `set_value` — see its doc
    /// comment for why. A `Blob` field can be searched (useful for "which
    /// records use this exact asset"); the comparison is just hash equality.
    ///
    /// Filters in Rust over `list_records`' full result rather than pushing
    /// the comparison into a SQL `WHERE` clause. Correct and simple; worth
    /// replacing with an indexed lookup if this ever shows up as a real
    /// bottleneck at the "thousands of records" scale the spec targets, but
    /// not before then — see Architecture.md §6 on staying narrow for now.
    pub fn find_by_value(&self, database_id: DatabaseId, field_id: FieldId, value: Value) -> Result<Vec<Record>> {
        let field = {
            let db = self.registry.get_database(database_id)?;
            db.field(field_id).cloned().ok_or(DataLineError::FieldNotFound(field_id))?
        };
        if let FieldKind::Reference { .. } = &field.kind {
            return Err(DataLineError::UnsupportedFieldKindForValue { field: field_id, kind: kind_label(&field.kind) });
        }

        let records = self.list_records(database_id)?;
        Ok(records.into_iter().filter(|r| r.values.get(&field_id) == Some(&value)).collect())
    }

    /// Records in `database_id` with any `Text` field value containing
    /// `query`, case-insensitively — matching BaseLine's own `SearchService`
    /// semantics. Non-text field kinds (including `Selection`, which is
    /// text-shaped but not free text) aren't searched.
    pub fn search_text(&self, database_id: DatabaseId, query: &str) -> Result<Vec<Record>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let needle = query.to_lowercase();

        let records = self.list_records(database_id)?;
        Ok(records
            .into_iter()
            .filter(|r| r.values.values().any(|v| matches!(v, Value::Text(t) if t.to_lowercase().contains(&needle))))
            .collect())
    }

    /// Every record `link` is related to, across *all* of its database's
    /// Reference fields, flattened into one deduplicated list — the coarse,
    /// cross-field counterpart to `list_references`' single-field, structured
    /// result. Matches BaseLine's own `ReferenceService.getRelatedEntries`.
    pub fn related_records(&self, link: Link) -> Result<Vec<Link>> {
        let database_id = self.resolve_database_id(link)?;
        let reference_fields: Vec<FieldId> = {
            let db = self.registry.get_database(database_id)?;
            db.fields.iter().filter(|f| matches!(f.kind, FieldKind::Reference { .. })).map(|f| f.id).collect()
        };

        let mut related = HashSet::new();
        for field_id in reference_fields {
            for reference in self.list_references(link, field_id)? {
                related.insert(reference.target_link);
            }
        }
        Ok(related.into_iter().collect())
    }

    /// Writes `bytes` into the store's content-addressed blob table
    /// (Architecture.md §5) and returns a handle to them, along with
    /// `filename` if given (the file's original name — real per-blob
    /// metadata the engine now carries natively, not something a caller has
    /// to smuggle in through a separate value; see [`Value::Json`]'s doc
    /// comment for the analogous fix on the structured-data side). Writing
    /// the same *content* twice is a no-op the second time regardless of
    /// filename — `BlobRef`s dedupe by content hash, so identical bytes
    /// always produce the identical handle, and `INSERT OR IGNORE` skips the
    /// redundant write; the returned `BlobRef` reflects whichever filename
    /// was actually stored on the first write, which may not be the one
    /// just passed in if this exact content already existed under a
    /// different name.
    pub fn write_blob(&mut self, bytes: &[u8], filename: Option<&str>) -> Result<BlobRef> {
        let hash = sha256_hex(bytes);
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT OR IGNORE INTO blobs (hash, bytes, filename, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![hash, bytes, filename, now],
        )?;
        let stored_filename = self.blob_filename(&hash)?;
        Ok(BlobRef::from_hash_and_filename(hash, stored_filename))
    }

    /// Reads back the bytes behind a [`BlobRef`]. Since a `BlobRef` is only
    /// ever handed out by `write_blob`, a `BlobNotFound` here means the
    /// underlying row was removed some other way, not caller error — there's
    /// no `delete_blob` yet (Architecture.md §5 doesn't call for one; safely
    /// garbage-collecting a *shared*, content-addressed blob needs reference
    /// counting across every record that might point at it, which is real
    /// scope of its own, not something to bolt on here).
    pub fn read_blob(&self, blob_ref: &BlobRef) -> Result<Vec<u8>> {
        self.conn
            .query_row("SELECT bytes FROM blobs WHERE hash = ?1", params![blob_ref.hash], |row| row.get(0))
            .optional()?
            .ok_or_else(|| DataLineError::BlobNotFound(blob_ref.clone()))
    }

    fn blob_filename(&self, hash: &str) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT filename FROM blobs WHERE hash = ?1", params![hash], |row| {
                row.get::<_, Option<String>>(0)
            })
            .optional()
            .map(Option::flatten)
            .map_err(DataLineError::from)
    }

    fn resolve_database_id(&self, link: Link) -> Result<DatabaseId> {
        let id_str: Option<String> = self
            .conn
            .query_row("SELECT database_id FROM links WHERE link = ?1", params![link.as_uuid().to_string()], |row| {
                row.get(0)
            })
            .optional()?;
        let id_str = id_str.ok_or(DataLineError::RecordNotFound(link))?;
        Ok(DatabaseId::from_uuid(parse_uuid(&id_str)?))
    }

    /// Runs `mutate` against the in-memory registry, then persists exactly
    /// what changed. `SchemaRegistry`'s own methods never partially mutate on
    /// error (Architecture.md §2's tests rely on this), so the only case that
    /// needs rolling back is persistence itself failing after a successful
    /// in-memory mutation.
    fn with_persisted_mutation<T>(
        &mut self,
        mutate: impl FnOnce(&mut SchemaRegistry) -> Result<T>,
    ) -> Result<T> {
        let snapshot = self.registry.clone();

        let value = match mutate(&mut self.registry) {
            Ok(value) => value,
            Err(err) => {
                self.registry = snapshot;
                return Err(err);
            }
        };

        match self.persist_diff(&snapshot) {
            Ok(()) => Ok(value),
            Err(err) => {
                self.registry = snapshot;
                Err(err)
            }
        }
    }

    /// Persists the difference between `before` and the current in-memory
    /// registry in a single transaction: new databases (with their record
    /// table), new or changed fields (with a scalar column if applicable),
    /// and fields removed since `before`.
    fn persist_diff(&mut self, before: &SchemaRegistry) -> Result<()> {
        let tx = self.conn.transaction()?;

        for db in self.registry.list_databases() {
            if before.get_database(db.id).is_err() {
                tx.execute("INSERT INTO databases (id, name) VALUES (?1, ?2)", params![db.id.as_uuid().to_string(), db.name])?;
                tx.execute(
                    &format!(
                        "CREATE TABLE \"{}\" (link TEXT PRIMARY KEY, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
                        database_table_name(db.id)
                    ),
                    [],
                )?;
            }
        }

        for db in self.registry.list_databases() {
            let before_db = before.get_database(db.id).ok();
            for field in &db.fields {
                match before_db.and_then(|d| d.field(field.id)) {
                    None => {
                        insert_field_row(&tx, db.id, field)?;
                        match &field.kind {
                            FieldKind::Reference { paired_field: Some(paired), .. } => {
                                // Both sides of a pair land as "new" in the same diff
                                // (self-reference: same database; cross-database: two
                                // databases both touched by this mutation). Creating
                                // the table only from the canonical side means it
                                // happens exactly once regardless of which side is
                                // processed first.
                                if let ReferenceSide::A = reference_side(field.id, *paired) {
                                    create_reference_table(&tx, field.id, *paired)?;
                                }
                            }
                            FieldKind::Reference { paired_field: None, .. } => {
                                // Shouldn't happen — add_field always pairs before
                                // returning — but if it ever did, there's simply no
                                // join table to create until pairing completes.
                            }
                            _ => {
                                if let Some(sql_type) = scalar_sql_type(&field.kind) {
                                    tx.execute(
                                        &format!(
                                            "ALTER TABLE \"{}\" ADD COLUMN \"{}\" {}",
                                            database_table_name(db.id),
                                            field_column_name(field.id),
                                            sql_type
                                        ),
                                        [],
                                    )?;
                                }
                            }
                        }
                    }
                    Some(old) if old != field => {
                        update_field_row(&tx, field)?;
                    }
                    _ => {}
                }
            }
        }

        for db in before.list_databases() {
            let current_db = self.registry.get_database(db.id).ok();
            for field in &db.fields {
                let still_present = current_db.and_then(|d| d.field(field.id)).is_some();
                if !still_present {
                    tx.execute("DELETE FROM fields WHERE id = ?1", params![field.id.as_uuid().to_string()])?;
                    match &field.kind {
                        FieldKind::Reference { paired_field: Some(paired), .. } => {
                            // Same one-shot guard as creation, mirrored for removal.
                            if let ReferenceSide::A = reference_side(field.id, *paired) {
                                tx.execute(
                                    &format!("DROP TABLE IF EXISTS \"{}\"", reference_table_name(field.id, *paired)),
                                    [],
                                )?;
                            }
                        }
                        _ => {
                            if current_db.is_some() && scalar_sql_type(&field.kind).is_some() {
                                tx.execute(
                                    &format!(
                                        "ALTER TABLE \"{}\" DROP COLUMN \"{}\"",
                                        database_table_name(db.id),
                                        field_column_name(field.id)
                                    ),
                                    [],
                                )?;
                            }
                        }
                    }
                }
            }
        }

        // Databases removed since `before` — dropped last, after every field
        // that lived on them (including paired back-references on databases
        // they pointed at) has already been deleted above, so the `fields`
        // FK to `databases` is never violated.
        for db in before.list_databases() {
            if self.registry.get_database(db.id).is_err() {
                tx.execute("DELETE FROM links WHERE database_id = ?1", params![db.id.as_uuid().to_string()])?;
                tx.execute("DELETE FROM databases WHERE id = ?1", params![db.id.as_uuid().to_string()])?;
                tx.execute(&format!("DROP TABLE IF EXISTS \"{}\"", database_table_name(db.id)), [])?;
            }
        }

        tx.commit()?;
        Ok(())
    }
}

fn database_table_name(id: DatabaseId) -> String {
    format!("db_{}", id.as_uuid().simple())
}

fn field_column_name(id: FieldId) -> String {
    format!("field_{}", id.as_uuid().simple())
}

/// Which physical column (`link_a`/`link_b`) a field's own side of a
/// reference pair lands in, in the pair's canonical join table.
enum ReferenceSide {
    A,
    B,
}

/// Orders a field pair deterministically (lower uuid first) so both sides of
/// a relationship agree on one join table and one column layout regardless
/// of which field a caller happens to be looking at.
fn canonical_pair(a: FieldId, b: FieldId) -> (FieldId, FieldId) {
    if a.as_uuid() <= b.as_uuid() {
        (a, b)
    } else {
        (b, a)
    }
}

fn reference_side(field_id: FieldId, paired_field_id: FieldId) -> ReferenceSide {
    if canonical_pair(field_id, paired_field_id).0 == field_id {
        ReferenceSide::A
    } else {
        ReferenceSide::B
    }
}

fn reference_table_name(field_a: FieldId, field_b: FieldId) -> String {
    let (lo, _) = canonical_pair(field_a, field_b);
    format!("ref_{}", lo.as_uuid().simple())
}

fn create_reference_table(tx: &Transaction, field_a: FieldId, field_b: FieldId) -> rusqlite::Result<()> {
    let table = reference_table_name(field_a, field_b);
    tx.execute(
        &format!("CREATE TABLE \"{table}\" (id TEXT PRIMARY KEY, link_a TEXT NOT NULL, link_b TEXT NOT NULL)"),
        [],
    )?;
    tx.execute(&format!("CREATE INDEX \"{table}_link_a\" ON \"{table}\" (link_a)"), [])?;
    tx.execute(&format!("CREATE INDEX \"{table}_link_b\" ON \"{table}\" (link_b)"), [])?;
    Ok(())
}

/// Resolves a Reference field's target database and paired field, or a clear
/// error if `field` isn't a (properly paired) Reference field at all.
fn reference_pairing(field: &FieldDefinition) -> Result<(DatabaseId, FieldId)> {
    match &field.kind {
        FieldKind::Reference { target_database, paired_field: Some(paired) } => Ok((*target_database, *paired)),
        FieldKind::Reference { paired_field: None, .. } => {
            Err(DataLineError::Storage(format!("reference field {} is missing its paired field", field.id)))
        }
        other => Err(DataLineError::UnsupportedFieldKindForValue { field: field.id, kind: kind_label(other) }),
    }
}

/// The SQLite column type for a field's *own* table, or `None` if the kind
/// doesn't get a column there at all (Reference — the relationship lives in a
/// join table instead, added in step 5).
fn scalar_sql_type(kind: &FieldKind) -> Option<&'static str> {
    match kind {
        FieldKind::Text => Some("TEXT"),
        FieldKind::Number => Some("REAL"),
        FieldKind::Boolean => Some("INTEGER"),
        FieldKind::Date => Some("TEXT"),
        FieldKind::Blob => Some("TEXT"), // stores a content hash, not bytes — see Architecture.md §5
        FieldKind::Selection { .. } => Some("TEXT"),
        FieldKind::Json => Some("TEXT"), // stores raw JSON text — see Value::Json
        FieldKind::Custom { .. } => Some("BLOB"), // inline opaque bytes — see Value::Custom
        FieldKind::CustomBlob { .. } => Some("TEXT"), // stores a content hash, not bytes — same as Blob
        FieldKind::Reference { .. } => None,
    }
}

fn kind_label(kind: &FieldKind) -> &'static str {
    match kind {
        FieldKind::Text => "text",
        FieldKind::Number => "number",
        FieldKind::Boolean => "boolean",
        FieldKind::Date => "date",
        FieldKind::Selection { .. } => "selection",
        FieldKind::Blob => "blob",
        FieldKind::Json => "json",
        FieldKind::Custom { .. } => "custom",
        FieldKind::CustomBlob { .. } => "custom_blob",
        FieldKind::Reference { .. } => "reference",
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn create_schema_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE _dataline_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE databases (id TEXT PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE fields (
             id TEXT PRIMARY KEY,
             database_id TEXT NOT NULL REFERENCES databases(id),
             name TEXT NOT NULL,
             kind TEXT NOT NULL,
             selection_multi INTEGER,
             selection_options TEXT,
             reference_target_database TEXT,
             reference_paired_field TEXT,
             custom_kind_id TEXT,
             ordering INTEGER NOT NULL
         );
         CREATE TABLE links (
             link TEXT PRIMARY KEY,
             database_id TEXT NOT NULL REFERENCES databases(id)
         );
         CREATE TABLE blobs (
             hash TEXT PRIMARY KEY,
             bytes BLOB NOT NULL,
             filename TEXT,
             created_at TEXT NOT NULL
         );",
    )?;
    conn.execute(
        "INSERT INTO _dataline_meta (key, value) VALUES ('schema_version', ?1)",
        params![SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

/// One version's upgrade step: takes the store from `from` to `from + 1`.
/// Add a new entry here (and bump `SCHEMA_VERSION`) for every future format
/// change rather than writing a fresh one-off `if` — that's what lets a
/// store several versions behind the current build walk forward through
/// each intermediate step in one open, instead of only the single version
/// jump a hard-coded check could special-case.
type MigrationStep = fn(&Transaction) -> rusqlite::Result<()>;

const MIGRATIONS: &[(i64, MigrationStep)] = &[
    // v1 -> v2: adds `blobs.filename`.
    (1, |tx| tx.execute("ALTER TABLE blobs ADD COLUMN filename TEXT", []).map(|_| ())),
    // v2 -> v3: adds `fields.custom_kind_id`, for FieldKind::Custom.
    (2, |tx| tx.execute("ALTER TABLE fields ADD COLUMN custom_kind_id TEXT", []).map(|_| ())),
];

/// Checks the store's schema version, migrating forward in place through
/// every intermediate step this build knows about (see `MIGRATIONS`) —
/// e.g. a v1 store opened by a build several versions ahead upgrades v1→v2,
/// v2→v3, etc. in one open, all inside a single transaction so a failure
/// partway through never leaves the store on an undocumented in-between
/// version. A version this build has no migration path *to* — either
/// because it's newer than `SCHEMA_VERSION` (an older build opening a newer
/// file) or because a migration step is missing — is a hard error rather
/// than a guess; there's no reason to trust an unknown version is safe to
/// read or write.
fn check_schema_version(conn: &mut Connection) -> Result<()> {
    let value: String = conn
        .query_row("SELECT value FROM _dataline_meta WHERE key = 'schema_version'", [], |row| row.get(0))
        .map_err(DataLineError::from)?;
    let version: i64 =
        value.parse().map_err(|_| DataLineError::Storage(format!("invalid schema_version value: {value}")))?;

    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if version > SCHEMA_VERSION {
        return Err(DataLineError::Storage(format!(
            "store schema version {version} is not supported by this build (expected {SCHEMA_VERSION})"
        )));
    }

    let tx = conn.transaction().map_err(DataLineError::from)?;
    let mut current = version;
    while current < SCHEMA_VERSION {
        let Some((_, step)) = MIGRATIONS.iter().find(|(from, _)| *from == current) else {
            return Err(DataLineError::Storage(format!(
                "store schema version {version} is not supported by this build (expected {SCHEMA_VERSION}, no migration from {current})"
            )));
        };
        step(&tx).map_err(DataLineError::from)?;
        current += 1;
    }
    tx.execute("UPDATE _dataline_meta SET value = ?1 WHERE key = 'schema_version'", params![SCHEMA_VERSION.to_string()])
        .map_err(DataLineError::from)?;
    tx.commit().map_err(DataLineError::from)?;
    Ok(())
}

fn insert_field_row(tx: &Transaction, database_id: DatabaseId, field: &FieldDefinition) -> rusqlite::Result<()> {
    let encoded = encode_kind(&field.kind)?;
    tx.execute(
        "INSERT INTO fields (id, database_id, name, kind, selection_multi, selection_options, reference_target_database, reference_paired_field, custom_kind_id, ordering)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            field.id.as_uuid().to_string(),
            database_id.as_uuid().to_string(),
            field.name,
            encoded.kind_tag,
            encoded.selection_multi,
            encoded.selection_options,
            encoded.reference_target_database,
            encoded.reference_paired_field,
            encoded.custom_kind_id,
            field.ordering,
        ],
    )?;
    Ok(())
}

fn update_field_row(tx: &Transaction, field: &FieldDefinition) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE fields SET name = ?1, ordering = ?2 WHERE id = ?3",
        params![field.name, field.ordering, field.id.as_uuid().to_string()],
    )?;
    Ok(())
}

fn update_field_kind_row(tx: &Transaction, field_id: FieldId, kind: &FieldKind) -> rusqlite::Result<()> {
    let encoded = encode_kind(kind)?;
    tx.execute(
        "UPDATE fields SET kind = ?1, selection_multi = ?2, selection_options = ?3,
                           reference_target_database = ?4, reference_paired_field = ?5,
                           custom_kind_id = ?6
         WHERE id = ?7",
        params![
            encoded.kind_tag,
            encoded.selection_multi,
            encoded.selection_options,
            encoded.reference_target_database,
            encoded.reference_paired_field,
            encoded.custom_kind_id,
            field_id.as_uuid().to_string(),
        ],
    )?;
    Ok(())
}

struct EncodedKind {
    kind_tag: &'static str,
    selection_multi: Option<i64>,
    selection_options: Option<String>,
    reference_target_database: Option<String>,
    reference_paired_field: Option<String>,
    custom_kind_id: Option<String>,
}

fn encode_kind(kind: &FieldKind) -> rusqlite::Result<EncodedKind> {
    let empty = EncodedKind {
        kind_tag: "",
        selection_multi: None,
        selection_options: None,
        reference_target_database: None,
        reference_paired_field: None,
        custom_kind_id: None,
    };
    Ok(match kind {
        FieldKind::Text | FieldKind::Number | FieldKind::Boolean | FieldKind::Date | FieldKind::Blob | FieldKind::Json => {
            EncodedKind { kind_tag: kind_label(kind), ..empty }
        }
        FieldKind::Selection { multi, options } => {
            let json = serde_json::to_string(options)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            EncodedKind {
                kind_tag: kind_label(kind),
                selection_multi: Some(*multi as i64),
                selection_options: Some(json),
                ..empty
            }
        }
        FieldKind::Reference { target_database, paired_field } => EncodedKind {
            kind_tag: kind_label(kind),
            reference_target_database: Some(target_database.as_uuid().to_string()),
            reference_paired_field: paired_field.map(|f| f.as_uuid().to_string()),
            ..empty
        },
        FieldKind::Custom { kind_id } | FieldKind::CustomBlob { kind_id } => {
            EncodedKind { kind_tag: kind_label(kind), custom_kind_id: Some(kind_id.clone()), ..empty }
        }
    })
}

fn decode_kind(
    tag: &str,
    selection_multi: Option<i64>,
    selection_options: Option<String>,
    reference_target_database: Option<String>,
    reference_paired_field: Option<String>,
    custom_kind_id: Option<String>,
) -> Result<FieldKind> {
    Ok(match tag {
        "text" => FieldKind::Text,
        "number" => FieldKind::Number,
        "boolean" => FieldKind::Boolean,
        "date" => FieldKind::Date,
        "blob" => FieldKind::Blob,
        "json" => FieldKind::Json,
        "selection" => {
            let multi = selection_multi.unwrap_or(0) != 0;
            let options: Vec<SelectionOption> = match selection_options {
                Some(json) => serde_json::from_str(&json).map_err(|e| DataLineError::Storage(e.to_string()))?,
                None => Vec::new(),
            };
            FieldKind::Selection { multi, options }
        }
        "reference" => {
            let target = reference_target_database
                .ok_or_else(|| DataLineError::Storage("reference field missing target database".into()))?;
            let target_database = DatabaseId::from_uuid(parse_uuid(&target)?);
            let paired_field = reference_paired_field.map(|s| parse_uuid(&s)).transpose()?.map(FieldId::from_uuid);
            FieldKind::Reference { target_database, paired_field }
        }
        "custom" => {
            let kind_id = custom_kind_id
                .ok_or_else(|| DataLineError::Storage("custom field missing kind_id".into()))?;
            FieldKind::Custom { kind_id }
        }
        "custom_blob" => {
            let kind_id = custom_kind_id
                .ok_or_else(|| DataLineError::Storage("custom_blob field missing kind_id".into()))?;
            FieldKind::CustomBlob { kind_id }
        }
        other => return Err(DataLineError::Storage(format!("unknown field kind '{other}' in store"))),
    })
}

fn parse_uuid(s: &str) -> Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| DataLineError::Storage(e.to_string()))
}

fn load_registry(conn: &Connection) -> Result<SchemaRegistry> {
    let mut databases: Vec<Database> = {
        let mut stmt = conn.prepare("SELECT id, name FROM databases")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id_str, name) = row?;
            out.push(Database { id: DatabaseId::from_uuid(parse_uuid(&id_str)?), name, fields: Vec::new() });
        }
        out
    };

    for db in &mut databases {
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, selection_multi, selection_options, reference_target_database, reference_paired_field, custom_kind_id, ordering
             FROM fields WHERE database_id = ?1 ORDER BY ordering",
        )?;
        let rows = stmt.query_map(params![db.id.as_uuid().to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, i32>(8)?,
            ))
        })?;

        for row in rows {
            let (id_str, name, kind_tag, selection_multi, selection_options, ref_target, ref_paired, custom_kind_id, ordering) = row?;
            let id = FieldId::from_uuid(parse_uuid(&id_str)?);
            let kind = decode_kind(&kind_tag, selection_multi, selection_options, ref_target, ref_paired, custom_kind_id)?;
            db.fields.push(FieldDefinition { id, name, kind, ordering });
        }
    }

    Ok(SchemaRegistry::from_databases(databases))
}

fn parse_rfc3339(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc)).map_err(|e| DataLineError::Storage(e.to_string()))
}

/// Encodes a [`Value`] into the SQL representation for a column of kind
/// `kind`. `Value::Empty` always maps to `NULL` regardless of kind; any
/// other mismatch between the value's variant and `kind` is rejected — see
/// [`Store::set_value`] for why. Takes `field_id`/`kind` rather than a whole
/// `FieldDefinition` so `Store::retype_field` can call it with a *new* kind
/// a field doesn't have yet — there's nothing else in `FieldDefinition` this
/// needs.
fn encode_scalar_value(field_id: FieldId, kind: &FieldKind, value: &Value) -> Result<rusqlite::types::Value> {
    use rusqlite::types::Value as SqlValue;

    if matches!(value, Value::Empty) {
        return Ok(SqlValue::Null);
    }

    match (kind, value) {
        (FieldKind::Text, Value::Text(s)) => Ok(SqlValue::Text(s.clone())),
        (FieldKind::Number, Value::Number(n)) => Ok(SqlValue::Real(*n)),
        (FieldKind::Boolean, Value::Boolean(b)) => Ok(SqlValue::Integer(*b as i64)),
        (FieldKind::Date, Value::Date(d)) => Ok(SqlValue::Text(d.to_string())),
        (FieldKind::Selection { .. }, Value::Selection(names)) => {
            let json = serde_json::to_string(names).map_err(|e| DataLineError::Storage(e.to_string()))?;
            Ok(SqlValue::Text(json))
        }
        (FieldKind::Blob, Value::Blob(blob_ref)) => Ok(SqlValue::Text(blob_ref.hash.clone())),
        (FieldKind::Json, Value::Json(json)) => {
            serde_json::from_str::<serde_json::Value>(json)
                .map_err(|e| DataLineError::Storage(format!("invalid JSON value: {e}")))?;
            Ok(SqlValue::Text(json.clone()))
        }
        (FieldKind::Custom { kind_id: field_kind_id }, Value::Custom { kind_id: value_kind_id, data }) => {
            if field_kind_id != value_kind_id {
                return Err(DataLineError::ValueKindMismatch { field: field_id, expected: kind_label(kind) });
            }
            Ok(SqlValue::Blob(data.clone()))
        }
        (FieldKind::CustomBlob { kind_id: field_kind_id }, Value::CustomBlob { kind_id: value_kind_id, blob }) => {
            if field_kind_id != value_kind_id {
                return Err(DataLineError::ValueKindMismatch { field: field_id, expected: kind_label(kind) });
            }
            Ok(SqlValue::Text(blob.hash.clone()))
        }
        _ => Err(DataLineError::ValueKindMismatch { field: field_id, expected: kind_label(kind) }),
    }
}

fn decode_scalar_value(kind: &FieldKind, sql_value: rusqlite::types::Value) -> Result<Value> {
    use rusqlite::types::Value as SqlValue;

    if matches!(sql_value, SqlValue::Null) {
        return Ok(Value::Empty);
    }

    Ok(match kind {
        FieldKind::Text => Value::Text(expect_text(sql_value)?),
        FieldKind::Number => Value::Number(expect_real(sql_value)?),
        FieldKind::Boolean => Value::Boolean(expect_integer(sql_value)? != 0),
        FieldKind::Date => {
            let text = expect_text(sql_value)?;
            let date = text.parse().map_err(|_| DataLineError::Storage(format!("invalid stored date: {text}")))?;
            Value::Date(date)
        }
        FieldKind::Selection { .. } => {
            let json = expect_text(sql_value)?;
            let names: Vec<String> = serde_json::from_str(&json).map_err(|e| DataLineError::Storage(e.to_string()))?;
            Value::Selection(names)
        }
        FieldKind::Blob => Value::Blob(BlobRef::from_hash(expect_text(sql_value)?)),
        FieldKind::Json => Value::Json(expect_text(sql_value)?),
        FieldKind::Custom { kind_id } => Value::Custom { kind_id: kind_id.clone(), data: expect_blob(sql_value)? },
        FieldKind::CustomBlob { kind_id } => {
            Value::CustomBlob { kind_id: kind_id.clone(), blob: BlobRef::from_hash(expect_text(sql_value)?) }
        }
        FieldKind::Reference { .. } => {
            unreachable!("reference fields never get a column on their owning table (Architecture.md §3)")
        }
    })
}

/// Coerces `value` (of some field's *old* kind) into what it should become
/// under `target_kind`, per Architecture.md §7's three buckets: lossless
/// (passes through), lossy-but-well-defined (converted), or not
/// representable (`Value::Empty` — `Store::retype_field` is responsible for
/// noticing this happened to a non-empty value and reporting it, since this
/// function alone can't tell "was already empty" apart from "got cleared").
///
/// Only ever called with scalar-kind values — `retype_field` rejects
/// `Reference` on either side before this runs (see
/// `SchemaRegistry::retype_field`), so a `Value::Reference` reaching here
/// would mean that guard was bypassed.
fn coerce_value(value: &Value, target_kind: &FieldKind) -> Value {
    if matches!(value, Value::Empty) {
        return Value::Empty;
    }

    match (value, target_kind) {
        (Value::Text(_), FieldKind::Text) => value.clone(),
        (Value::Number(_), FieldKind::Number) => value.clone(),
        (Value::Boolean(_), FieldKind::Boolean) => value.clone(),
        (Value::Date(_), FieldKind::Date) => value.clone(),
        (Value::Blob(_), FieldKind::Blob) => value.clone(),
        (Value::Json(_), FieldKind::Json) => value.clone(),
        // Lossless in both directions — a `Blob`/`CustomBlob` value is just
        // a `BlobRef` either way (same shared, content-addressed blob
        // table), so retyping between them is purely "gain/drop a plugin
        // tag," never a real content conversion. This is the actual
        // migration path a plugin-owned field like Music's storage field
        // uses to move from an untagged `Blob` onto its own `CustomBlob`
        // kind_id without losing (or having to rewrite) the underlying
        // bytes. See Architecture.md's CustomBlob note.
        (Value::Blob(blob_ref), FieldKind::CustomBlob { kind_id }) => {
            Value::CustomBlob { kind_id: kind_id.clone(), blob: blob_ref.clone() }
        }
        (Value::CustomBlob { blob, .. }, FieldKind::Blob) => Value::Blob(blob.clone()),
        (Value::CustomBlob { blob, .. }, FieldKind::CustomBlob { kind_id }) => {
            Value::CustomBlob { kind_id: kind_id.clone(), blob: blob.clone() }
        }
        (Value::Selection(names), FieldKind::Selection { options, .. }) => {
            // Also how a Selection option removal is handled: retyping a
            // field to itself with a narrower `options` list drops any
            // currently-selected value that no longer has a matching option.
            let kept: Vec<String> = names.iter().filter(|n| options.iter().any(|o| &o.name == *n)).cloned().collect();
            if kept.is_empty() {
                Value::Empty
            } else {
                Value::Selection(kept)
            }
        }

        (Value::Text(s), FieldKind::Number) => s.parse::<f64>().map(Value::Number).unwrap_or(Value::Empty),
        (Value::Text(s), FieldKind::Boolean) => match s.to_lowercase().as_str() {
            "true" => Value::Boolean(true),
            "false" => Value::Boolean(false),
            _ => Value::Empty,
        },
        (Value::Text(s), FieldKind::Date) => s.parse().map(Value::Date).unwrap_or(Value::Empty),
        (Value::Text(s), FieldKind::Json) => {
            if serde_json::from_str::<serde_json::Value>(s).is_ok() { Value::Json(s.clone()) } else { Value::Empty }
        }
        (Value::Json(s), FieldKind::Text) => Value::Text(s.clone()),
        (Value::Text(s), FieldKind::Selection { options, .. }) => {
            if options.iter().any(|o| &o.name == s) {
                Value::Selection(vec![s.clone()])
            } else {
                Value::Empty
            }
        }

        (Value::Number(n), FieldKind::Text) => Value::Text(n.to_string()),
        (Value::Number(n), FieldKind::Boolean) => Value::Boolean(*n != 0.0),

        (Value::Boolean(b), FieldKind::Text) => Value::Text(b.to_string()),
        (Value::Boolean(b), FieldKind::Number) => Value::Number(if *b { 1.0 } else { 0.0 }),

        (Value::Selection(names), FieldKind::Text) => Value::Text(names.join(", ")),
        (Value::Selection(names), FieldKind::Number) => {
            names.first().and_then(|n| n.parse::<f64>().ok()).map(Value::Number).unwrap_or(Value::Empty)
        }

        (Value::Date(d), FieldKind::Text) => Value::Text(d.to_string()),

        // Everything else (Number/Boolean/Selection/Date/Blob -> a kind with
        // no sensible mapping defined above, or anything -> Blob, which
        // never accepts a coerced value since a hash has to come from a real
        // write_blob call) isn't representable.
        _ => Value::Empty,
    }
}

fn expect_text(v: rusqlite::types::Value) -> Result<String> {
    match v {
        rusqlite::types::Value::Text(s) => Ok(s),
        other => Err(DataLineError::Storage(format!("expected a TEXT column value, got {other:?}"))),
    }
}

fn expect_real(v: rusqlite::types::Value) -> Result<f64> {
    match v {
        rusqlite::types::Value::Real(n) => Ok(n),
        rusqlite::types::Value::Integer(n) => Ok(n as f64),
        other => Err(DataLineError::Storage(format!("expected a REAL column value, got {other:?}"))),
    }
}

fn expect_integer(v: rusqlite::types::Value) -> Result<i64> {
    match v {
        rusqlite::types::Value::Integer(n) => Ok(n),
        other => Err(DataLineError::Storage(format!("expected an INTEGER column value, got {other:?}"))),
    }
}

fn expect_blob(v: rusqlite::types::Value) -> Result<Vec<u8>> {
    match v {
        rusqlite::types::Value::Blob(b) => Ok(b),
        other => Err(DataLineError::Storage(format!("expected a BLOB column value, got {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;

    fn temp_store_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("store.sqlite")
    }

    mod coerce_value_tests {
        use super::*;

        #[test]
        fn empty_always_stays_empty() {
            for kind in [FieldKind::Text, FieldKind::Number, FieldKind::Boolean, FieldKind::Date, FieldKind::Blob] {
                assert_eq!(coerce_value(&Value::Empty, &kind), Value::Empty);
            }
        }

        #[test]
        fn same_kind_passes_through_unchanged() {
            assert_eq!(coerce_value(&Value::Text("hi".into()), &FieldKind::Text), Value::Text("hi".into()));
            assert_eq!(coerce_value(&Value::Number(3.0), &FieldKind::Number), Value::Number(3.0));
            assert_eq!(coerce_value(&Value::Boolean(true), &FieldKind::Boolean), Value::Boolean(true));
        }

        #[test]
        fn text_to_number_parses_or_clears() {
            assert_eq!(coerce_value(&Value::Text("3.5".into()), &FieldKind::Number), Value::Number(3.5));
            assert_eq!(coerce_value(&Value::Text("not a number".into()), &FieldKind::Number), Value::Empty);
        }

        #[test]
        fn text_to_boolean_only_accepts_true_false_case_insensitive() {
            assert_eq!(coerce_value(&Value::Text("TRUE".into()), &FieldKind::Boolean), Value::Boolean(true));
            assert_eq!(coerce_value(&Value::Text("false".into()), &FieldKind::Boolean), Value::Boolean(false));
            assert_eq!(coerce_value(&Value::Text("yes".into()), &FieldKind::Boolean), Value::Empty);
        }

        #[test]
        fn text_to_date_parses_iso_or_clears() {
            assert_eq!(
                coerce_value(&Value::Text("2024-01-15".into()), &FieldKind::Date),
                Value::Date(chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap())
            );
            assert_eq!(coerce_value(&Value::Text("not a date".into()), &FieldKind::Date), Value::Empty);
        }

        #[test]
        fn text_to_selection_only_matches_an_existing_option() {
            let options = vec![
                SelectionOption { name: "Common".into(), color: "gray".into() },
                SelectionOption { name: "Rare".into(), color: "blue".into() },
            ];
            let kind = FieldKind::Selection { multi: false, options: options.clone() };
            assert_eq!(coerce_value(&Value::Text("Rare".into()), &kind), Value::Selection(vec!["Rare".into()]));
            assert_eq!(coerce_value(&Value::Text("Mythic".into()), &kind), Value::Empty);
        }

        #[test]
        fn number_to_text_and_boolean() {
            assert_eq!(coerce_value(&Value::Number(3.0), &FieldKind::Text), Value::Text("3".into()));
            assert_eq!(coerce_value(&Value::Number(0.0), &FieldKind::Boolean), Value::Boolean(false));
            assert_eq!(coerce_value(&Value::Number(1.0), &FieldKind::Boolean), Value::Boolean(true));
        }

        #[test]
        fn number_to_date_or_blob_is_not_representable() {
            assert_eq!(coerce_value(&Value::Number(3.0), &FieldKind::Date), Value::Empty);
            assert_eq!(coerce_value(&Value::Number(3.0), &FieldKind::Blob), Value::Empty);
        }

        #[test]
        fn boolean_to_text_and_number() {
            assert_eq!(coerce_value(&Value::Boolean(true), &FieldKind::Text), Value::Text("true".into()));
            assert_eq!(coerce_value(&Value::Boolean(true), &FieldKind::Number), Value::Number(1.0));
            assert_eq!(coerce_value(&Value::Boolean(false), &FieldKind::Number), Value::Number(0.0));
        }

        #[test]
        fn selection_to_text_joins_every_selected_name() {
            let value = Value::Selection(vec!["Common".into(), "Rare".into()]);
            assert_eq!(coerce_value(&value, &FieldKind::Text), Value::Text("Common, Rare".into()));
        }

        #[test]
        fn selection_to_number_parses_first_name_or_clears() {
            assert_eq!(coerce_value(&Value::Selection(vec!["3".into()]), &FieldKind::Number), Value::Number(3.0));
            assert_eq!(coerce_value(&Value::Selection(vec!["Rare".into()]), &FieldKind::Number), Value::Empty);
        }

        #[test]
        fn selection_narrowed_options_drops_values_that_no_longer_match() {
            // Same field, same kind, but an option was removed — the
            // Selection -> Selection branch, not a cross-kind conversion.
            let narrowed = FieldKind::Selection {
                multi: true,
                options: vec![SelectionOption { name: "Common".into(), color: "gray".into() }],
            };
            assert_eq!(
                coerce_value(&Value::Selection(vec!["Common".into(), "Rare".into()]), &narrowed),
                Value::Selection(vec!["Common".into()]),
                "Rare should be dropped, Common kept"
            );
            assert_eq!(
                coerce_value(&Value::Selection(vec!["Rare".into()]), &narrowed),
                Value::Empty,
                "nothing left selected once the only match is removed"
            );
        }

        #[test]
        fn date_to_text_and_everything_else_not_representable() {
            let date = chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
            assert_eq!(coerce_value(&Value::Date(date), &FieldKind::Text), Value::Text("2024-01-15".into()));
            assert_eq!(coerce_value(&Value::Date(date), &FieldKind::Number), Value::Empty);
        }

        #[test]
        fn blob_never_coerces_into_anything_else() {
            let blob = Value::Blob(BlobRef::from_hash("deadbeef"));
            assert_eq!(coerce_value(&blob, &FieldKind::Text), Value::Empty);
            assert_eq!(coerce_value(&blob, &FieldKind::Number), Value::Empty);
        }

        #[test]
        fn nothing_ever_coerces_into_blob() {
            assert_eq!(coerce_value(&Value::Text("deadbeef".into()), &FieldKind::Blob), Value::Empty);
            assert_eq!(coerce_value(&Value::Number(1.0), &FieldKind::Blob), Value::Empty);
        }
    }

    #[test]
    fn create_record_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let name_field = store.add_field(card, "Name", FieldKind::Text).unwrap();

        let link = store.create_record(card).unwrap();
        let record = store.get_record(link).unwrap();

        assert_eq!(record.link, link);
        assert_eq!(record.database_id, card);
        assert_eq!(record.values.get(&name_field), Some(&Value::Empty));
        assert_eq!(record.created_at, record.updated_at);
    }

    #[test]
    fn set_value_round_trips_every_scalar_kind() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();

        let text_field = store.add_field(card, "Name", FieldKind::Text).unwrap();
        let number_field = store.add_field(card, "Cost", FieldKind::Number).unwrap();
        let bool_field = store.add_field(card, "Active", FieldKind::Boolean).unwrap();
        let date_field = store.add_field(card, "Released", FieldKind::Date).unwrap();
        let selection_field =
            store.add_field(card, "Rarity", FieldKind::Selection { multi: false, options: vec![] }).unwrap();

        let link = store.create_record(card).unwrap();
        store.set_value(link, text_field, Value::Text("Fireball".into())).unwrap();
        store.set_value(link, number_field, Value::Number(3.0)).unwrap();
        store.set_value(link, bool_field, Value::Boolean(true)).unwrap();
        store.set_value(link, date_field, Value::Date(NaiveDate::from_ymd_opt(2024, 1, 15).unwrap())).unwrap();
        store.set_value(link, selection_field, Value::Selection(vec!["Rare".to_string()])).unwrap();

        let record = store.get_record(link).unwrap();
        assert_eq!(record.values[&text_field], Value::Text("Fireball".into()));
        assert_eq!(record.values[&number_field], Value::Number(3.0));
        assert_eq!(record.values[&bool_field], Value::Boolean(true));
        assert_eq!(record.values[&date_field], Value::Date(NaiveDate::from_ymd_opt(2024, 1, 15).unwrap()));
        assert_eq!(record.values[&selection_field], Value::Selection(vec!["Rare".to_string()]));
        assert!(record.updated_at >= record.created_at);
    }

    #[test]
    fn set_value_round_trips_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let field = store.add_field(card, "Style", FieldKind::Json).unwrap();
        let link = store.create_record(card).unwrap();

        store.set_value(link, field, Value::Json(r##"{"kind":"gradient","stops":["#fff","#000"]}"##.into())).unwrap();

        let record = store.get_record(link).unwrap();
        assert_eq!(record.values[&field], Value::Json(r##"{"kind":"gradient","stops":["#fff","#000"]}"##.into()));
    }

    #[test]
    fn set_value_rejects_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let field = store.add_field(card, "Style", FieldKind::Json).unwrap();
        let link = store.create_record(card).unwrap();

        let err = store.set_value(link, field, Value::Json("not json".into())).unwrap_err();
        assert!(matches!(err, DataLineError::Storage(_)));
    }

    #[test]
    fn json_field_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);
        let card;
        let field;
        let link;
        {
            let mut store = Store::open(&path).unwrap();
            card = store.create_database("Card").unwrap();
            field = store.add_field(card, "Style", FieldKind::Json).unwrap();
            link = store.create_record(card).unwrap();
            store.set_value(link, field, Value::Json(r#"{"a":1}"#.into())).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_record(link).unwrap().values[&field], Value::Json(r#"{"a":1}"#.into()));
    }

    #[test]
    fn set_value_round_trips_custom() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let field = store
            .add_field(card, "Track", FieldKind::Custom { kind_id: "com.myline.music".into() })
            .unwrap();
        let link = store.create_record(card).unwrap();

        store
            .set_value(
                link,
                field,
                Value::Custom { kind_id: "com.myline.music".into(), data: vec![1, 2, 3, 4] },
            )
            .unwrap();

        let record = store.get_record(link).unwrap();
        assert_eq!(
            record.values[&field],
            Value::Custom { kind_id: "com.myline.music".into(), data: vec![1, 2, 3, 4] }
        );
    }

    #[test]
    fn set_value_rejects_custom_kind_id_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let field = store
            .add_field(card, "Track", FieldKind::Custom { kind_id: "com.myline.music".into() })
            .unwrap();
        let link = store.create_record(card).unwrap();

        let err = store
            .set_value(link, field, Value::Custom { kind_id: "com.myline.image".into(), data: vec![9] })
            .unwrap_err();
        assert!(matches!(err, DataLineError::ValueKindMismatch { .. }));
    }

    #[test]
    fn custom_field_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);
        let card;
        let field;
        let link;
        {
            let mut store = Store::open(&path).unwrap();
            card = store.create_database("Card").unwrap();
            field = store
                .add_field(card, "Track", FieldKind::Custom { kind_id: "com.myline.music".into() })
                .unwrap();
            link = store.create_record(card).unwrap();
            store
                .set_value(
                    link,
                    field,
                    Value::Custom { kind_id: "com.myline.music".into(), data: vec![5, 6, 7] },
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.get_database(card).unwrap().field(field).unwrap().kind,
            FieldKind::Custom { kind_id: "com.myline.music".into() }
        );
        assert_eq!(
            store.get_record(link).unwrap().values[&field],
            Value::Custom { kind_id: "com.myline.music".into(), data: vec![5, 6, 7] }
        );
    }

    #[test]
    fn set_value_round_trips_custom_blob() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let field = store
            .add_field(card, "Track", FieldKind::CustomBlob { kind_id: "com.myline.music.value".into() })
            .unwrap();
        let link = store.create_record(card).unwrap();
        let blob_ref = store.write_blob(b"fake audio bytes", Some("song.mp3")).unwrap();

        store
            .set_value(
                link,
                field,
                Value::CustomBlob { kind_id: "com.myline.music.value".into(), blob: blob_ref.clone() },
            )
            .unwrap();

        let record = store.get_record(link).unwrap();
        assert_eq!(
            record.values[&field],
            Value::CustomBlob { kind_id: "com.myline.music.value".into(), blob: blob_ref }
        );
    }

    #[test]
    fn set_value_rejects_custom_blob_kind_id_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let field = store
            .add_field(card, "Track", FieldKind::CustomBlob { kind_id: "com.myline.music.value".into() })
            .unwrap();
        let link = store.create_record(card).unwrap();
        let blob_ref = store.write_blob(b"fake audio bytes", None).unwrap();

        let err = store
            .set_value(link, field, Value::CustomBlob { kind_id: "com.myline.image.value".into(), blob: blob_ref })
            .unwrap_err();
        assert!(matches!(err, DataLineError::ValueKindMismatch { .. }));
    }

    /// The exact gap-2 motivating scenario: a CustomBlob value pointing at
    /// a hash that was never actually written should be caught the same
    /// way an ordinary dangling `Blob` reference already is — never
    /// silently accepted, per `set_value`'s own doc comment.
    #[test]
    fn set_value_rejects_dangling_custom_blob_reference() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let field = store
            .add_field(card, "Track", FieldKind::CustomBlob { kind_id: "com.myline.music.value".into() })
            .unwrap();
        let link = store.create_record(card).unwrap();
        let bogus = BlobRef::from_hash("not-a-real-hash");

        let err = store
            .set_value(link, field, Value::CustomBlob { kind_id: "com.myline.music.value".into(), blob: bogus.clone() })
            .unwrap_err();
        assert_eq!(err, DataLineError::BlobNotFound(bogus));
    }

    /// Proves the actual point of `CustomBlob` over `Custom`: the blob
    /// table stays the single source of truth for the bytes — two
    /// `CustomBlob` values pointing at identical content share one blob
    /// row, same content-addressed dedup `Blob` fields already get, and
    /// the filename set on first write survives being read back through a
    /// completely different field/record.
    #[test]
    fn custom_blob_shares_content_addressed_storage_with_blob() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let field = store
            .add_field(card, "Track", FieldKind::CustomBlob { kind_id: "com.myline.music.value".into() })
            .unwrap();
        let a = store.create_record(card).unwrap();
        let b = store.create_record(card).unwrap();

        let first = store.write_blob(b"same bytes", Some("first-name.mp3")).unwrap();
        let second = store.write_blob(b"same bytes", Some("second-name.mp3")).unwrap();
        assert_eq!(first.hash, second.hash, "identical content must dedupe to the same hash");

        store
            .set_value(a, field, Value::CustomBlob { kind_id: "com.myline.music.value".into(), blob: first })
            .unwrap();
        store
            .set_value(b, field, Value::CustomBlob { kind_id: "com.myline.music.value".into(), blob: second })
            .unwrap();

        let Value::CustomBlob { blob: a_blob, .. } = &store.get_record(a).unwrap().values[&field] else {
            panic!("expected a CustomBlob value");
        };
        assert_eq!(a_blob.filename(), Some("first-name.mp3"), "dedup keeps whichever filename was stored first");
    }

    #[test]
    fn custom_blob_field_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);
        let card;
        let field;
        let link;
        let blob_ref;
        {
            let mut store = Store::open(&path).unwrap();
            card = store.create_database("Card").unwrap();
            field = store
                .add_field(card, "Track", FieldKind::CustomBlob { kind_id: "com.myline.music.value".into() })
                .unwrap();
            link = store.create_record(card).unwrap();
            blob_ref = store.write_blob(b"reopen bytes", Some("reopen.mp3")).unwrap();
            store
                .set_value(
                    link,
                    field,
                    Value::CustomBlob { kind_id: "com.myline.music.value".into(), blob: blob_ref.clone() },
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.get_database(card).unwrap().field(field).unwrap().kind,
            FieldKind::CustomBlob { kind_id: "com.myline.music.value".into() }
        );
        assert_eq!(
            store.get_record(link).unwrap().values[&field],
            Value::CustomBlob { kind_id: "com.myline.music.value".into(), blob: blob_ref }
        );
    }

    #[test]
    fn set_value_can_clear_back_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let field = store.add_field(card, "Name", FieldKind::Text).unwrap();
        let link = store.create_record(card).unwrap();

        store.set_value(link, field, Value::Text("Fireball".into())).unwrap();
        store.set_value(link, field, Value::Empty).unwrap();

        assert_eq!(store.get_record(link).unwrap().values[&field], Value::Empty);
    }

    #[test]
    fn set_value_rejects_kind_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let field = store.add_field(card, "Name", FieldKind::Text).unwrap();
        let link = store.create_record(card).unwrap();

        let err = store.set_value(link, field, Value::Number(1.0)).unwrap_err();
        assert_eq!(err, DataLineError::ValueKindMismatch { field, expected: "text" });
    }

    #[test]
    fn set_value_rejects_reference_field() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();
        let ref_field = store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();
        let link = store.create_record(card).unwrap();

        let err = store.set_value(link, ref_field, Value::Reference(vec![])).unwrap_err();
        assert_eq!(err, DataLineError::UnsupportedFieldKindForValue { field: ref_field, kind: "reference" });
    }

    #[test]
    fn write_blob_then_read_blob_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();

        let blob_ref = store.write_blob(b"pretend this is a texture", None).unwrap();
        let bytes = store.read_blob(&blob_ref).unwrap();

        assert_eq!(bytes, b"pretend this is a texture");
    }

    #[test]
    fn write_blob_is_content_addressed() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();

        let a = store.write_blob(b"same bytes", None).unwrap();
        let b = store.write_blob(b"same bytes", None).unwrap();
        let c = store.write_blob(b"different bytes", None).unwrap();

        assert_eq!(a, b, "identical content must produce the identical handle");
        assert_ne!(a, c);

        let count: i64 =
            store.conn.query_row("SELECT count(*) FROM blobs", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 2, "writing the same content twice must not duplicate the row");
    }

    #[test]
    fn write_blob_stores_and_returns_filename() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();

        let blob_ref = store.write_blob(b"song bytes", Some("track.mp3")).unwrap();
        assert_eq!(blob_ref.filename(), Some("track.mp3"));
    }

    #[test]
    fn write_blob_same_content_keeps_first_filename() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();

        let first = store.write_blob(b"same bytes", Some("first.mp3")).unwrap();
        let second = store.write_blob(b"same bytes", Some("second.mp3")).unwrap();

        assert_eq!(first.filename(), Some("first.mp3"));
        assert_eq!(second.filename(), Some("first.mp3"), "content dedup keeps whichever filename was written first");
    }

    #[test]
    fn blob_filename_round_trips_through_get_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let db = store.create_database("Card").unwrap();
        let field = store.add_field(db, "Art", FieldKind::Blob).unwrap();
        let link = store.create_record(db).unwrap();

        let blob_ref = store.write_blob(b"art bytes", Some("art.png")).unwrap();
        store.set_value(link, field, Value::Blob(blob_ref)).unwrap();

        let record = store.get_record(link).unwrap();
        match &record.values[&field] {
            Value::Blob(blob_ref) => assert_eq!(blob_ref.filename(), Some("art.png")),
            other => panic!("expected Blob, got {other:?}"),
        }
    }

    #[test]
    fn read_blob_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(temp_store_path(&dir)).unwrap();

        let bogus = BlobRef::from_hash("not-a-real-hash");
        assert_eq!(store.read_blob(&bogus).unwrap_err(), DataLineError::BlobNotFound(bogus));
    }

    #[test]
    fn set_value_on_blob_field_round_trips_through_get_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let artwork_field = store.add_field(card, "Artwork", FieldKind::Blob).unwrap();
        let link = store.create_record(card).unwrap();

        let blob_ref = store.write_blob(b"pretend artwork bytes", None).unwrap();
        store.set_value(link, artwork_field, Value::Blob(blob_ref.clone())).unwrap();

        let record = store.get_record(link).unwrap();
        assert_eq!(record.values[&artwork_field], Value::Blob(blob_ref.clone()));

        let Value::Blob(stored_ref) = &record.values[&artwork_field] else { panic!("expected a Blob value") };
        assert_eq!(store.read_blob(stored_ref).unwrap(), b"pretend artwork bytes");
    }

    #[test]
    fn set_value_on_blob_field_rejects_non_blob_value() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let artwork_field = store.add_field(card, "Artwork", FieldKind::Blob).unwrap();
        let link = store.create_record(card).unwrap();

        let err = store.set_value(link, artwork_field, Value::Text("not a blob".into())).unwrap_err();
        assert_eq!(err, DataLineError::ValueKindMismatch { field: artwork_field, expected: "blob" });
    }

    #[test]
    fn set_value_on_blob_field_rejects_a_hash_that_was_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let artwork_field = store.add_field(card, "Artwork", FieldKind::Blob).unwrap();
        let link = store.create_record(card).unwrap();

        // BlobRef::from_hash exists precisely so a value round-tripped through
        // an external boundary (e.g. FFI) can be reconstructed — which also
        // means it's not itself proof the hash was ever written. That check
        // has to happen here, at the point of use.
        let fabricated = BlobRef::from_hash("not-a-real-hash");
        let err = store.set_value(link, artwork_field, Value::Blob(fabricated.clone())).unwrap_err();
        assert_eq!(err, DataLineError::BlobNotFound(fabricated));
    }

    #[test]
    fn find_by_value_matches_records_sharing_the_same_blob() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let artwork_field = store.add_field(card, "Artwork", FieldKind::Blob).unwrap();

        let blob_ref = store.write_blob(b"shared texture", None).unwrap();
        let a = store.create_record(card).unwrap();
        store.set_value(a, artwork_field, Value::Blob(blob_ref.clone())).unwrap();
        let b = store.create_record(card).unwrap();
        store.set_value(b, artwork_field, Value::Blob(blob_ref.clone())).unwrap();
        let c = store.create_record(card).unwrap();
        let other_blob = store.write_blob(b"other texture", None).unwrap();
        store.set_value(c, artwork_field, Value::Blob(other_blob)).unwrap();

        let matches = store.find_by_value(card, artwork_field, Value::Blob(blob_ref)).unwrap();
        let links: HashSet<Link> = matches.into_iter().map(|r| r.link).collect();
        assert_eq!(links, HashSet::from([a, b]));
    }

    #[test]
    fn blobs_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);

        let (card, link, field, blob_ref) = {
            let mut store = Store::open(&path).unwrap();
            let card = store.create_database("Card").unwrap();
            let field = store.add_field(card, "Artwork", FieldKind::Blob).unwrap();
            let link = store.create_record(card).unwrap();
            let blob_ref = store.write_blob(b"persisted bytes", None).unwrap();
            store.set_value(link, field, Value::Blob(blob_ref.clone())).unwrap();
            (card, link, field, blob_ref)
        };

        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.read_blob(&blob_ref).unwrap(), b"persisted bytes");
        let record = reopened.get_record(link).unwrap();
        assert_eq!(record.database_id, card);
        assert_eq!(record.values[&field], Value::Blob(blob_ref));
    }

    #[test]
    fn set_value_on_missing_record_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let field = store.add_field(card, "Name", FieldKind::Text).unwrap();

        let missing = Link::new();
        let err = store.set_value(missing, field, Value::Text("x".into())).unwrap_err();
        assert_eq!(err, DataLineError::RecordNotFound(missing));
    }

    #[test]
    fn get_record_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(temp_store_path(&dir)).unwrap();
        let missing = Link::new();
        assert_eq!(store.get_record(missing).unwrap_err(), DataLineError::RecordNotFound(missing));
    }

    #[test]
    fn delete_record_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let link = store.create_record(card).unwrap();

        store.delete_record(link).unwrap();

        assert_eq!(store.get_record(link).unwrap_err(), DataLineError::RecordNotFound(link));
        assert!(store.list_records(card).unwrap().is_empty());
    }

    #[test]
    fn list_records_returns_every_record_in_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();

        let a = store.create_record(card).unwrap();
        let b = store.create_record(card).unwrap();
        store.create_record(balance).unwrap();

        let links: HashSet<Link> = store.list_records(card).unwrap().into_iter().map(|r| r.link).collect();
        assert_eq!(links, HashSet::from([a, b]));
    }

    #[test]
    fn records_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);

        let (card, link, field) = {
            let mut store = Store::open(&path).unwrap();
            let card = store.create_database("Card").unwrap();
            let field = store.add_field(card, "Name", FieldKind::Text).unwrap();
            let link = store.create_record(card).unwrap();
            store.set_value(link, field, Value::Text("Fireball".into())).unwrap();
            (card, link, field)
        };

        let reopened = Store::open(&path).unwrap();
        let record = reopened.get_record(link).unwrap();
        assert_eq!(record.database_id, card);
        assert_eq!(record.values[&field], Value::Text("Fireball".into()));
    }

    #[test]
    fn find_by_value_returns_exact_matches_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let name_field = store.add_field(card, "Name", FieldKind::Text).unwrap();

        let fireball = store.create_record(card).unwrap();
        store.set_value(fireball, name_field, Value::Text("Fireball".into())).unwrap();
        let frostbolt = store.create_record(card).unwrap();
        store.set_value(frostbolt, name_field, Value::Text("Frostbolt".into())).unwrap();
        let another_fireball = store.create_record(card).unwrap();
        store.set_value(another_fireball, name_field, Value::Text("Fireball".into())).unwrap();

        let matches = store.find_by_value(card, name_field, Value::Text("Fireball".into())).unwrap();
        let links: HashSet<Link> = matches.into_iter().map(|r| r.link).collect();
        assert_eq!(links, HashSet::from([fireball, another_fireball]));
    }

    #[test]
    fn find_by_value_rejects_reference_field() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();
        let ref_field = store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        let err = store.find_by_value(card, ref_field, Value::Reference(vec![])).unwrap_err();
        assert_eq!(err, DataLineError::UnsupportedFieldKindForValue { field: ref_field, kind: "reference" });
    }

    #[test]
    fn find_by_value_missing_field_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let missing = FieldId::new();

        let err = store.find_by_value(card, missing, Value::Text("x".into())).unwrap_err();
        assert_eq!(err, DataLineError::FieldNotFound(missing));
    }

    #[test]
    fn search_text_matches_case_insensitively_and_ignores_other_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let name_field = store.add_field(card, "Name", FieldKind::Text).unwrap();
        let cost_field = store.add_field(card, "Cost", FieldKind::Number).unwrap();

        let fireball = store.create_record(card).unwrap();
        store.set_value(fireball, name_field, Value::Text("Fireball".into())).unwrap();
        store.set_value(fireball, cost_field, Value::Number(3.0)).unwrap();

        let frostbolt = store.create_record(card).unwrap();
        store.set_value(frostbolt, name_field, Value::Text("Frostbolt".into())).unwrap();

        let matches = store.search_text(card, "FIRE").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].link, fireball);

        // A number field containing digits that happen to match the query text
        // should never match — only Text-kind field values are searched.
        assert!(store.search_text(card, "3").unwrap().is_empty());
    }

    #[test]
    fn search_text_empty_query_returns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        store.add_field(card, "Name", FieldKind::Text).unwrap();
        store.create_record(card).unwrap();

        assert!(store.search_text(card, "").unwrap().is_empty());
    }

    #[test]
    fn related_records_flattens_across_multiple_reference_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();
        let balance_field = store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();
        let related_field = store
            .add_field(card, "Related Cards", FieldKind::Reference { target_database: card, paired_field: None })
            .unwrap();

        let fireball = store.create_record(card).unwrap();
        let firebolt = store.create_record(card).unwrap();
        let balance_entry = store.create_record(balance).unwrap();

        store.create_reference(fireball, balance_field, balance_entry).unwrap();
        store.create_reference(fireball, related_field, firebolt).unwrap();

        let related: HashSet<Link> = store.related_records(fireball).unwrap().into_iter().collect();
        assert_eq!(related, HashSet::from([balance_entry, firebolt]));
    }

    #[test]
    fn related_records_missing_link_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(temp_store_path(&dir)).unwrap();
        let missing = Link::new();
        assert_eq!(store.related_records(missing).unwrap_err(), DataLineError::RecordNotFound(missing));
    }

    #[test]
    fn create_reference_is_readable_from_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();
        let card_field = store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();
        let balance_field = match &store.get_database(card).unwrap().field(card_field).unwrap().kind {
            FieldKind::Reference { paired_field: Some(p), .. } => *p,
            other => panic!("expected paired Reference, got {other:?}"),
        };

        let card_link = store.create_record(card).unwrap();
        let balance_link = store.create_record(balance).unwrap();

        let reference = store.create_reference(card_link, card_field, balance_link).unwrap();
        assert_eq!(reference.source_link, card_link);
        assert_eq!(reference.target_link, balance_link);

        let from_card = store.list_references(card_link, card_field).unwrap();
        assert_eq!(from_card.len(), 1);
        assert_eq!(from_card[0].id, reference.id);
        assert_eq!(from_card[0].target_link, balance_link);

        let from_balance = store.list_references(balance_link, balance_field).unwrap();
        assert_eq!(from_balance.len(), 1);
        assert_eq!(from_balance[0].id, reference.id);
        assert_eq!(from_balance[0].target_link, card_link, "querying from the back-reference side returns the original record");
    }

    #[test]
    fn get_record_includes_reference_field_values() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();
        let card_field = store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        let card_link = store.create_record(card).unwrap();
        let balance_link = store.create_record(balance).unwrap();
        store.create_reference(card_link, card_field, balance_link).unwrap();

        let record = store.get_record(card_link).unwrap();
        assert_eq!(record.values[&card_field], Value::Reference(vec![balance_link]));
    }

    #[test]
    fn get_record_reference_field_defaults_to_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();
        let card_field = store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        let card_link = store.create_record(card).unwrap();

        let record = store.get_record(card_link).unwrap();
        assert_eq!(record.values[&card_field], Value::Reference(vec![]));
    }

    #[test]
    fn self_referencing_reference_field_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let related_field = store
            .add_field(card, "Related Cards", FieldKind::Reference { target_database: card, paired_field: None })
            .unwrap();

        let a = store.create_record(card).unwrap();
        let b = store.create_record(card).unwrap();
        store.create_reference(a, related_field, b).unwrap();

        let from_a = store.list_references(a, related_field).unwrap();
        assert_eq!(from_a.len(), 1);
        assert_eq!(from_a[0].target_link, b);

        // The back-reference field is a *different* field on the same database —
        // querying b through it should show a, even though both fields live here.
        let back_field = match &store.get_database(card).unwrap().field(related_field).unwrap().kind {
            FieldKind::Reference { paired_field: Some(p), .. } => *p,
            other => panic!("expected paired Reference, got {other:?}"),
        };
        let from_b = store.list_references(b, back_field).unwrap();
        assert_eq!(from_b.len(), 1);
        assert_eq!(from_b[0].target_link, a);
    }

    #[test]
    fn create_reference_rejects_wrong_target_database() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();
        let other = store.create_database("Other").unwrap();
        let card_field = store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        let card_link = store.create_record(card).unwrap();
        let wrong_link = store.create_record(other).unwrap();

        let err = store.create_reference(card_link, card_field, wrong_link).unwrap_err();
        assert_eq!(
            err,
            DataLineError::ReferenceTargetDatabaseMismatch { field: card_field, expected: balance, actual: other }
        );
    }

    #[test]
    fn delete_reference_removes_it_from_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();
        let card_field = store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();
        let balance_field = match &store.get_database(card).unwrap().field(card_field).unwrap().kind {
            FieldKind::Reference { paired_field: Some(p), .. } => *p,
            other => panic!("expected paired Reference, got {other:?}"),
        };

        let card_link = store.create_record(card).unwrap();
        let balance_link = store.create_record(balance).unwrap();
        let reference = store.create_reference(card_link, card_field, balance_link).unwrap();

        store.delete_reference(reference.id, card_field).unwrap();

        assert!(store.list_references(card_link, card_field).unwrap().is_empty());
        assert!(store.list_references(balance_link, balance_field).unwrap().is_empty());
    }

    #[test]
    fn delete_reference_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();
        let card_field = store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        let missing = ReferenceId::new();
        let err = store.delete_reference(missing, card_field).unwrap_err();
        assert_eq!(err, DataLineError::ReferenceNotFound(missing));
    }

    #[test]
    fn delete_record_cascades_to_remove_references() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();
        let card_field = store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();
        let balance_field = match &store.get_database(card).unwrap().field(card_field).unwrap().kind {
            FieldKind::Reference { paired_field: Some(p), .. } => *p,
            other => panic!("expected paired Reference, got {other:?}"),
        };

        let card_link = store.create_record(card).unwrap();
        let balance_link = store.create_record(balance).unwrap();
        store.create_reference(card_link, card_field, balance_link).unwrap();

        store.delete_record(card_link).unwrap();

        // The balance record survives; only the relationship (and the card
        // record) should be gone.
        assert!(store.get_record(balance_link).is_ok());
        assert!(store.list_references(balance_link, balance_field).unwrap().is_empty());
    }

    #[test]
    fn reference_join_table_is_dropped_when_the_field_pair_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();
        let card_field = store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        let table_name = reference_table_name(card_field, {
            match &store.get_database(card).unwrap().field(card_field).unwrap().kind {
                FieldKind::Reference { paired_field: Some(p), .. } => *p,
                other => panic!("expected paired Reference, got {other:?}"),
            }
        });
        assert!(table_exists(&store.conn, &table_name).unwrap());

        store.remove_field(card, card_field).unwrap();

        assert!(!table_exists(&store.conn, &table_name).unwrap());
    }

    #[test]
    fn references_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);

        let (card_link, balance_link, card_field, reference_id) = {
            let mut store = Store::open(&path).unwrap();
            let card = store.create_database("Card").unwrap();
            let balance = store.create_database("Balance change").unwrap();
            let card_field = store
                .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
                .unwrap();
            let card_link = store.create_record(card).unwrap();
            let balance_link = store.create_record(balance).unwrap();
            let reference = store.create_reference(card_link, card_field, balance_link).unwrap();
            (card_link, balance_link, card_field, reference.id)
        };

        let reopened = Store::open(&path).unwrap();
        let refs = reopened.list_references(card_link, card_field).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, reference_id);
        assert_eq!(refs[0].target_link, balance_link);
    }

    #[test]
    fn create_database_creates_metadata_row_and_record_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);
        let mut store = Store::open(&path).unwrap();

        let card = store.create_database("Card").unwrap();

        let conn = Connection::open(&path).unwrap();
        let name: String = conn
            .query_row("SELECT name FROM databases WHERE id = ?1", params![card.as_uuid().to_string()], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "Card");

        let table_present = table_exists(&conn, &database_table_name(card)).unwrap();
        assert!(table_present, "per-database record table should exist");
    }

    #[test]
    fn scalar_field_gets_a_real_column_reference_field_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();

        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();

        let name_field = store.add_field(card, "Name", FieldKind::Text).unwrap();
        let ref_field = store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        let columns = table_columns(&store.conn, &database_table_name(card));
        assert!(columns.contains(&field_column_name(name_field)), "scalar field should have a column");
        assert!(
            !columns.contains(&field_column_name(ref_field)),
            "reference field should never get a column on the owning table"
        );
    }

    #[test]
    fn reopening_rehydrates_schema_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);

        let (card, balance, name_field) = {
            let mut store = Store::open(&path).unwrap();
            let card = store.create_database("Card").unwrap();
            let balance = store.create_database("Balance change").unwrap();
            let name_field = store.add_field(card, "Name", FieldKind::Text).unwrap();
            store
                .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
                .unwrap();
            (card, balance, name_field)
        };

        let reopened = Store::open(&path).unwrap();
        let card_db = reopened.get_database(card).unwrap();
        assert_eq!(card_db.fields.len(), 2);
        assert_eq!(card_db.field(name_field).unwrap().name, "Name");

        let balance_db = reopened.get_database(balance).unwrap();
        assert_eq!(balance_db.fields.len(), 1, "back-reference field should have persisted too");
        match &balance_db.fields[0].kind {
            FieldKind::Reference { target_database, .. } => assert_eq!(*target_database, card),
            other => panic!("expected Reference, got {other:?}"),
        }
    }

    #[test]
    fn self_reference_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);

        let card = {
            let mut store = Store::open(&path).unwrap();
            let card = store.create_database("Card").unwrap();
            store
                .add_field(card, "Related Cards", FieldKind::Reference { target_database: card, paired_field: None })
                .unwrap();
            card
        };

        let reopened = Store::open(&path).unwrap();
        let card_db = reopened.get_database(card).unwrap();
        assert_eq!(card_db.fields.len(), 2);
        for field in &card_db.fields {
            match &field.kind {
                FieldKind::Reference { target_database, .. } => assert_eq!(*target_database, card),
                other => panic!("expected Reference, got {other:?}"),
            }
        }
    }

    #[test]
    fn remove_field_persists_cascading_removal_and_drops_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);
        let mut store = Store::open(&path).unwrap();

        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();
        let forward_id = store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        store.remove_field(card, forward_id).unwrap();

        assert!(store.get_database(card).unwrap().fields.is_empty());
        assert!(store.get_database(balance).unwrap().fields.is_empty());

        let reopened = Store::open(&path).unwrap();
        assert!(reopened.get_database(card).unwrap().fields.is_empty());
        assert!(reopened.get_database(balance).unwrap().fields.is_empty());
    }

    #[test]
    fn remove_scalar_field_drops_its_column() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();

        let card = store.create_database("Card").unwrap();
        let name_field = store.add_field(card, "Name", FieldKind::Text).unwrap();
        assert!(table_columns(&store.conn, &database_table_name(card)).contains(&field_column_name(name_field)));

        store.remove_field(card, name_field).unwrap();

        assert!(!table_columns(&store.conn, &database_table_name(card)).contains(&field_column_name(name_field)));
    }

    #[test]
    fn delete_database_removes_it_and_its_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);
        let mut store = Store::open(&path).unwrap();

        let card = store.create_database("Card").unwrap();
        store.add_field(card, "Name", FieldKind::Text).unwrap();
        store.create_record(card).unwrap();

        store.delete_database(card).unwrap();

        assert_eq!(store.get_database(card).unwrap_err(), DataLineError::DatabaseNotFound(card));
        assert!(!table_exists(&store.conn, &database_table_name(card)).unwrap());

        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.get_database(card).unwrap_err(), DataLineError::DatabaseNotFound(card));
    }

    #[test]
    fn delete_database_removes_fields_referencing_it_from_surviving_databases() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);
        let mut store = Store::open(&path).unwrap();

        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();
        store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        store.delete_database(balance).unwrap();

        assert!(store.get_database(card).unwrap().fields.is_empty());

        let reopened = Store::open(&path).unwrap();
        assert!(reopened.get_database(card).unwrap().fields.is_empty());
    }

    #[test]
    fn delete_missing_database_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let missing = DatabaseId::new();
        assert_eq!(store.delete_database(missing).unwrap_err(), DataLineError::DatabaseNotFound(missing));
    }

    #[test]
    fn retype_field_lossless_conversion_preserves_every_value() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let cost_field = store.add_field(card, "Cost", FieldKind::Number).unwrap();

        let fireball = store.create_record(card).unwrap();
        store.set_value(fireball, cost_field, Value::Number(3.0)).unwrap();

        let report = store.retype_field(card, cost_field, FieldKind::Text).unwrap();

        assert!(report.cleared_links.is_empty());
        assert_eq!(store.get_database(card).unwrap().field(cost_field).unwrap().kind, FieldKind::Text);
        assert_eq!(store.get_record(fireball).unwrap().values[&cost_field], Value::Text("3".into()));
    }

    /// The exact gap-2 migration path: an existing `Blob` field (Music's
    /// storage field before this feature existed) retypes onto its own
    /// plugin-tagged `CustomBlob` kind without losing or rewriting the
    /// underlying bytes — just gaining a `kind_id` tag. This is what lets
    /// a real, already-populated project adopt `CustomBlob` safely.
    #[test]
    fn retype_field_from_blob_to_custom_blob_is_lossless() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let track_field = store.add_field(card, "Track", FieldKind::Blob).unwrap();
        let link = store.create_record(card).unwrap();
        let blob_ref = store.write_blob(b"real audio bytes", Some("song.mp3")).unwrap();
        store.set_value(link, track_field, Value::Blob(blob_ref.clone())).unwrap();

        let report = store
            .retype_field(card, track_field, FieldKind::CustomBlob { kind_id: "com.myline.music.value".into() })
            .unwrap();

        assert!(report.cleared_links.is_empty(), "retyping Blob->CustomBlob must never clear a value");
        assert_eq!(
            store.get_database(card).unwrap().field(track_field).unwrap().kind,
            FieldKind::CustomBlob { kind_id: "com.myline.music.value".into() }
        );
        assert_eq!(
            store.get_record(link).unwrap().values[&track_field],
            Value::CustomBlob { kind_id: "com.myline.music.value".into(), blob: blob_ref }
        );
    }

    #[test]
    fn retype_field_reports_records_it_had_to_clear() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let name_field = store.add_field(card, "Name", FieldKind::Text).unwrap();

        let numeric = store.create_record(card).unwrap();
        store.set_value(numeric, name_field, Value::Text("3".into())).unwrap();
        let non_numeric = store.create_record(card).unwrap();
        store.set_value(non_numeric, name_field, Value::Text("Fireball".into())).unwrap();
        let already_empty = store.create_record(card).unwrap();

        let report = store.retype_field(card, name_field, FieldKind::Number).unwrap();

        assert_eq!(report.cleared_links, vec![non_numeric], "only the non-numeric value should be reported cleared");
        assert_eq!(store.get_record(numeric).unwrap().values[&name_field], Value::Number(3.0));
        assert_eq!(store.get_record(non_numeric).unwrap().values[&name_field], Value::Empty);
        assert_eq!(
            store.get_record(already_empty).unwrap().values[&name_field],
            Value::Empty,
            "a value that was already empty isn't a data-loss event"
        );
    }

    #[test]
    fn retype_field_narrowing_selection_options_clears_orphaned_selections() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let rarity_field = store
            .add_field(
                card,
                "Rarity",
                FieldKind::Selection {
                    multi: false,
                    options: vec![
                        SelectionOption { name: "Common".into(), color: "gray".into() },
                        SelectionOption { name: "Rare".into(), color: "blue".into() },
                    ],
                },
            )
            .unwrap();

        let rare_card = store.create_record(card).unwrap();
        store.set_value(rare_card, rarity_field, Value::Selection(vec!["Rare".into()])).unwrap();

        let report = store
            .retype_field(
                card,
                rarity_field,
                FieldKind::Selection {
                    multi: false,
                    options: vec![SelectionOption { name: "Common".into(), color: "gray".into() }],
                },
            )
            .unwrap();

        assert_eq!(report.cleared_links, vec![rare_card]);
        assert_eq!(store.get_record(rare_card).unwrap().values[&rarity_field], Value::Empty);
    }

    #[test]
    fn retype_field_rejects_reference_field() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let balance = store.create_database("Balance change").unwrap();
        let ref_field = store
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        let err = store.retype_field(card, ref_field, FieldKind::Text).unwrap_err();
        assert_eq!(err, DataLineError::ReferenceFieldRetypeNotSupported(ref_field));
        // Untouched.
        assert!(matches!(
            store.get_database(card).unwrap().field(ref_field).unwrap().kind,
            FieldKind::Reference { .. }
        ));
    }

    #[test]
    fn retype_field_missing_field_errors_and_leaves_store_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp_store_path(&dir)).unwrap();
        let card = store.create_database("Card").unwrap();
        let missing = FieldId::new();

        let err = store.retype_field(card, missing, FieldKind::Text).unwrap_err();
        assert_eq!(err, DataLineError::FieldNotFound(missing));
    }

    #[test]
    fn retype_field_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);

        let (card, field_id, fireball) = {
            let mut store = Store::open(&path).unwrap();
            let card = store.create_database("Card").unwrap();
            let field_id = store.add_field(card, "Cost", FieldKind::Number).unwrap();
            let fireball = store.create_record(card).unwrap();
            store.set_value(fireball, field_id, Value::Number(3.0)).unwrap();
            store.retype_field(card, field_id, FieldKind::Text).unwrap();
            (card, field_id, fireball)
        };

        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.get_database(card).unwrap().field(field_id).unwrap().kind, FieldKind::Text);
        assert_eq!(reopened.get_record(fireball).unwrap().values[&field_id], Value::Text("3".into()));
    }

    #[test]
    fn duplicate_database_schema_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);

        let duplicate = {
            let mut store = Store::open(&path).unwrap();
            let card = store.create_database("Card").unwrap();
            store.add_field(card, "Name", FieldKind::Text).unwrap();
            store.duplicate_database_schema(card, "Card Copy").unwrap()
        };

        let reopened = Store::open(&path).unwrap();
        let dup_db = reopened.get_database(duplicate).unwrap();
        assert_eq!(dup_db.name, "Card Copy");
        assert_eq!(dup_db.fields.len(), 1);
        assert_eq!(dup_db.fields[0].name, "Name");
    }

    #[test]
    fn rename_field_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);

        let (card, field_id) = {
            let mut store = Store::open(&path).unwrap();
            let card = store.create_database("Card").unwrap();
            let field_id = store.add_field(card, "Name", FieldKind::Text).unwrap();
            store.rename_field(card, field_id, "Title").unwrap();
            (card, field_id)
        };

        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.get_database(card).unwrap().field(field_id).unwrap().name, "Title");
    }

    #[test]
    fn mutation_error_does_not_touch_the_store_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);
        let mut store = Store::open(&path).unwrap();
        let card = store.create_database("Card").unwrap();

        let missing = DatabaseId::new();
        let err = store.add_field(card, "Broken", FieldKind::Reference { target_database: missing, paired_field: None });
        assert!(err.is_err());
        assert!(store.get_database(card).unwrap().fields.is_empty(), "failed mutation must not leave a partial field");

        let reopened = Store::open(&path).unwrap();
        assert!(reopened.get_database(card).unwrap().fields.is_empty());
    }

    #[test]
    fn opening_a_v1_store_migrates_blobs_table_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);

        // Hand-build a v1 store (blobs table with no filename column,
        // schema_version 1) — what every store on disk before this
        // migration was shaped like.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE _dataline_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE databases (id TEXT PRIMARY KEY, name TEXT NOT NULL);
                 CREATE TABLE fields (
                     id TEXT PRIMARY KEY, database_id TEXT NOT NULL REFERENCES databases(id),
                     name TEXT NOT NULL, kind TEXT NOT NULL, selection_multi INTEGER,
                     selection_options TEXT, reference_target_database TEXT,
                     reference_paired_field TEXT, ordering INTEGER NOT NULL
                 );
                 CREATE TABLE links (link TEXT PRIMARY KEY, database_id TEXT NOT NULL REFERENCES databases(id));
                 CREATE TABLE blobs (hash TEXT PRIMARY KEY, bytes BLOB NOT NULL, created_at TEXT NOT NULL);
                 INSERT INTO _dataline_meta (key, value) VALUES ('schema_version', '1');",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO blobs (hash, bytes, created_at) VALUES ('abc', X'01020304', '2024-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        }

        let mut store = Store::open(&path).unwrap();
        // The migrated column is usable — a fresh write with a filename
        // round-trips, and the pre-migration row (filename NULL) still reads
        // back fine as `None`.
        let blob_ref = store.write_blob(b"new bytes", Some("new.bin")).unwrap();
        assert_eq!(blob_ref.filename(), Some("new.bin"));

        let conn = Connection::open(&path).unwrap();
        let version: String =
            conn.query_row("SELECT value FROM _dataline_meta WHERE key = 'schema_version'", [], |row| row.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);
        {
            let _store = Store::open(&path).unwrap();
        }

        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE _dataline_meta SET value = '999' WHERE key = 'schema_version'", []).unwrap();
        drop(conn);

        match Store::open(&path) {
            Err(DataLineError::Storage(msg)) => assert!(msg.contains("999")),
            Err(other) => panic!("expected Storage error, got {other:?}"),
            Ok(_) => panic!("expected schema version mismatch to be rejected"),
        }
    }

    #[test]
    fn opening_a_store_enables_wal_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);
        let store = Store::open(&path).unwrap();

        let mode: String = store.conn.query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
        assert_eq!(mode.to_lowercase(), "wal");

        let sync: i64 = store.conn.query_row("PRAGMA synchronous", [], |row| row.get(0)).unwrap();
        assert_eq!(sync, 1, "synchronous should be NORMAL (1)");
    }

    #[test]
    fn checkpoint_truncates_the_wal_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_store_path(&dir);
        let mut store = Store::open(&path).unwrap();

        let db_id = store.create_database("Notes").unwrap();
        store.add_field(db_id, "Title", FieldKind::Text).unwrap();

        store.checkpoint().unwrap();

        let wal_path = path.with_extension("sqlite-wal");
        if wal_path.exists() {
            let size = std::fs::metadata(&wal_path).unwrap().len();
            assert_eq!(size, 0, "checkpoint should truncate the WAL file");
        }
    }

    fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{table}\")")).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }
}
