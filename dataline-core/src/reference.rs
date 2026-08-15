use crate::ids::{FieldId, Link, ReferenceId};

/// One bidirectional reference relationship between two records
/// (Architecture.md §4). `source_*`/`target_*` are relative to how the
/// `Reference` was asked for (created via, or listed via, `source_field`) —
/// storage itself doesn't distinguish a "source" and "target" side, since the
/// relationship is symmetric by construction (every Reference field is
/// auto-paired with a back-reference field, see `SchemaRegistry::add_field`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reference {
    pub id: ReferenceId,
    pub source_link: Link,
    pub source_field: FieldId,
    pub target_link: Link,
    pub target_field: FieldId,
}
