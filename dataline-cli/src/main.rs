//! Fast manual-iteration loop for dataline-core (Architecture.md §9) —
//! drives the schema module directly, no Swift or SQLite involved.

use dataline_core::{FieldKind, SchemaRegistry};

fn main() {
    let mut registry = SchemaRegistry::new();

    // LineBase's own Card / Balance change example (ProjectSpecification.md).
    let card = registry.create_database("Card");
    let balance_change = registry.create_database("Balance change");

    registry.add_field(card, "Name", FieldKind::Text).unwrap();
    registry.add_field(card, "Cost", FieldKind::Number).unwrap();
    registry
        .add_field(
            card,
            "Balance change",
            FieldKind::Reference { target_database: balance_change, paired_field: None },
        )
        .unwrap();
    registry
        .add_field(card, "Related Cards", FieldKind::Reference { target_database: card, paired_field: None })
        .unwrap();

    registry.add_field(balance_change, "Change", FieldKind::Text).unwrap();
    registry.add_field(balance_change, "Update version", FieldKind::Text).unwrap();

    for db in registry.list_databases() {
        println!("Database: {} ({})", db.name, db.id);
        for field in &db.fields {
            println!("  - {} : {:?}  [ordering {}]", field.name, field.kind, field.ordering);
        }
    }
}
