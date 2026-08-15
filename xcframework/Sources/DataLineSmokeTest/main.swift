import DataLineFFI
import Foundation

// End-to-end smoke test for the real Swift API generated from dataline-ffi —
// not just a compile check, but actually exercising schema, records,
// references, and blobs through Store the way BaseLine eventually will.

func check(_ condition: Bool, _ message: String) {
    guard condition else {
        print("FAIL: \(message)")
        exit(1)
    }
}

check(!datalineVersion().isEmpty, "datalineVersion() should report a non-empty version string")

let tempDir = FileManager.default.temporaryDirectory
let storePath = tempDir.appendingPathComponent("dataline-smoketest-\(UUID().uuidString).sqlite").path

let store = try Store.open(path: storePath)

// Schema: Card / Balance change, mirroring LineBase's own spec example.
let card = try store.createDatabase(name: "Card")
let balance = try store.createDatabase(name: "Balance change")

let nameField = try store.addField(databaseId: card, name: "Name", kind: .text)
let costField = try store.addField(databaseId: card, name: "Cost", kind: .number)
let artworkField = try store.addField(databaseId: card, name: "Artwork", kind: .blob)
let balanceField = try store.addField(
    databaseId: card, name: "Balance change",
    kind: .reference(targetDatabase: balance, pairedField: nil)
)
let relatedField = try store.addField(
    databaseId: card, name: "Related Cards",
    kind: .reference(targetDatabase: card, pairedField: nil)
)

let cardDb = try store.getDatabase(databaseId: card)
check(cardDb.fields.count == 6, "Card should have 5 own fields + 1 back-reference field for self-reference, got \(cardDb.fields.count)")

// Records + scalar values.
let fireball = try store.createRecord(databaseId: card)
try store.setValue(link: fireball, fieldId: nameField, value: .text("Fireball"))
try store.setValue(link: fireball, fieldId: costField, value: .number(3))

let frostbolt = try store.createRecord(databaseId: card)
try store.setValue(link: frostbolt, fieldId: nameField, value: .text("Frostbolt"))

let record = try store.getRecord(link: fireball)
check(record.values[nameField] == .text("Fireball"), "Name should round-trip through set_value/get_record")
check(record.values[costField] == .number(3), "Cost should round-trip through set_value/get_record")

// Blobs.
let artworkBytes = Data("pretend PNG bytes".utf8)
let blobHash = try store.writeBlob(bytes: artworkBytes)
try store.setValue(link: fireball, fieldId: artworkField, value: .blob(blobHash))
let readBack = try store.readBlob(hash: blobHash)
check(readBack == artworkBytes, "Blob bytes should round-trip through write_blob/read_blob")

// References: cross-database and self-referencing.
let balanceEntry = try store.createRecord(databaseId: balance)
let reference = try store.createReference(sourceLink: fireball, sourceField: balanceField, targetLink: balanceEntry)
check(reference.targetLink == balanceEntry, "create_reference should return the target link")

_ = try store.createReference(sourceLink: fireball, sourceField: relatedField, targetLink: frostbolt)

let related = try store.relatedRecords(link: fireball)
check(Set(related) == Set([balanceEntry, frostbolt]), "related_records should flatten across both reference fields")

// Querying.
let matches = try store.findByValue(databaseId: card, fieldId: nameField, value: .text("Fireball"))
check(matches.count == 1 && matches[0].link == fireball, "find_by_value should find the exact match")

let searchResults = try store.searchText(databaseId: card, query: "fire")
check(searchResults.count == 1, "search_text should be case-insensitive and match only Fireball")

// Retyping: Cost (Number) -> Text should losslessly preserve "3"; Name
// (Text) -> Number should clear Frostbolt's non-numeric value and report it.
let retypeReport1 = try store.retypeField(databaseId: card, fieldId: costField, kind: .text)
check(retypeReport1.clearedLinks.isEmpty, "Number -> Text should be lossless")
check(try store.getRecord(link: fireball).values[costField] == .text("3"), "Cost should now read back as text")

let retypeReport2 = try store.retypeField(databaseId: card, fieldId: nameField, kind: .number)
check(
    Set(retypeReport2.clearedLinks) == Set([fireball, frostbolt]),
    "neither 'Fireball' nor 'Frostbolt' is numeric, so both should be reported cleared"
)
check(try store.getRecord(link: frostbolt).values[nameField] == .empty, "cleared value should read back empty")

print("PASS: all \(8) smoke-test checks passed against the real Swift bindings.")
