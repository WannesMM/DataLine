# DataLine — Usage Documentation

This is a practical guide to *using* DataLine, not a design document — see [ProjectSpecification.md](ProjectSpecification.md) for what DataLine is for and [Architecture.md](Architecture.md) for how it's built internally. Everything below is written against the real, working Swift API generated in build order step 8 — every code snippet here either matches or is a direct variation of code that has actually been compiled and run (see `xcframework/Sources/DataLineSmokeTest`).

The examples use the same running domain the rest of the project does — a `Card` database referencing a `Balance change` database — straight out of `LineBase`'s own spec.

## 1. The Mental Model

Four ideas, and everything else follows from them:

- **Database** — a typed table. Every record in it shares the same fields. (`Database`, `FieldDefinition`)
- **Field** — one column. Its `FieldKind` determines what kind of `Value` it can hold (`Text`, `Number`, `Boolean`, `Selection`, `Date`, `Reference`, `Blob`).
- **Record** — one row. Identified by its **Link** — a stable id, independent of the record's field values, generated once on creation and never reused.
- **Reference** — a special field kind. Creating one *always* creates its inverse field on the target database automatically — you never manage both sides by hand. See §4.

Every id in the API — database ids, field ids, links, reference ids, blob hashes — is a plain `String` (a UUID's string form, or a SHA-256 hex hash for blobs). Nothing here is an opaque object you need to manage a lifetime for; store these strings wherever you'd naturally store any other identifier.

## 2. Adding DataLine to a Swift Project

`DataLine/xcframework/` is a local Swift package. In Xcode: **File → Add Package Dependencies → Add Local...**, and point it at that folder. It exposes one library product, `DataLineFFI`.

```swift
import DataLineFFI
```

That gives you `Store`, `Database`, `FieldDefinition`, `FieldKind`, `Record`, `Reference`, `SelectionOption`, `Value`, `DataLineError`, and `datalineVersion()`.

## 3. Opening a Store

A `Store` owns one SQLite file. Open it once, keep it around (e.g. as a property on whatever object owns a project in your app) — every operation goes through it.

```swift
let store = try Store.open(path: projectURL.appendingPathComponent("project.dataline").path)
```

If the file doesn't exist yet, it's created with a fresh schema. If it does, its schema is loaded automatically — you don't re-declare anything on reopen.

## 4. Defining Your Schema

### Creating a database and adding fields

```swift
let card = try store.createDatabase(name: "Card")

let nameField = try store.addField(databaseId: card, name: "Name", kind: .text)
let costField = try store.addField(databaseId: card, name: "Cost", kind: .number)
let releasedField = try store.addField(databaseId: card, name: "Released", kind: .date)
let rarityField = try store.addField(
    databaseId: card, name: "Rarity",
    kind: .selection(multi: false, options: [
        SelectionOption(name: "Common", color: "gray"),
        SelectionOption(name: "Rare", color: "blue"),
    ])
)
let artworkField = try store.addField(databaseId: card, name: "Artwork", kind: .blob)
```

`FieldKind` cases map directly to what a field can store:

| `FieldKind` | Holds | Notes |
|---|---|---|
| `.text` | `.text(String)` | |
| `.number` | `.number(Double)` | |
| `.boolean` | `.boolean(Bool)` | |
| `.selection(multi:options:)` | `.selection([String])` | Single-select uses a 1-element array. |
| `.date` | `.date(String)` | `"YYYY-MM-DD"`. |
| `.reference(targetDatabase:pairedField:)` | *(not settable via `setValue`)* | See §5. |
| `.blob` | `.blob(String)` | A hash from `writeBlob`, not raw bytes. See §6. |

Fields are looked up by id everywhere, never by name — renaming a field (`renameField`) never breaks anything that already referred to it.

### Reference fields — the one field kind you don't fully control

```swift
let balance = try store.createDatabase(name: "Balance change")

let balanceField = try store.addField(
    databaseId: card, name: "Balance change",
    kind: .reference(targetDatabase: balance, pairedField: nil)
)
```

`pairedField: nil` above is always correct — you never fill it in yourself. This one call also creates a second field, on `Balance change`, pointing back at `Card`. Confirm it if you're curious:

```swift
let balanceDb = try store.getDatabase(databaseId: balance)
// balanceDb.fields now has one field: "Card (via Balance change)"
```

This is the single biggest difference from a typical ORM: **you never create a foreign key and its inverse separately.** One `addField` call with `.reference(...)` gives you both directions, and they always stay in sync — deleting the field removes both sides together, and deleting a record removes every reference row touching it, in both directions, automatically.

A reference field can point at its own database — that's how "Related Cards" style self-links work:

```swift
let relatedField = try store.addField(
    databaseId: card, name: "Related Cards",
    kind: .reference(targetDatabase: card, pairedField: nil)
)
```

### Reusing a schema

There's no "shared Type" you reference from multiple databases — deliberately, since that would mean editing one database's fields could silently change another's. Instead, copy a schema when you want to reuse it:

```swift
let cardTemplate = try store.duplicateDatabaseSchema(databaseId: card, newName: "Card (Expansion Set 2)")
```

The new database gets its own independent copy of every field, including a fresh, independent pairing for any reference fields (a self-reference in the original stays self-referencing in the copy, not pointing back at the original).

### Changing a field's kind

`retypeField` changes a field's `FieldKind` after it already has records — every existing value is migrated in the same operation, following the rules in the table below. It's never silent about data it can't preserve:

```swift
let report = try store.retypeField(databaseId: card, fieldId: costField, kind: .text)
// report.clearedLinks: [String] — any record whose value couldn't survive the change
```

| From → To | What happens |
|---|---|
| Same kind, compatible (e.g. `Text` → `Text`, or `Selection` → `Selection` with a superset of options) | Value unchanged. |
| Well-defined conversion (e.g. `Number` → `Text`, `Boolean` → `Number`) | Value converted. |
| Not representable (e.g. `Text` "hello" → `Number`) | Value cleared to `.empty`, and the record's link is added to `report.clearedLinks`. |

A value that was already unset before the change is never reported — only real data loss is. Check `report.clearedLinks` after a retype and surface it to the user if it's non-empty; DataLine won't do that for you.

`retypeField` doesn't support `Reference` fields on either side — retyping into or out of one would mean creating or dropping the underlying join table, which isn't the same operation as re-coercing a scalar value. Add a new field and migrate manually if you ever need that.

Removing an option from a `Selection` field is just a special case of this: call `retypeField` with the same field, same `.selection` kind, but a shorter `options` list — any record whose selected value isn't in the new list gets cleared and reported, same as any other retype.

## 5. Working with Records

### Create, set values, read back

```swift
let fireball = try store.createRecord(databaseId: card)

try store.setValue(link: fireball, fieldId: nameField, value: .text("Fireball"))
try store.setValue(link: fireball, fieldId: costField, value: .number(3))
try store.setValue(link: fireball, fieldId: rarityField, value: .selection(["Rare"]))

let record = try store.getRecord(link: fireball)
record.values[nameField]   // .text("Fireball")
record.link                // same string as `fireball`
record.createdAt           // RFC3339 timestamp string
```

`setValue` rejects a value whose kind doesn't match the field — passing `.number(3)` to a `.text` field throws `DataLineError.ValueKindMismatch`, not a silent coercion or a confusing SQL error. Passing `.empty` clears a field back to unset.

### Listing and deleting

```swift
let allCards = try store.listRecords(databaseId: card)   // every record in Card, oldest first
try store.deleteRecord(link: fireball)                      // also removes every reference touching it
```

## 6. Working with References

Once a reference field exists (§4), the operations are `createReference` / `listReferences` / `deleteReference` — none of them take the target's field, since it's always derivable from the field you *do* pass in.

```swift
let balanceEntry = try store.createRecord(databaseId: balance)

let reference = try store.createReference(
    sourceLink: fireball, sourceField: balanceField, targetLink: balanceEntry
)
// reference.id, reference.sourceLink, reference.targetLink, reference.targetField
```

`createReference` checks that `targetLink` actually belongs to the database the field points at — passing a link from the wrong database throws `DataLineError.ReferenceTargetDatabaseMismatch` rather than silently creating a broken reference.

Reading back — from either side, it's symmetric:

```swift
let fromCard = try store.listReferences(link: fireball, field: balanceField)
// [Reference] — fireball's outbound references through this field

let backField = try store.getDatabase(databaseId: balance).fields[0].id   // the auto-created back-reference field
let fromBalance = try store.listReferences(link: balanceEntry, field: backField)
// [Reference] — same relationship, seen from balanceEntry's side
```

Deleting one specific relationship:

```swift
try store.deleteReference(referenceId: reference.id, field: balanceField)
```

`field` can be either side of the pair — whichever one you happen to have on hand.

To see everything a record relates to, across *every* reference field at once (not just one), use `relatedRecords` instead — see §7.

## 7. Working with Blobs

Blobs (images, audio, any binary asset) are content-addressed: write the bytes once, get back a hash, and attach that hash to a `.blob` field. Writing the same bytes twice is free — you get the same hash back both times, with no duplicate storage.

```swift
let artworkBytes: Data = /* load a PNG, etc. */
let hash = try store.writeBlob(bytes: artworkBytes)

try store.setValue(link: fireball, fieldId: artworkField, value: .blob(hash))

// ...later, anywhere, even after reopening the store:
let record = try store.getRecord(link: fireball)
if case .blob(let hash) = record.values[artworkField] {
    let bytes = try store.readBlob(hash: hash)
}
```

`setValue` checks the hash actually exists before accepting it — a hash that was never written throws `DataLineError.BlobNotFound`, not a silently dangling reference.

There's no `deleteBlob` yet — safely removing a blob that might be shared by more than one record needs reference counting, which hasn't been built. Blobs currently only accumulate.

## 8. Querying

Three operations, each narrow and specific rather than a general query language:

```swift
// Exact match on one field.
let fireballs = try store.findByValue(databaseId: card, fieldId: nameField, value: .text("Fireball"))

// Case-insensitive substring match across every Text field in the database.
let results = try store.searchText(databaseId: card, query: "fire")

// Every record this one relates to, across *all* reference fields at once, flattened and deduplicated.
let related = try store.relatedRecords(link: fireball)   // [String] of links
```

`findByValue`/`searchText` reject `Reference` fields (there's no single "value" to compare — use `listReferences`/`relatedRecords` instead), but `findByValue` does work on `.blob` fields — useful for "which records use this exact asset."

These all scan the full record set in memory rather than using an indexed SQL lookup — correct and simple at the scale this has been built and tested at; worth revisiting only if it ever actually becomes a bottleneck.

## 9. Error Handling

Every fallible call `throws` a `DataLineError`. Swift's generated case names keep the PascalCase from the Rust source (unlike `Value`/`FieldKind`, which are lowercased):

```swift
do {
    try store.setValue(link: fireball, fieldId: costField, value: .text("not a number"))
} catch DataLineError.ValueKindMismatch(let field, let expected) {
    print("field \(field) expected a \(expected) value")
} catch DataLineError.RecordNotFound(let id) {
    print("no record with link \(id)")
} catch {
    print("unexpected: \(error)")
}
```

`DataLineError` also conforms to `LocalizedError`, so `error.localizedDescription` gives a readable message for any case without matching on it explicitly — useful for surfacing errors directly in UI while prototyping.

The full case list: `DatabaseNotFound`, `FieldNotFound`, `RecordNotFound`, `ReferenceNotFound`, `BlobNotFound`, `ValueKindMismatch`, `UnsupportedFieldKindForValue`, `ReferenceTargetDatabaseMismatch`, `Storage` (a catch-all for underlying SQLite failures — disk full, corrupted file, etc.).

## 10. A Complete Walkthrough

Everything above, in one pass — this is close to verbatim what `xcframework/Sources/DataLineSmokeTest/main.swift` actually runs and asserts against the real compiled library:

```swift
import DataLineFFI
import Foundation

let store = try Store.open(path: storeURL.path)

// Schema
let card = try store.createDatabase(name: "Card")
let balance = try store.createDatabase(name: "Balance change")

let nameField = try store.addField(databaseId: card, name: "Name", kind: .text)
let costField = try store.addField(databaseId: card, name: "Cost", kind: .number)
let artworkField = try store.addField(databaseId: card, name: "Artwork", kind: .blob)
let balanceField = try store.addField(
    databaseId: card, name: "Balance change",
    kind: .reference(targetDatabase: balance, pairedField: nil)
)

// Records
let fireball = try store.createRecord(databaseId: card)
try store.setValue(link: fireball, fieldId: nameField, value: .text("Fireball"))
try store.setValue(link: fireball, fieldId: costField, value: .number(3))

// Blob
let hash = try store.writeBlob(bytes: Data("...".utf8))
try store.setValue(link: fireball, fieldId: artworkField, value: .blob(hash))

// Reference
let change = try store.createRecord(databaseId: balance)
_ = try store.createReference(sourceLink: fireball, sourceField: balanceField, targetLink: change)

// Read it all back
let record = try store.getRecord(link: fireball)
let related = try store.relatedRecords(link: fireball)   // [change]
let matches = try store.findByValue(databaseId: card, fieldId: nameField, value: .text("Fireball"))
```

## 11. For Rust Consumers (PowerLine, or Line once it exists)

Everything above has a 1:1 counterpart in `dataline_core::Store`, used directly (no FFI boundary, no string ids — real `DatabaseId`/`FieldId`/`Link` types):

```rust
use dataline_core::{FieldKind, Store, Value};

let mut store = Store::open("project.dataline")?;
let card = store.create_database("Card")?;
let name_field = store.add_field(card, "Name", FieldKind::Text)?;
let fireball = store.create_record(card)?;
store.set_value(fireball, name_field, Value::Text("Fireball".into()))?;
```

`dataline-cli` (`cargo run -p dataline-cli`) is a small runnable example of exactly this, useful for trying something out without touching Swift or Xcode at all.

## 12. Current Limitations

Worth knowing before you design around this:

- **Blobs are never garbage-collected.** Every unique asset written accumulates in the store file permanently — there's no `deleteBlob`. Safely removing one needs reference counting across every record that might share it, which hasn't been built; deferred until it's a real problem, not before.
- **iOS isn't packaged.** The XCFramework currently only ships universal macOS (arm64 + x86_64) binaries — nothing in the ecosystem needs iOS yet.
- **`findByValue`/`searchText` do a full scan**, not an indexed lookup. Fine at the scale this has been exercised at; may need revisiting at very large record counts.
- **There's no Rust-native FFI adapter yet.** A future non-Swift Rust consumer (a Line-native app, for instance) shouldn't link `dataline-core` as a source dependency — the project's own philosophy calls for a precompiled, dynamically-loaded artifact there too, just via a different boundary than `dataline-ffi` (which is specifically a Swift/UniFFI adapter). That boundary doesn't exist yet; deferred until there's a real Rust consumer to build it against instead of guessing its shape.
