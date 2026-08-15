#DataLine

DataLine is a standalone, embeddable database engine written in Rust. It provides typed, schema-driven data storage with automatic bidirectional references, designed to be reused across multiple, otherwise unrelated applications (LineBase, StoryLine, and eventually PowerLine) without any of them sharing code, process space, or assumptions about one another.

DataLine has no user interface, no host assumptions, and no dependency on any other project in the ecosystem. It is a capability, not an app.

#Design Decisions

##Purpose and Scope

DataLine exists to solve one problem well: typed, structured, relational data storage that is reliable, scales to large volumes of records, and can be embedded identically across different applications and platforms. It is the shared foundation beneath LineBase's wiki-style database and is intended to also serve StoryLine and PowerLine's editor/runtime data needs in the future, without those three products needing to reimplement the same reference-graph and schema logic independently.

DataLine is deliberately narrow in scope. It does not know about:
- Pages, layouts, or any presentation concept (that belongs to consumers such as LineBase)
- Timelines, sequences, or narrative structure (that belongs to StoryLine)
- Rendering, scenes, or runtime engine concepts (that belongs to PowerLine)

DataLine only knows about types, fields, records, and references. Anything above that layer is a consumer's responsibility.

##Architectural Position

DataLine is not a Line plugin in the dynamic-loading sense, since Line itself does not exist yet and will be scoped later, during PowerLine's development. DataLine is written so that it already behaves like one conceptually: it is self-contained, declares a clear capability surface, and makes no assumptions about a host. When Line's plugin contract is eventually designed, wrapping DataLine as a native Line plugin should be a thin adapter over its existing API, not a redesign.

In the meantime, DataLine is consumed directly as a linked library:
- On Apple platforms, compiled to an XCFramework with Swift bindings generated via UniFFI
- On any future non-Apple platform or from PowerLine's own Rust codebase, consumed directly as a Rust crate, with no FFI layer needed at all

DataLine is versioned and released independently of any consumer. Consumers pin a version; upgrading one consumer's DataLine version never requires upgrading another's.

##Core Data Model

###Types

A Type is a named collection of typed Fields. Types are user-defined at runtime by whichever consumer is authoring the schema (e.g. a user defining a "Card" type inside LineBase). A Type has no inherent presentation; it is purely a schema definition.

Supported field kinds include, at minimum: text, number, boolean, selection (single and multi), date, link, reference, and blob/asset (for opaque binary data such as images, resolved by the consumer, not interpreted by DataLine).

###Link

Every record automatically receives a Link: a stable, unique identifier generated on creation. A Link is the addressable identity of a record and is independent of any of its field values, meaning records can be renamed, retyped in part, or heavily edited without ever losing their identity or breaking anything that refers to them.

###Reference Fields

A Reference field holds one or more Links, pointing to records in a (possibly different) Database. References are two-sided: creating a reference from Database A to Database B automatically registers the corresponding back-reference in Database B, and this relationship is kept consistent on every write, including deletion. A single reference field may hold multiple values (a record can reference many others through the same field).

This bidirectional consistency is core, load-bearing logic in DataLine and is the primary reason a shared engine exists rather than three independent implementations: getting this right once benefits every consumer.

###Databases

A Database is a typed collection of records: every record in a Database conforms to the same Type. Multiple Databases may exist per project/store, and a Database's Type may be modified after records already exist (adding, removing, or retyping fields), which DataLine must handle via an internal migration step rather than requiring the consumer to manage it.

##Persistence

DataLine persists data to SQLite via `rusqlite`. Scalar fields are stored as native, indexed SQLite columns for performance and integrity. Reference fields are stored via dedicated join tables, one per reference relationship, to correctly support multi-value and bidirectional semantics without contaminating the scalar schema.

Schema changes driven by a Type's field additions, removals, or type changes are handled internally as migrations. DataLine is responsible for keeping existing records valid (or clearly flagging what cannot be preserved) when a field's type changes after records already exist; this must never silently corrupt or drop data without the consumer being informed.

DataLine is designed for large record volumes per Database (thousands of records and beyond) without degrading interactive performance, since this is explicitly the gap it is meant to fill relative to tools like Notion.

##API Surface

DataLine's public interface is organized around a small number of capability-shaped operations, intended to read as a cohesive unit rather than a loose collection of functions:

- **Schema management** — define, inspect, and modify Types and their Fields
- **Record operations** — create, read, update, delete records within a Database
- **Reference resolution** — read and write reference relationships, with bidirectional consistency handled internally
- **Querying** — retrieve records by Database, by Type, by field value, and by reference traversal (e.g. "all records referencing this Link")

A DataLine instance is opened once against a store location and handed around as a stateful core object, rather than being a pile of free functions relying on implicit global state. This mirrors how a resource behaves once registered in a Line-style bootstrap, even though no such bootstrap exists yet.

##FFI and Distribution

DataLine is always consumed as a precompiled, dynamically loaded artifact, never as linked-in source, on every platform including from other Rust code. This is a deliberate architectural invariant, not an incidental packaging detail: a capability such as DataLine should never be compiled directly into a consuming app, since that is the same principle the wider Line ecosystem is built around (an app's capabilities are composed from separately built plugins, not baked into it at compile time). The Swift process-ownership constraint (covered under Architectural Position) remains the one unavoidable exception to how a host itself is launched; it does not change how DataLine is loaded once the host exists.

Rust remains the single source of truth for all logic. DataLine's public API is defined once and exposed through a stable, versioned C-ABI boundary — opaque handles, no raw Rust types crossing the boundary directly — so that no consumer, regardless of language, depends on Rust's (unstable) in-memory layout.

- **Swift consumers** (LineBase, and any future Swift-based app): DataLine is compiled in library mode and distributed as a versioned XCFramework, with Swift bindings generated from the same interface definition via UniFFI. The app links this precompiled artifact; it is never rebuilt as part of the consumer's own build.
- **Rust consumers** (PowerLine today, Line once it exists): DataLine is distributed as a precompiled dynamic library (`.dylib`/`.so`/`.dll`) and loaded at runtime (e.g. via `libloading`) through the same C-ABI boundary, rather than added as a source-level crate dependency. This is intentionally more ceremony than a plain `Cargo.toml` dependency would require, in exchange for consistency with the rest of the ecosystem's plugin model.

This C-ABI boundary is deliberately built as a real, working precedent for Line's eventual native plugin contract. It is proven against DataLine and a real consumer (PowerLine) before Line's plugin shell is formally designed, so that Line's contract can generalize from something that already works rather than being speculated in the abstract.

DataLine still does not require, and does not attempt to provide, ABI stability for entirely unknown, independently-authored third-party consumers the way a fully open plugin ecosystem eventually will. Every consumer today links a specific released version of the compiled artifact; the interface is versioned and checked at load time, but DataLine and its known consumers are still developed by the same author on a coordinated release cadence, not by unrelated third parties.

##Non-Goals

- DataLine does not render, format, or lay out data in any way. That is entirely a consumer concern.
- DataLine does not implement publishing, export, or file-format conversion beyond its own store.
- DataLine does not implement networked sync or multi-user concurrent access in its initial scope. Local, single-writer embedded use is the baseline target; sharing a store across multiple apps on the same device (e.g. via an App Group container on Apple platforms) is a distribution/deployment concern for consumers, not something DataLine manages itself.
- DataLine does not assume or depend on Line, PowerLine, LineBase, or StoryLine. It must remain buildable, testable, and useful in complete isolation.