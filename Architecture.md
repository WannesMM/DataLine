# DataLine — Architecture

This document maps `ProjectSpecification.md` onto a concrete, buildable Rust workspace. It is an implementation plan, not a spec revision — where a decision below narrows something the spec left open, it's called out explicitly in **Open Questions for Review** rather than assumed silently.

It was written after reading BaseLine's current Swift/SwiftData model and service layer (`Models/`, `Services/`, `Types/`), since that code is today's only working precedent for what this data model needs to support in practice. BaseLine currently implements its own database engine directly in SwiftData — schema (`DatabaseDefinition`/`FieldDefinition`), records (`DatabaseEntry`/`FieldValueEntry`), and bidirectional references (`Reference`/`ReferenceService`). That's exactly the surface DataLine needs to take over, so BaseLine's `SchemaManager`, `EntryService`, and `ReferenceService` are treated below as a functional checklist, not as a design to port as-is — its storage strategy (JSON-blob EAV rows) is specifically what the spec asks DataLine to improve on.

## 1. Workspace Layout

```
DataLine/
  Cargo.toml                 # workspace root
  dataline-core/              # pure Rust engine: schema, records, references, query, migrations, sqlite
    src/
      lib.rs
      schema.rs               # Database, FieldDefinition, FieldKind
      record.rs                # Record, Link
      value.rs                 # Value enum, coercion/validity rules
      reference.rs              # bidirectional reference engine
      query.rs                  # filter/search/traversal operations
      store.rs                  # Store: owns the SQLite connection, opened once per spec
      migration.rs               # schema-change migrations
      error.rs
    tests/                      # integration tests against dataline-core directly
  dataline-ffi/                # UniFFI boundary crate — the only thing consumers ever link against
    src/lib.rs                 # #[uniffi::export] wrapper around dataline-core
    Cargo.toml                 # crate-type = ["cdylib", "staticlib"]
  dataline-cli/                 # tiny example binary exercising dataline-core directly, no Swift needed
  xcframework/                  # build script + output for the Swift-facing artifact
  ProjectSpecification.md
  Architecture.md
```

`dataline-core` is a normal Cargo library crate — testable in isolation with `cargo test`, no FFI concerns at all. `dataline-ffi` is deliberately thin: it wraps `dataline-core` types behind UniFFI objects and does nothing else. Per the spec, no consumer — Swift or Rust — depends on `dataline-core` as a source-level crate; everything goes through the compiled `dataline-ffi` artifact (XCFramework for Swift, a loaded dylib for Rust hosts). The core/ffi split exists purely so the engine logic stays unit-testable and free of FFI ceremony during development; it is an internal implementation detail, not a second public contract.

## 2. Core Data Model

```rust
pub struct Link(Uuid);              // spec: stable identity, independent of field values

pub struct Database {
    pub id: DatabaseId,
    pub name: String,
    pub fields: Vec<FieldDefinition>,   // ordered
}

pub struct FieldDefinition {
    pub id: FieldId,
    pub name: String,
    pub kind: FieldKind,
    pub ordering: i32,
}

pub enum FieldKind {
    Text,
    Number,
    Boolean,
    Selection { multi: bool, options: Vec<SelectionOption> },
    Date,
    Reference { target_database: DatabaseId, paired_field: Option<FieldId> },
    Blob,   // opaque binary — images, audio, arbitrary assets
}

pub enum Value {
    Text(String),
    Number(f64),
    Boolean(bool),
    Selection(Vec<String>),     // single-select uses a 1-element Vec
    Date(NaiveDate),
    Reference(Vec<Link>),
    Blob(BlobRef),               // content hash, resolved against this store's own blobs table — see §5
    Empty,
}

pub struct Record {
    pub link: Link,
    pub database_id: DatabaseId,
    pub values: HashMap<FieldId, Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

`Database` fuses what the spec calls a "Type" directly into the database it backs, rather than modeling Type as a separate entity multiple databases could share live. A shared, simultaneously-editable schema across databases means editing one database's fields could silently change another's — surprising behavior that works against ease of use. Schema *reuse* is still supported, just via duplication rather than a live link: `duplicate_database_schema(database_id) -> Database` (§6) copies a database's field structure into a new, independent database. This gives most of the versatility a shared Type would, without the cascading-edit risk. Decided 2026-08-12.

`page` and `image`, which exist in BaseLine's current `FieldType` enum, are deliberately absent from `FieldKind`. `image` is just a `Blob`. `page` is a BaseLine presentation concept (which page instance is attached to a record) — it has no place in DataLine's schema per the spec's non-goal on presentation. Confirmed 2026-08-12; BaseLine will represent "this record has a page" off the record's own `Link` directly, without a dedicated schema field.

## 3. Persistence (SQLite via `rusqlite`)

Per spec: *"Scalar fields are stored as native, indexed SQLite columns... Reference fields are stored via dedicated join tables."* Concretely:

- One SQLite file per opened `Store` (a `Store` is opened once and handed around, per spec — never a pile of free functions over implicit global state).
- A metadata table (`_dataline_meta`) holding a `schema_version` integer, checked on open, and a `databases` + `fields` table holding schema definitions (name, kind, options-as-JSON).
- **Each `Database` gets its own real SQLite table** (`db_<database_id>`), with one native column per scalar field (`TEXT`, `REAL`, `INTEGER` for booleans, `TEXT` for dates in ISO-8601), plus a `link` primary key column. Adding/removing a field is a plain `ALTER TABLE ADD/DROP COLUMN` against that table, not a rewrite of a JSON blob — this is the concrete improvement over BaseLine's current `FieldValueEntry.rawValue: Data` (JSON-encoded `FieldValue`) approach, and is what makes indexing, sorting, and filtering by field value fast at the volumes the spec targets ("thousands of records and beyond"). **Retyping** a field is *not* a simple `ALTER TABLE`, though — see the correction under §7.
- **Reference fields never get a column on the owning table.** Each reference *relationship* (a paired forward/back field) gets its own join table: `ref_<field_id> (source_link TEXT, target_link TEXT)`, with both directions queryable by indexing both columns. This is what lets a single reference field hold multiple values without contaminating the scalar schema, per spec.
- **Blobs** are not stored as inline SQLite `BLOB` columns — see §5.

## 4. Reference Fields — Bidirectional Consistency

This is called out in the spec as the load-bearing logic that justifies a shared engine existing at all, so it gets first-class schema support rather than being assembled by hand at the call site (which is how BaseLine's `ReferenceService.createReference` works today — it requires the caller to already have both a source *and* target field, created independently, and just links their values).

Proposed model: creating a `Reference` field on Database A pointing at Database B **automatically creates the paired back-reference field on B** as part of the same schema-mutation call — the two `FieldDefinition`s carry each other's `FieldId` in `paired_field`. `create_reference(source_link, source_field, target_link) -> Reference` then only needs one field, not two, and writes both sides of the join table row atomically. Deleting a reference, or deleting a record that participates in one, removes both sides in the same transaction. This removes an entire class of "forgot to wire up the other side" bugs that the current hand-assembled version is exposed to.

This is a real behavior change from BaseLine's current implementation, not just a translation of it — confirmed 2026-08-12 as the more user-friendly direction.

## 5. Blobs / Assets

BaseLine currently stores images as `.image(Data)` — the raw bytes JSON/base64-encoded inline in the same blob column as every other field value. That doesn't scale to PowerLine's eventual asset sizes (textures, audio, animation data), and per spec DataLine doesn't interpret blob contents anyway, it just needs to move them reliably.

Decided 2026-08-12: blobs stay **inside the same SQLite file** — a self-contained store was the priority, since external loose-file references can go missing or drift out of sync with the database, which is exactly the unreliability a user-facing app can't have. But the bytes don't sit inline on every record row either; they live in a dedicated `blobs` table within that same file, keyed by content hash:

```sql
CREATE TABLE blobs (hash TEXT PRIMARY KEY, bytes BLOB NOT NULL, created_at TEXT);
```

A record's `Value::Blob` field just stores the hash (a `BlobRef`), not the bytes — keeping the per-database scalar tables (§3) narrow and fast to scan/sort/index, which is the entire reason those tables exist as real columns instead of JSON blobs. `Store::write_blob(bytes) -> BlobRef` and `Store::read_blob(&BlobRef) -> Vec<u8>` write to and read from that table. `BlobRef` has no public constructor outside `write_blob` (Rust-level, not just a convention), so a record can never end up holding a handle to bytes that were never actually written — `set_value` on a Blob field can only ever be given a `BlobRef` that already came from a successful `write_blob` call. Because everything lives in one file, there's also nothing to go dangling if a file gets moved or deleted outside the app, the way an external-loose-files design would risk. Content-addressing gives free deduplication when the same asset is referenced by multiple records (`write_blob` is `INSERT OR IGNORE` on the hash), at no extra design cost.

Trade-off worth knowing: this means the single `.sqlite` file grows with every asset in the project (a big animation/texture library could put it in the gigabytes), and SQLite doesn't reclaim space from deleted blobs until a `VACUUM`. That's the accepted cost of reliability over the loose-files alternative; if it becomes a real problem at PowerLine's asset scale later, it's a `Store`-internal change (e.g. an optional external blob mode), not a change to the public API shape.

## 6. API Surface — Four Capability Groups

The spec's own "API Surface" section already names four groups; the FFI boundary in `dataline-ffi` is organized around exactly these, deliberately, so that each one maps to a single named capability if/when Line's plugin contract wraps DataLine later (see §8):

| Group | Representative operations |
|---|---|
| **Schema management** | `create_database`, `duplicate_database_schema`, `add_field` (auto-pairs if `Reference`), `rename_field`, `retype_field`, `remove_field`, `get_database`, `list_databases` |
| **Record operations** | `create_record`, `get_record`, `set_value`, `delete_record`, `list_records(database_id)` |
| **Reference resolution** | `create_reference`, `delete_reference(id, field)`, `list_references(link, field) -> Vec<Reference>` |
| **Querying** | `find_by_value(database_id, field_id, value) -> Vec<Record>`, `search_text(database_id, query) -> Vec<Record>`, `related_records(link) -> Vec<Link>` |

`Store` is the single UniFFI object exposing all four groups as methods — matching the spec's "opened once, handed around as a stateful core object" — rather than four separate objects, since they all share one open SQLite connection and one transaction boundary. Errors are a typed `DataLineError` enum (UniFFI supports typed throws), not raw status codes, while still respecting the "no raw Rust types cross the boundary, only opaque handles and primitives" rule — `DataLineError`'s variants are plain data (strings, ids), not Rust-internal types.

Reference resolution's original `list_outbound`/`list_inbound` naming (this doc's first draft) assumed a record's outbound and inbound relationships through a given field could differ — true in BaseLine's current hand-wired model, where a reference isn't guaranteed to have a counterpart. It stopped being true once §4 committed to *always* auto-pairing a Reference field with its back-reference field: for any given `(link, field)`, there's exactly one well-defined set of related records, not two. Implemented as a single `list_references(link, field) -> Vec<Reference>`, returning full `Reference` values (not just the linked records) so a caller can act on a specific result with `delete_reference` without a second lookup. `related_records(link)` (step 6, no `field` argument, flattened to `Vec<Link>` across every reference field) remains a distinct, coarser operation for display/search use, not a duplicate of this one.

`Reference` itself — `{ id, source_link, source_field, target_link, target_field }` — mirrors BaseLine's own `Reference` model shape (§4's stated precedent) and isn't spelled out in §2's data model snippet above; it's an implementation detail of the reference-resolution API, not part of the core Type/Record/Value model DataLine defines.

`find_by_value`'s original `predicate` naming implied something more general than what's built: a raw closure can't cross the eventual FFI boundary (§8), and nothing today needs more than exact equality, so `find_by_value(database_id, field_id, value: Value)` matches on equality only, rejecting `Reference`/`Blob` fields the same way `set_value` does. All three Querying operations filter over `list_records`'/`list_references`' full results in Rust rather than pushing the comparison into a SQL `WHERE` clause — correct and simple, at the cost of a full-table scan per call. That's an intentional trade-off for now, matching this section's own "narrow now, widen later without breaking the boundary shape" stance; worth revisiting with real indexing if it becomes a bottleneck at the record volumes the spec targets, not before.

Querying starts intentionally narrow (fixed operations above) rather than a general query language, since nothing in the spec or in BaseLine's current `SearchService`/`EntryService` needs more than field-equality, text search, and reference traversal yet. Widening this to a real filter/query builder is easy to do later without breaking the boundary shape; building it now would be speculative.

## 7. Migrations — Implemented (2026-08-13)

A `Database`'s fields can change after records exist (spec, §Databases). `Store::retype_field(database_id, field_id, new_kind) -> RetypeReport` (`dataline-core/src/store.rs` — no separate `migration.rs`; small enough to live alongside the rest of `Store`'s persistence logic) migrates every existing record's value for that field in the same transaction as the schema change, per three buckets:

- **Lossless** (e.g. `Text` → `Text`, or `Selection` → `Selection` where every currently-selected option still exists in the new options list): value passes through unchanged.
- **Lossy but well-defined** (e.g. `Number` → `Text`, `Boolean` → `Number`): applied, value converted.
- **Not representable** (e.g. `Text` → `Number` where the text isn't numeric, or a `Selection` value whose option was just removed): the value is cleared to `Empty`, and the record's `Link` is added to the returned `RetypeReport.cleared_links` — never silent, per spec. A value that was already `Empty` before the retype is never reported; only real data loss is.

`SchemaRegistry::retype_field` handles the pure schema-side rename (id/name/ordering untouched, only `kind` changes) and — deliberately — rejects `Reference` on either side of the change with `DataLineError::ReferenceFieldRetypeNotSupported`. Retyping into or out of a reference field would mean creating or dropping a join table and migrating a fundamentally different *kind* of data (links, not scalar values), not just re-coercing a value in place — real scope of its own, not folded into this.

Coercion itself (`coerce_value` in `store.rs`) is a pure function, table-tested against every meaningful `FieldKind` pair — including the `Selection` narrowing case above, which is how removing a selection option is handled: it's the same operation as retyping a field to itself with a shorter `options` list.

BaseLine's current `ColumnType.convert`/`isDestructiveConversion` implements this same coercion logic today, but as Swift code sitting next to SwiftUI view code. As decided: the *coercion rules* live in `dataline-core` (shared, testable, reused by every consumer), while BaseLine keeps only the *UI* around it (the destructive-conversion confirmation dialog, the dropdown).

### Correction: SQLite column affinity is not cosmetic (2026-08-13)

§3 originally assumed a field's physical SQL column type never needed to change on retype, reasoning that `encode_scalar_value`/`decode_scalar_value` fully controlled the stored representation regardless of the column's declared affinity. That's wrong: SQLite actively *coerces* a value toward a column's declared affinity on write — a `TEXT` value inserted into a `REAL`-affinity column that looks numeric comes back out as a number, silently undoing the intended encoding. This surfaced immediately as a test failure the first time `retype_field` tried to write a `Text` value into a column that still had `REAL` affinity from its `Number` days.

Since SQLite has no `ALTER COLUMN TYPE`, `retype_field` works around it the standard way: add a new column with the new affinity, write every migrated value into it, drop the old column, then rename the new one back to the stable, id-keyed name (`field_<uuid>`) every other query already expects — so nothing outside `retype_field` itself needs to know this happened.

## 8. Designed for Line, Not Built as a Plugin Yet

Per spec, DataLine is not a Line plugin today (Line doesn't exist yet) but must already behave like one conceptually so that wrapping it later is "a thin adapter, not a redesign." Concretely, that constrains today's design in three ways, all already reflected above:

1. **Ownership discipline is already enforced** — the *only* opaque UniFFI object is `Store` itself; every id (`DatabaseId`, `FieldId`, `Link`, `ReferenceId`) and every blob hash crosses the boundary as a plain `String`, not a Rust struct. That's still exactly the rule Line will eventually want ("no raw language-native types cross the boundary, only opaque handles and primitives") — a `String` *is* a primitive. Ids were deliberately not turned into their own opaque handle types: Swift already keys everything off `UUID`-shaped strings (BaseLine's existing SwiftData models included), and `dataline-core`'s own richer types (`Uuid`, `chrono::NaiveDate`/`DateTime`) aren't UniFFI-native, so mirroring them 1:1 as more opaque objects would have meant either teaching UniFFI custom-type converters for both or building N tiny wrapper objects for no real benefit over a string. See `dataline-ffi/src/types.rs`'s module doc for the full reasoning.
2. **The API is already capability-shaped**, not a loose pile of functions — the four groups in §6 are named the same way the spec's own "API Surface" section names them, which is deliberate: those four names are the natural candidates for Line's future `capability declaration by name` (e.g. `dataline.schema`, `dataline.records`, `dataline.references`, `dataline.query`).
3. **Identity and versioning already exist for free** — `dataline-ffi` exposes a standalone `dataline_version() -> String` (no `Store` needed, mapped straight from the crate's semver via `env!("CARGO_PKG_VERSION")`) and `_dataline_meta.schema_version` for the on-disk store format. Line's future "plugin reports its name and version, Line checks compatibility before trusting anything else" step has nothing new to build on DataLine's side when that day comes.

What's explicitly *not* built now: a plugin manifest, a bootstrap green-light handshake, or any registration into a type-erased registry — none of that has a home to go into yet since Line doesn't exist. Building it speculatively would be exactly the "guessing" the spec's Implementation Order Strategy is trying to avoid.

### Implementation notes (2026-08-12)

- **UniFFI 0.32, proc-macro only** (`#[uniffi::export]`/`#[derive(uniffi::Object/Record/Enum/Error)]`, `uniffi::setup_scaffolding!()`) — no `.udl` file. `Store` wraps `dataline_core::Store` in a `std::sync::Mutex`: a UniFFI `Object` is always shared behind `Arc<Self>` (Swift can call in from anywhere, concurrently), so interior mutability is what makes `&self` methods here able to call `dataline-core`'s `&mut self` ones. Calls serialize against each other — an acceptable constraint, since a single SQLite connection already implies it.
- **A real gap this surfaced in `dataline-core` itself**: `BlobRef` originally had no public constructor at all (by design — only `write_blob` could produce one). That guarantee can't survive crossing an FFI boundary: Swift can only ever hand back a hash *string*, so `dataline-ffi` needed a way to turn that back into a `BlobRef` to pass into `set_value`. Fixed by adding a public `BlobRef::from_hash`, and — since that alone would reopen the door to a fabricated/dangling reference — moving the actual safety check into `Store::set_value` itself, which now verifies the hash exists in the `blobs` table before accepting it. The guarantee still holds; it just lives at the point of use instead of at construction. See `dataline-core/src/record.rs`'s `BlobRef` doc comment.
- **Packaging**: `xcframework/DataLineFFI.xcframework` (universal `arm64`+`x86_64` macOS static lib, built via `lipo` from `cargo build --release --target {aarch64,x86_64}-apple-darwin`, `MACOSX_DEPLOYMENT_TARGET=14.0` set explicitly to avoid a deployment-target mismatch against consumers) plus the generated `dataline_ffi.swift`, wrapped in a local Swift package at `xcframework/Package.swift` (`DataLineFFIRust` binary target + `DataLineFFI` Swift target) that BaseLine can add directly as a local package dependency. iOS targets aren't built — nothing in the ecosystem needs them yet (BaseLine is Mac-only); trivial to add to the same `lipo`/`xcodebuild` pipeline whenever a Line-native iOS app does.
- **Verified end-to-end, not just compiled**: `xcframework/Sources/DataLineSmokeTest` is a real Swift executable target (`swift run DataLineSmokeTest`) that drives the actual generated API — schema creation, auto-paired references (cross-database and self-referencing), scalar values, blobs, querying — through the real compiled library, not a mock. This is the strongest verification available without wiring DataLine into BaseLine's own Xcode project, which remains step 9's job.

## 9. Testing Strategy

- `dataline-core` unit tests: schema mutation, migration coercion rules (including the "not representable" case), reference bidirectionality (create/delete/cascade-on-record-delete), multi-value reference correctness.
- `dataline-core` integration tests: a full `Store` opened against a temp SQLite file, exercising realistic sequences (BaseLine's own `Card`/`Balance change` example from `LineBase`'s spec is a good fixture — two databases, a two-sided reference, a field retype after records exist).
- `dataline-ffi` boundary tests: a couple of Rust-side unit tests for the parts a Swift-level test can't reach (malformed id strings, an unwritable store path) — the bulk of real boundary verification is `xcframework/Sources/DataLineSmokeTest`, a real Swift executable target exercising the actual generated API end-to-end. Turned out to matter in practice: it caught a stale generated C header (rebuilt the Rust library and the `.swift` file, forgot to refresh the paired `.h`) that a Rust-only test harness would never have seen, since that failure only exists at the Swift/C linking layer.
- `dataline-cli`: not a test suite, but a fast manual-iteration loop — a small binary that opens a store and drives `dataline-core` directly, useful while building without waiting on Swift bindings or Xcode at all.

## 10. Build Order

1. Workspace scaffold, `dataline-core` skeleton, error types.
2. Schema module (`Database`, `FieldDefinition`, `FieldKind`) — in-memory only, unit tested.
3. SQLite persistence — `Store::open`, schema tables, per-database record tables, scalar migrations.
4. Record CRUD against real per-database tables.
5. Reference fields — join tables, paired-field auto-creation, bidirectional consistency.
6. Querying — filter/search/traversal operations.
7. Blob storage — content-addressed `blobs` table inside the store file.
8. `dataline-ffi` — UniFFI boundary wrapping the four capability groups; generate Swift bindings; build the XCFramework.
9. *(Separate effort, BaseLine's own, not DataLine's)* — migrate BaseLine off its SwiftData models onto generated DataLine bindings.

Steps 1–7 are fully verifiable via `dataline-core`'s own test suite with no Swift or Xcode involved at all, which keeps early iteration fast.

## Decisions Confirmed (2026-08-12)

1. **Type fused into Database**, reuse via `duplicate_database_schema` rather than a live-shared Type — see §2.
2. **Back-reference fields auto-created** when a Reference field is added — see §4.
3. **Blobs stored inline in the store file**, via a dedicated content-addressed `blobs` table rather than external loose files — see §5.
4. **`page` dropped from `FieldKind`** — BaseLine represents it off the record's own `Link`, not a DataLine schema field — see §2.
5. **Type-coercion rules live in `dataline-core`**, BaseLine keeps only the UI around them — see §7.

No open questions remain blocking implementation start.
