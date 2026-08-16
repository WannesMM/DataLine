use std::collections::{HashMap, HashSet};

use crate::error::{DataLineError, Result};
use crate::ids::{DatabaseId, FieldId};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SelectionOption {
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Text,
    Number,
    Boolean,
    Selection { multi: bool, options: Vec<SelectionOption> },
    Date,
    /// `paired_field` is always established internally by [`SchemaRegistry::add_field`] —
    /// a caller-supplied value is ignored, since a reference field is never created
    /// without its back-reference counterpart. See Architecture.md §4.
    Reference { target_database: DatabaseId, paired_field: Option<FieldId> },
    Blob,
    /// See [`crate::record::Value::Json`].
    Json,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldDefinition {
    pub id: FieldId,
    pub name: String,
    pub kind: FieldKind,
    pub ordering: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Database {
    pub id: DatabaseId,
    pub name: String,
    pub fields: Vec<FieldDefinition>,
}

impl Database {
    pub fn field(&self, id: FieldId) -> Option<&FieldDefinition> {
        self.fields.iter().find(|f| f.id == id)
    }

    fn field_mut(&mut self, id: FieldId) -> Option<&mut FieldDefinition> {
        self.fields.iter_mut().find(|f| f.id == id)
    }
}

/// In-memory schema catalog. `Store` (Architecture.md §3) wraps this same
/// mutation logic with SQLite-backed persistence — every method here stays
/// storage-agnostic on purpose.
#[derive(Debug, Default, Clone)]
pub struct SchemaRegistry {
    databases: HashMap<DatabaseId, Database>,
}

impl SchemaRegistry {
    pub fn new() -> Self {
        Self { databases: HashMap::new() }
    }

    /// Rebuilds a registry from fully-formed `Database`s already carrying their
    /// original ids — used only by `Store` to rehydrate from a store file that
    /// already has schema rows, never for fresh in-memory use (which goes
    /// through `create_database`/`add_field` so pairing stays consistent).
    pub(crate) fn from_databases(databases: Vec<Database>) -> Self {
        Self { databases: databases.into_iter().map(|db| (db.id, db)).collect() }
    }

    pub fn create_database(&mut self, name: impl Into<String>) -> DatabaseId {
        let id = DatabaseId::new();
        self.databases.insert(id, Database { id, name: name.into(), fields: Vec::new() });
        id
    }

    /// Removes a database, plus every field on any *other* database that
    /// referenced it — a Reference field never points at a database that no
    /// longer exists, so removing one bulk-removes every field pointing at
    /// it, the same guarantee `remove_field` gives for a single pair. Fields
    /// the removed database owned (including its own paired back-references
    /// to other databases, and any self-referencing pair) go with it
    /// automatically since the whole `Database` entry is dropped.
    pub fn remove_database(&mut self, database_id: DatabaseId) -> Result<()> {
        self.get_database(database_id)?;
        self.databases.remove(&database_id);
        for database in self.databases.values_mut() {
            database.fields.retain(|field| {
                !matches!(&field.kind, FieldKind::Reference { target_database, .. } if *target_database == database_id)
            });
        }
        Ok(())
    }

    pub fn get_database(&self, id: DatabaseId) -> Result<&Database> {
        self.databases.get(&id).ok_or(DataLineError::DatabaseNotFound(id))
    }

    pub fn list_databases(&self) -> Vec<&Database> {
        self.databases.values().collect()
    }

    /// Finds a field by id alone, without knowing its owning database up
    /// front — needed by reference operations that only have a `FieldId` to
    /// go on (e.g. `Store::delete_reference`). Schema is small and fully
    /// in-memory, so a linear scan is fine; this isn't a hot path.
    pub fn find_field(&self, field_id: FieldId) -> Option<(DatabaseId, &FieldDefinition)> {
        self.databases.values().find_map(|db| db.field(field_id).map(|f| (db.id, f)))
    }

    /// Adds a field to `database_id`. If `kind` is [`FieldKind::Reference`], the
    /// paired back-reference field is created on the target database automatically
    /// (Architecture.md §4) — including when the target is `database_id` itself.
    pub fn add_field(
        &mut self,
        database_id: DatabaseId,
        name: impl Into<String>,
        kind: FieldKind,
    ) -> Result<FieldId> {
        self.get_database(database_id)?;

        let name = name.into();
        let field_id = FieldId::new();
        let ordering = self.databases[&database_id].fields.len() as i32;

        let target_database = match &kind {
            FieldKind::Reference { target_database, .. } => {
                self.get_database(*target_database)?;
                Some(*target_database)
            }
            _ => None,
        };

        // paired_field is always established below, never trusted from the caller.
        let forward_kind = match kind {
            FieldKind::Reference { target_database, .. } => {
                FieldKind::Reference { target_database, paired_field: None }
            }
            other => other,
        };

        let field = FieldDefinition { id: field_id, name: name.clone(), kind: forward_kind, ordering };
        self.databases.get_mut(&database_id).unwrap().fields.push(field);

        if let Some(target_database) = target_database {
            let back_name = format!("{} (via {})", self.databases[&database_id].name, name);
            let back_ordering = self.databases[&target_database].fields.len() as i32;
            let back_id = FieldId::new();
            let back_field = FieldDefinition {
                id: back_id,
                name: back_name,
                kind: FieldKind::Reference { target_database: database_id, paired_field: Some(field_id) },
                ordering: back_ordering,
            };
            self.databases.get_mut(&target_database).unwrap().fields.push(back_field);

            let forward = self.databases.get_mut(&database_id).unwrap().field_mut(field_id).unwrap();
            if let FieldKind::Reference { paired_field, .. } = &mut forward.kind {
                *paired_field = Some(back_id);
            }
        }

        Ok(field_id)
    }

    pub fn rename_field(
        &mut self,
        database_id: DatabaseId,
        field_id: FieldId,
        name: impl Into<String>,
    ) -> Result<()> {
        let database =
            self.databases.get_mut(&database_id).ok_or(DataLineError::DatabaseNotFound(database_id))?;
        let field = database.field_mut(field_id).ok_or(DataLineError::FieldNotFound(field_id))?;
        field.name = name.into();
        Ok(())
    }

    /// Changes a field's kind in place — id, name, and ordering untouched.
    /// Rejects `Reference` on either side: retyping into or out of a
    /// reference field would mean creating or dropping a join table and
    /// migrating a fundamentally different kind of data (links, not scalar
    /// values), not just re-coercing a value — real scope of its own, not
    /// something to fold into this. Value migration for the records that
    /// already exist under the old kind is `Store::retype_field`'s job
    /// (Architecture.md §7) — this method only ever touches schema.
    pub fn retype_field(&mut self, database_id: DatabaseId, field_id: FieldId, new_kind: FieldKind) -> Result<()> {
        let database =
            self.databases.get_mut(&database_id).ok_or(DataLineError::DatabaseNotFound(database_id))?;
        let field = database.field_mut(field_id).ok_or(DataLineError::FieldNotFound(field_id))?;

        if matches!(field.kind, FieldKind::Reference { .. }) || matches!(new_kind, FieldKind::Reference { .. }) {
            return Err(DataLineError::ReferenceFieldRetypeNotSupported(field_id));
        }

        field.kind = new_kind;
        Ok(())
    }

    /// Removes a field. If it's one side of a reference pair, the paired field on
    /// the other database is removed with it — a reference field never survives
    /// without its counterpart. See Architecture.md §4.
    pub fn remove_field(&mut self, database_id: DatabaseId, field_id: FieldId) -> Result<()> {
        let database = self.get_database(database_id)?;
        let field = database.field(field_id).ok_or(DataLineError::FieldNotFound(field_id))?;

        let paired = match &field.kind {
            FieldKind::Reference { target_database, paired_field: Some(paired_field) } => {
                Some((*target_database, *paired_field))
            }
            _ => None,
        };

        self.databases.get_mut(&database_id).unwrap().fields.retain(|f| f.id != field_id);

        if let Some((paired_database, paired_field_id)) = paired {
            if let Some(db) = self.databases.get_mut(&paired_database) {
                db.fields.retain(|f| f.id != paired_field_id);
            }
        }

        Ok(())
    }

    /// Copies `database_id`'s field structure into a new, independent database —
    /// the reuse mechanism in place of a live-shared Type (Architecture.md §2).
    /// A reference field pointing at `database_id` itself is remapped to point at
    /// the new database instead, so a self-referencing schema stays self-referencing
    /// after duplication rather than pointing back at the original.
    ///
    /// A self-referencing pair occupies *two* entries in `database_id`'s own field
    /// list (the forward field and its auto-created back-reference both live there —
    /// see `add_field`). Duplicating each independently would double-create the
    /// pair, so once the forward side has been recreated, its paired sibling is
    /// skipped rather than processed again.
    pub fn duplicate_database_schema(
        &mut self,
        database_id: DatabaseId,
        new_name: impl Into<String>,
    ) -> Result<DatabaseId> {
        let source_fields: Vec<FieldDefinition> = {
            let source = self.get_database(database_id)?;
            source.fields.clone()
        };

        let new_id = self.create_database(new_name);
        let mut already_paired = HashSet::new();

        for field in &source_fields {
            if already_paired.contains(&field.id) {
                continue;
            }

            match &field.kind {
                FieldKind::Reference { target_database, paired_field } => {
                    let target = if *target_database == database_id { new_id } else { *target_database };
                    self.add_field(
                        new_id,
                        field.name.clone(),
                        FieldKind::Reference { target_database: target, paired_field: None },
                    )?;
                    if *target_database == database_id {
                        if let Some(paired) = paired_field {
                            already_paired.insert(*paired);
                        }
                    }
                }
                other => {
                    self.add_field(new_id, field.name.clone(), other.clone())?;
                }
            }
        }

        Ok(new_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_database_and_add_scalar_fields() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");

        registry.add_field(card, "Name", FieldKind::Text).unwrap();
        registry.add_field(card, "Cost", FieldKind::Number).unwrap();

        let db = registry.get_database(card).unwrap();
        assert_eq!(db.fields.len(), 2);
        assert_eq!(db.fields[0].name, "Name");
        assert_eq!(db.fields[0].ordering, 0);
        assert_eq!(db.fields[1].ordering, 1);
    }

    #[test]
    fn add_reference_field_creates_paired_back_reference() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");
        let balance = registry.create_database("Balance change");

        let forward_id = registry
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        let card_db = registry.get_database(card).unwrap();
        let balance_db = registry.get_database(balance).unwrap();

        assert_eq!(balance_db.fields.len(), 1);
        let back_field = &balance_db.fields[0];
        assert_eq!(back_field.name, "Card (via Balance change)");

        match &back_field.kind {
            FieldKind::Reference { target_database, paired_field } => {
                assert_eq!(*target_database, card);
                assert_eq!(*paired_field, Some(forward_id));
            }
            other => panic!("expected Reference, got {other:?}"),
        }

        match &card_db.field(forward_id).unwrap().kind {
            FieldKind::Reference { target_database, paired_field } => {
                assert_eq!(*target_database, balance);
                assert_eq!(*paired_field, Some(back_field.id));
            }
            other => panic!("expected Reference, got {other:?}"),
        }
    }

    #[test]
    fn add_reference_field_ignores_caller_supplied_paired_field() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");
        let balance = registry.create_database("Balance change");

        let bogus = FieldId::new();
        let forward_id = registry
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: Some(bogus) })
            .unwrap();

        let card_db = registry.get_database(card).unwrap();
        match &card_db.field(forward_id).unwrap().kind {
            FieldKind::Reference { paired_field, .. } => assert_ne!(*paired_field, Some(bogus)),
            other => panic!("expected Reference, got {other:?}"),
        }
    }

    #[test]
    fn remove_reference_field_removes_paired_field() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");
        let balance = registry.create_database("Balance change");

        let forward_id = registry
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        registry.remove_field(card, forward_id).unwrap();

        assert!(registry.get_database(card).unwrap().fields.is_empty());
        assert!(registry.get_database(balance).unwrap().fields.is_empty());
    }

    #[test]
    fn self_referencing_reference_field() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");

        let forward_id = registry
            .add_field(card, "Related Cards", FieldKind::Reference { target_database: card, paired_field: None })
            .unwrap();

        let card_db = registry.get_database(card).unwrap();
        assert_eq!(card_db.fields.len(), 2, "forward and back field both land on the same database");

        let forward = card_db.field(forward_id).unwrap();
        let back_id = match &forward.kind {
            FieldKind::Reference { paired_field: Some(id), .. } => *id,
            other => panic!("expected paired Reference, got {other:?}"),
        };
        let back = card_db.field(back_id).unwrap();
        match &back.kind {
            FieldKind::Reference { paired_field, .. } => assert_eq!(*paired_field, Some(forward_id)),
            other => panic!("expected Reference, got {other:?}"),
        }
    }

    #[test]
    fn self_referencing_field_removal_cleans_up_both_sides() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");

        let forward_id = registry
            .add_field(card, "Related Cards", FieldKind::Reference { target_database: card, paired_field: None })
            .unwrap();

        registry.remove_field(card, forward_id).unwrap();
        assert!(registry.get_database(card).unwrap().fields.is_empty());
    }

    #[test]
    fn duplicate_database_schema_copies_scalar_fields_and_repairs_external_references() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");
        let balance = registry.create_database("Balance change");

        registry.add_field(card, "Name", FieldKind::Text).unwrap();
        registry
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        let duplicate = registry.duplicate_database_schema(card, "Card Copy").unwrap();
        let dup_db = registry.get_database(duplicate).unwrap();

        assert_eq!(dup_db.fields.len(), 2);
        assert_eq!(dup_db.fields[0].name, "Name");

        match &dup_db.fields[1].kind {
            FieldKind::Reference { target_database, .. } => assert_eq!(*target_database, balance),
            other => panic!("expected Reference, got {other:?}"),
        }

        // Balance change now has two independent back-reference fields: one for
        // the original Card database, one for the duplicate.
        let balance_db = registry.get_database(balance).unwrap();
        assert_eq!(balance_db.fields.len(), 2);
    }

    #[test]
    fn duplicate_database_schema_remaps_self_reference_to_the_new_database() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");
        registry
            .add_field(card, "Related Cards", FieldKind::Reference { target_database: card, paired_field: None })
            .unwrap();

        let duplicate = registry.duplicate_database_schema(card, "Card Copy").unwrap();
        let dup_db = registry.get_database(duplicate).unwrap();

        assert_eq!(dup_db.fields.len(), 2, "duplicate gets its own self-referencing pair, not a link back to the original");
        for field in &dup_db.fields {
            match &field.kind {
                FieldKind::Reference { target_database, .. } => assert_eq!(*target_database, duplicate),
                other => panic!("expected Reference, got {other:?}"),
            }
        }

        // The original Card database is untouched.
        assert_eq!(registry.get_database(card).unwrap().fields.len(), 2);
    }

    #[test]
    fn add_field_to_missing_database_errors() {
        let mut registry = SchemaRegistry::new();
        let missing = DatabaseId::new();
        let err = registry.add_field(missing, "Name", FieldKind::Text).unwrap_err();
        assert_eq!(err, DataLineError::DatabaseNotFound(missing));
    }

    #[test]
    fn add_reference_field_to_missing_target_errors() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");
        let missing = DatabaseId::new();

        let err = registry
            .add_field(card, "Broken", FieldKind::Reference { target_database: missing, paired_field: None })
            .unwrap_err();
        assert_eq!(err, DataLineError::DatabaseNotFound(missing));
        assert!(registry.get_database(card).unwrap().fields.is_empty(), "no partial field left behind");
    }

    #[test]
    fn remove_database_removes_it() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");
        registry.remove_database(card).unwrap();
        assert_eq!(registry.get_database(card).unwrap_err(), DataLineError::DatabaseNotFound(card));
    }

    #[test]
    fn remove_database_removes_fields_referencing_it_from_other_databases() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");
        let balance = registry.create_database("Balance change");
        registry
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        registry.remove_database(balance).unwrap();

        assert!(registry.get_database(card).unwrap().fields.is_empty(), "the paired back-reference field on the surviving database is gone too");
    }

    #[test]
    fn remove_database_with_self_reference_leaves_no_trace() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");
        registry
            .add_field(card, "Related Cards", FieldKind::Reference { target_database: card, paired_field: None })
            .unwrap();

        registry.remove_database(card).unwrap();
        assert_eq!(registry.list_databases().len(), 0);
    }

    #[test]
    fn remove_missing_database_errors() {
        let mut registry = SchemaRegistry::new();
        let missing = DatabaseId::new();
        assert_eq!(registry.remove_database(missing).unwrap_err(), DataLineError::DatabaseNotFound(missing));
    }

    #[test]
    fn get_database_missing_errors() {
        let registry = SchemaRegistry::new();
        let missing = DatabaseId::new();
        assert_eq!(registry.get_database(missing).unwrap_err(), DataLineError::DatabaseNotFound(missing));
    }

    #[test]
    fn rename_field_updates_name_only() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");
        let field_id = registry.add_field(card, "Name", FieldKind::Text).unwrap();

        registry.rename_field(card, field_id, "Title").unwrap();

        assert_eq!(registry.get_database(card).unwrap().field(field_id).unwrap().name, "Title");
    }

    #[test]
    fn retype_field_changes_kind_leaves_id_name_ordering_alone() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");
        let field_id = registry.add_field(card, "Cost", FieldKind::Number).unwrap();

        registry.retype_field(card, field_id, FieldKind::Text).unwrap();

        let field = registry.get_database(card).unwrap().field(field_id).unwrap();
        assert_eq!(field.kind, FieldKind::Text);
        assert_eq!(field.id, field_id);
        assert_eq!(field.name, "Cost");
        assert_eq!(field.ordering, 0);
    }

    #[test]
    fn retype_field_rejects_reference_field() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");
        let balance = registry.create_database("Balance change");
        let ref_field = registry
            .add_field(card, "Balance change", FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap();

        let err = registry.retype_field(card, ref_field, FieldKind::Text).unwrap_err();
        assert_eq!(err, DataLineError::ReferenceFieldRetypeNotSupported(ref_field));
    }

    #[test]
    fn retype_field_rejects_retyping_into_reference() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");
        let balance = registry.create_database("Balance change");
        let field_id = registry.add_field(card, "Name", FieldKind::Text).unwrap();

        let err = registry
            .retype_field(card, field_id, FieldKind::Reference { target_database: balance, paired_field: None })
            .unwrap_err();
        assert_eq!(err, DataLineError::ReferenceFieldRetypeNotSupported(field_id));

        // Untouched — the field is still Text, not left in some half-migrated state.
        assert_eq!(registry.get_database(card).unwrap().field(field_id).unwrap().kind, FieldKind::Text);
    }

    #[test]
    fn retype_field_missing_field_errors() {
        let mut registry = SchemaRegistry::new();
        let card = registry.create_database("Card");
        let missing = FieldId::new();

        let err = registry.retype_field(card, missing, FieldKind::Text).unwrap_err();
        assert_eq!(err, DataLineError::FieldNotFound(missing));
    }
}
