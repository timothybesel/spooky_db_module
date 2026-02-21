# context.md -- spooky_db_module Integration Reference

This file is the canonical reference for integrating `spooky_db_module` into the SSP (Spooky Stream Processor) project. It is written for engineers and AI agents performing the integration work. All API names, type signatures, and file paths are drawn directly from the current source code.

---

## Purpose of This Module

`spooky_db_module` is a high-performance persistent storage layer backed by [redb](https://crates.io/crates/redb) (an embedded B-tree key-value store). It replaces SSP's in-memory `Database` struct with on-disk record storage while keeping ZSets (membership weights) entirely in memory for zero-I/O view evaluation. Records are stored in a custom binary format (sorted xxh64 hash index + native little-endian field data) that enables O(log N) field access with zero allocation for scalar reads. The module exposes a `DbBackend` trait that allows SSP to migrate incrementally from its current in-memory `Database` to the persistent `SpookyDb` without a flag-day rewrite.

---

## Module Architecture Summary

### On-disk storage: two redb tables

Both tables use flat `"table_name:record_id"` string keys. Table names must not contain `':'`; record IDs may. The first `':'` is the separator (`split_once(':')` everywhere).

```rust
// src/db/db.rs:21-25
const RECORDS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("records");
const VERSION_TABLE: TableDefinition<&str, u64>   = TableDefinition::new("versions");
```

- **RECORDS_TABLE**: Key `"table:id"` maps to serialized SpookyRecord bytes (`Vec<u8>`).
- **VERSION_TABLE**: Key `"table:id"` maps to `u64` version (read from the `"spooky_rv"` field or supplied explicitly).

### In-memory ZSets

```rust
// src/db/db.rs:38-46
pub struct SpookyDb {
    db: RedbDatabase,
    zsets: FastMap<SmolStr, ZSet>,  // per-table: record_id -> weight (1 = present)
}
```

ZSets are rebuilt from a sequential `RECORDS_TABLE` scan on startup (`rebuild_zsets_from_records`, `src/db/db.rs:77-91`). Every key found gets weight 1. This is O(N) over all records, approximately 20-100ms per million records on SSD.

All ZSet reads are pure memory, zero I/O. This is critical for view evaluation where the Scan operator borrows `&ZSet`.

### Binary record format

```
+-- Header (20 bytes) -----------------------------------------+
|  field_count: u32 (LE)  |  _reserved: [u8; 16]               |
+-- Index (20 bytes x N) --- SORTED by name_hash ascending ----+
|  name_hash:   u64 (LE)    (xxh64 of field name, seed=0)      |
|  data_offset: u32 (LE)                                       |
|  data_length: u32 (LE)                                       |
|  type_tag:    u8           (TAG_* constants)                  |
|  _padding:    [u8; 3]                                        |
+-- Data (variable) -------------------------------------------+
|  Scalars: native LE bytes (i64/u64/f64 = 8 bytes,            |
|           bool = 1 byte, null = 0 bytes)                      |
|  Strings: raw UTF-8                                           |
|  Nested:  CBOR-encoded (arrays + objects)                     |
+---------------------------------------------------------------+
```

Constants from `src/types.rs`: `HEADER_SIZE = 20`, `INDEX_ENTRY_SIZE = 20`.

Type tags (`src/types.rs:4-10`): `TAG_NULL=0`, `TAG_BOOL=1`, `TAG_I64=2`, `TAG_F64=3`, `TAG_STR=4`, `TAG_NESTED_CBOR=5`, `TAG_U64=6`.

### Key invariants

1. The index MUST be sorted by `name_hash` ascending. All serialization paths enforce this. Violation silently corrupts field lookups (binary search assumes sorted order).
2. Hard limit of 32 top-level fields per record (`ArrayVec<..., 32>` in `src/serialization.rs`). Exceeding this returns `RecordError::TooManyFields`.
3. Field names are NOT stored -- only xxh64 hashes. Field names cannot be recovered from a stored record without out-of-band metadata. This is a permanent format constraint.

---

## Integration Target: SSP

SSP (Spooky Stream Processor) is a Rust library implementing incremental materialized view maintenance using DBSP semantics. It currently stores all data in-memory in a `Database` struct containing `FastMap<String, Table>`, where each `Table` holds a `ZSet` and a `FastMap<RowKey, SpookyValue>` of row data.

SSP needs this module for three reasons:

1. **Persistence**: Circuit state (record data) is lost on restart. `SpookyDb` stores records on disk via redb while keeping ZSets in memory for zero-I/O view evaluation.

2. **Row key inconsistency fix**: SSP's `Table.rows` stores records with bare `"id"` OR prefixed `"table:id"` keys, requiring a two-attempt lookup with a `format!()` allocation on miss. The module enforces consistent `"table:id"` keys everywhere.

3. **Type alignment**: SSP's `Database.tables` uses `String` keys and `BatchDeltas.membership` uses `String` keys, creating type inconsistency with `ZSet` (which uses `SmolStr`). The module uses `SmolStr` throughout.

---

## Type Mapping Table

| SSP Type / Field | Module Equivalent | Notes |
|---|---|---|
| `Database` (struct) | `SpookyDb` (`src/db/db.rs:38`) | SpookyDb replaces Database entirely |
| `Database.tables: FastMap<String, Table>` | `SpookyDb.zsets: FastMap<SmolStr, ZSet>` | Module manages table registry internally |
| `Table.rows: FastMap<RowKey, SpookyValue>` | `RECORDS_TABLE` in redb | In-memory rows become on-disk bytes |
| `Table.zset: ZSet` | `SpookyDb.zsets[table_name]: ZSet` | Same `FastMap<SmolStr, i64>` type |
| `Table.name: TableName` | First segment of redb key `"table:id"` | No separate Table struct |
| `ssp::Operation` | `spooky_db_module::db::Operation` (`src/db/types.rs:89`) | Same semantics: Create=+1, Update=0, Delete=-1 |
| `BatchEntry { table, op, id, data: SpookyValue }` | `DbMutation { table, id, op, data: Option<Vec<u8>>, version }` (`src/db/types.rs:79`) | Data must be pre-serialized |
| `BatchDeltas { membership: FastMap<String, ZSet>, content_updates }` | `BatchMutationResult { membership_deltas: FastMap<SmolStr, ZSet>, content_updates, changed_tables }` (`src/db/types.rs:112`) | SmolStr keys, added `changed_tables` |
| `Delta { table, key, weight, content_changed }` | Return of `apply_mutation`: `(SmolStr, i64)` = (zset_key, weight_delta) | Content change tracked in `BatchMutationResult.content_updates` |
| `Circuit.db: Database` | `Circuit.db: SpookyDb` (or `Box<dyn DbBackend>`) | Direct replacement or trait object |
| `ssp::SpookyValue` | `spooky_db_module::spooky_value::SpookyValue` | Different types; see bridge section below |
| `ssp::FastMap<K, V>` (FxHashMap) | `spooky_db_module::db::types::FastMap<K, V>` (FxHashMap) | Same underlying type |
| `ssp::ZSet` | `spooky_db_module::db::types::ZSet` | Same: `FastMap<SmolStr, i64>` |
| `ssp::RowKey` (SmolStr) | `spooky_db_module::db::types::RowKey` (SmolStr) | Same type |
| `ssp::TableName` (SmolStr) | `SmolStr` (used directly, no alias in module) | Compatible |

---

## The SpookyValue Bridge

### The mismatch

SSP and the module both define a type called `SpookyValue`, but they are **different Rust types**:

```rust
// SSP's SpookyValue (ssp/src/engine/types/spooky_value.rs)
pub enum SpookyValue {
    Null, Bool(bool), Number(f64), Str(SmolStr),
    Array(Vec<SpookyValue>),
    Object(FastMap<SmolStr, SpookyValue>),  // FxHashMap
}

// Module's SpookyValue (spooky_db_module/src/spooky_value.rs)
pub enum SpookyValue {
    Null, Bool(bool), Number(SpookyNumber), Str(SmolStr),
    Array(Vec<SpookyValue>),
    Object(BTreeMap<SmolStr, SpookyValue>),  // BTreeMap, not FxHashMap
}
```

Key differences:
- `Number`: SSP uses bare `f64`; module uses `SpookyNumber` (canonical total ordering, NaN handling via `total_cmp`).
- `Object`: SSP uses `FastMap` (FxHashMap); module uses `BTreeMap`.

### The conversion bridge: serde_json::Value

Both types implement `From<serde_json::Value>` and `Into<serde_json::Value>`. The bridge is:

```
Write path:  SSP SpookyValue  ->  serde_json::Value  ->  module binary bytes
Read path:   module binary bytes  ->  serde_json::Value  ->  SSP SpookyValue
```

Concrete code for the write path:
```rust
use spooky_db_module::serialization;

fn ssp_value_to_record_bytes(val: &ssp::SpookyValue) -> Result<Vec<u8>, RecordError> {
    let json_val: serde_json::Value = val.into();  // SSP's Into<serde_json::Value>
    let map = match json_val {
        serde_json::Value::Object(m) => m,
        _ => return Err(RecordError::SerializationNotObject),
    };
    // serde_json::Value implements RecordSerialize
    let btree: BTreeMap<SmolStr, serde_json::Value> = map.into_iter()
        .map(|(k, v)| (SmolStr::new(&k), v))
        .collect();
    let (bytes, _field_count) = serialization::serialize(&btree)?;
    Ok(bytes)
}
```

Concrete code for the read path (full record reconstruction):
```rust
use spooky_db_module::serialization::from_bytes;
use spooky_db_module::spooky_record::{SpookyRecord, SpookyReadable};

fn record_bytes_to_ssp_value(
    raw: &[u8],
    field_names: &[&str],
) -> ssp::SpookyValue {
    let (buf, count) = from_bytes(raw).expect("valid record");
    let record = SpookyRecord::new(buf, count);
    let mut map = ssp::FastMap::default();
    for &name in field_names {
        if let Some(val) = record.get_field::<serde_json::Value>(name) {
            let ssp_val: ssp::SpookyValue = val.into();
            map.insert(SmolStr::new(name), ssp_val);
        }
    }
    ssp::SpookyValue::Object(map)
}
```

### When to use direct SpookyRecord accessors vs full conversion

**Use direct accessors** (`get_i64`, `get_str`, `get_f64`, `get_bool`) on hot paths:
- Filter predicates in `view.rs` that check a single field
- Join probe lookups that compare one field
- Any path where you know the field name and type at compile time

```rust
let (buf, count) = from_bytes(&raw)?;
let record = SpookyRecord::new(buf, count);
if record.get_i64("age").unwrap_or(0) > 21 { /* ... */ }
```

These accessors are zero-allocation and zero-copy (for strings).

**Use full conversion** via `serde_json::Value` only when:
- You need the entire record as a `SpookyValue` (e.g., returning to the user)
- You are passing the record to SSP code that expects `SpookyValue`
- The field set is dynamic / unknown at compile time

**Use FieldSlot for repeated access** to the same field across many records in a loop:
```rust
// resolve() is O(log N) once per field name
let slot = record.resolve("age");
// get_i64_at is O(1) thereafter
let age = record.get_i64_at(&slot);
```

Note: FieldSlot is tied to a specific record buffer and generation. For immutable `SpookyRecord` (generation always 0), slots from one record can be reused on another record with the same schema (same fields in same order). For `SpookyRecordMut`, slots are invalidated by structural mutations (generation increments).

---

## Concrete Changes Required in SSP

The following changes are listed in dependency order. Each step should compile and pass tests before proceeding.

### Step 1: Add the dependency

Add `spooky_db_module` to SSP's `Cargo.toml`. See the "Cargo.toml Changes" section below.

### Step 2: Implement `DbBackend` for SSP's current in-memory `Database`

Create a wrapper that implements `spooky_db_module::db::DbBackend` for the current in-memory `Database`. This allows both implementations to coexist behind the same trait.

File: `ssp/src/engine/db.rs` (new or modify existing)

```rust
impl DbBackend for Database {
    fn get_table_zset(&self, table: &str) -> Option<&ZSet> {
        self.tables.get(table).map(|t| &t.zset)
    }

    fn get_record_bytes(&self, table: &str, id: &str) -> Option<Vec<u8>> {
        // Convert SpookyValue -> serde_json::Value -> serialize to binary
        let row = get_row_value(table, id, self)?;
        let json_val: serde_json::Value = row.into();
        let map = /* convert to BTreeMap<SmolStr, serde_json::Value> */;
        serialization::serialize(&map).ok().map(|(bytes, _)| bytes)
    }

    fn ensure_table(&mut self, table: &str) {
        self.tables.entry(table.to_string()).or_insert_with(|| Table::new(SmolStr::new(table)));
    }

    fn apply_mutation(&mut self, table, op, id, data, version)
        -> Result<(SmolStr, i64), SpookyDbError> {
        // Delegate to existing Table::apply_mutation after deserializing bytes
        // ...
    }

    fn apply_batch(&mut self, mutations) -> Result<BatchMutationResult, SpookyDbError> { /* ... */ }
    fn bulk_load(&mut self, records) -> Result<(), SpookyDbError> { /* ... */ }
    fn get_zset_weight(&self, table, id) -> i64 { /* ... */ }
}
```

### Step 3: Change `Circuit.db` to use the trait

File: `ssp/src/engine/circuit.rs`

Change:
```rust
pub struct Circuit {
    pub db: Database,          // before
    // ...
}
```
To:
```rust
pub struct Circuit {
    pub db: Box<dyn DbBackend>,  // after (or use a generic parameter)
    // ...
}
```

Alternatively, use a generic parameter `Circuit<D: DbBackend>` if dynamic dispatch overhead is unacceptable.

### Step 4: Update `ingest_single` / `ingest_batch`

File: `ssp/src/engine/circuit.rs`

The current flow calls `Table::apply_mutation(op, key, data)` which writes to in-memory `rows` and `zset`. Replace with:

```rust
// Pre-serialize the SpookyValue to binary bytes
let json_val: serde_json::Value = batch_entry.data.into();
let btree = json_to_btreemap(json_val);  // helper: serde_json::Map -> BTreeMap<SmolStr, V>
let (bytes, _) = serialization::serialize(&btree)?;

// Build DbMutation
let mutation = DbMutation {
    table: batch_entry.table.clone(),
    id: batch_entry.id.clone(),
    op: translate_op(batch_entry.op),  // ssp::Operation -> module::Operation
    data: if matches!(batch_entry.op, Operation::Delete) { None } else { Some(bytes) },
    version: None,  // or extract from data if using spooky_rv
};
```

For batch ingestion, collect all `DbMutation`s into a `Vec` and call `db.apply_batch(mutations)` once.

### Step 5: Update view evaluation read paths

File: `ssp/src/engine/view.rs`

Replace `table.rows.get(id)` / `get_row_value(table, id, db)` with:

```rust
// For Scan operator: borrow ZSet
let zset: Option<&ZSet> = db.get_table_zset(table_name);

// For Filter/Join: read record bytes + use SpookyRecord accessors
if let Some(raw) = db.get_record_bytes(table_name, id) {
    let (buf, count) = from_bytes(&raw).expect("valid record");
    let record = SpookyRecord::new(buf, count);
    // Use typed accessors directly for predicates:
    if record.get_str("status") == Some("active") { /* ... */ }
}
```

### Step 6: Update `Circuit::load_from_json` / `Circuit::init_load`

File: `ssp/src/engine/circuit.rs`

Replace the bulk insertion into `Table.rows` with `db.bulk_load(records)`:

```rust
let bulk: Vec<BulkRecord> = records.iter().map(|(table, id, val)| {
    let json_val: serde_json::Value = val.into();
    let btree = json_to_btreemap(json_val);
    let (bytes, _) = serialization::serialize(&btree).expect("serialize");
    BulkRecord { table: table.clone(), id: id.clone(), data: bytes }
}).collect();
db.bulk_load(bulk)?;
```

### Step 7: Remove SSP's `Table.rows` field

Once all read/write paths go through `DbBackend`, remove:
- `Table.rows: FastMap<RowKey, SpookyValue>` from the `Table` struct
- `get_row_value()` helper function
- Any direct `table.rows.get()` / `table.rows.insert()` calls

### Step 8: Switch `Circuit.db` from in-memory to `SpookyDb`

```rust
let db = SpookyDb::new("path/to/circuit.redb")?;
let circuit = Circuit { db: Box::new(db), views, dependency_list };
```

This is the final switch. All prior steps should work with both backends.

---

## Integration Write Path

Step-by-step pseudocode for `ingest_single(entry: BatchEntry)`:

```
1. CONVERT entry.data (ssp::SpookyValue) to serde_json::Value
     let json_val: serde_json::Value = entry.data.into();

2. SERIALIZE to binary record bytes (BEFORE opening any write transaction)
     let btree: BTreeMap<SmolStr, serde_json::Value> = json_to_btreemap(json_val);
     let (bytes, _field_count) = serialization::serialize(&btree)?;
     // Or use serialize_into() with a reusable buffer for bulk paths:
     // let mut buf = Vec::with_capacity(4096);
     // serialization::serialize_into(&btree, &mut buf)?;

3. MAP ssp::Operation to module::Operation
     let mod_op = match entry.op {
         ssp::Operation::Create => spooky_db_module::db::Operation::Create,
         ssp::Operation::Update => spooky_db_module::db::Operation::Update,
         ssp::Operation::Delete => spooky_db_module::db::Operation::Delete,
     };

4. BUILD DbMutation
     let mutation = DbMutation {
         table: entry.table,
         id: entry.id,
         op: mod_op,
         data: if mod_op == Operation::Delete { None } else { Some(bytes) },
         version: None,
     };

5. CALL apply_batch (even for single mutations, to keep the pattern uniform)
     let result: BatchMutationResult = db.apply_batch(vec![mutation])?;
     // result.membership_deltas: per-table ZSet weight changes
     // result.content_updates: per-table set of changed record IDs
     // result.changed_tables: list of tables that were touched

6. BUILD BatchDeltas from result (for view evaluation)
     let batch_deltas = BatchDeltas {
         membership: result.membership_deltas,  // types already match
         content_updates: result.content_updates,
     };

7. EVALUATE impacted views (unchanged from current SSP logic)
     for table_name in &result.changed_tables {
         for &view_idx in &dependency_list[table_name] {
             views[view_idx].process_delta(delta, &*db);
         }
     }
```

For `ingest_batch(entries: Vec<BatchEntry>)`, steps 1-4 run in a loop collecting `Vec<DbMutation>`, then step 5 calls `apply_batch` once with the full vector. This is critical: one redb write transaction for N mutations = one fsync.

---

## Integration Read Path

### Row lookup (Filter / Join operators)

Current SSP code (`get_row_value` in `view.rs`):
```rust
fn get_row_value(table: &str, id: &str, db: &Database) -> Option<SpookyValue> {
    db.tables.get(table)?.rows.get(id)
        .or_else(|| db.tables.get(table)?.rows.get(&format!("{}:{}", table, id)))
        .cloned()
}
```

Replacement using `DbBackend`:
```rust
fn get_row_value(table: &str, id: &str, db: &dyn DbBackend) -> Option<ssp::SpookyValue> {
    let raw: Vec<u8> = db.get_record_bytes(table, id)?;
    // Option A: Full conversion (when you need the whole record as SpookyValue)
    let (buf, count) = from_bytes(&raw).ok()?;
    let record = SpookyRecord::new(buf, count);
    // Must provide field names -- they cannot be recovered from hashes
    let known_fields = get_schema_fields(table);  // caller must supply this
    let mut map = ssp::FastMap::default();
    for name in known_fields {
        if let Some(val) = record.get_field::<serde_json::Value>(name) {
            map.insert(SmolStr::new(name), val.into());
        }
    }
    Some(ssp::SpookyValue::Object(map))
}
```

Note the **field name requirement**: since the binary format stores only hashes, the caller must supply field names. Options:
- Maintain a `FastMap<SmolStr, Vec<String>>` mapping table names to known field names (derived from the schema or first ingested record).
- For Filter/Join operators, the view definition already specifies which fields are needed -- use those directly.

For hot-path Filter predicates, skip the full conversion entirely:
```rust
// In view evaluation, when checking a predicate like "age > 21"
if let Some(raw) = db.get_record_bytes(table, id) {
    let (buf, count) = from_bytes(&raw).unwrap();
    let record = SpookyRecord::new(buf, count);
    let passes = record.get_i64("age").map_or(false, |age| age > 21);
}
```

This is zero-allocation for scalar fields. For string fields, `get_str` returns a borrowed `&str` into the record buffer -- zero-copy, valid for the lifetime of `raw`.

### ZSet borrow (Scan operator)

Current SSP code:
```rust
let zset: &ZSet = &table.zset;  // or Cow::Borrowed(&table.zset)
```

Replacement:
```rust
let zset: &ZSet = db.get_table_zset(table_name)
    .unwrap_or(&EMPTY_ZSET);  // static empty ZSet for missing tables
// Zero-copy borrow, zero I/O. Same lifetime semantics as before.
```

The borrow is valid until the next `&mut self` call on `SpookyDb`. Since view evaluation is read-only, this is safe for the duration of a view tick.

### get_record_typed for compatibility

The module also provides `SpookyDb::get_record_typed(table, id, fields)` which returns a `spooky_db_module::SpookyValue::Object`. This is a convenience method but returns the module's SpookyValue, not SSP's. Convert via `serde_json::Value` if needed:

```rust
let mod_val: spooky_db_module::SpookyValue = db.get_record_typed("users", "alice", &["age", "name"])?.unwrap();
// mod_val -> serde_json::Value -> ssp::SpookyValue
```

---

## Migration Strategy

The `DbBackend` trait (`src/db/db.rs:415-450`) enables incremental migration without breaking SSP:

### Phase 1: Dual implementation behind trait

1. Add `spooky_db_module` dependency to SSP.
2. Implement `DbBackend` for SSP's current in-memory `Database` (wrapper that serializes/deserializes on the fly).
3. Change `Circuit.db` to `Box<dyn DbBackend>` (or generic `D: DbBackend`).
4. Wire all read/write paths through `DbBackend` methods.
5. **Test**: SSP should pass all existing tests using the in-memory `DbBackend` impl.

### Phase 2: Feature-flag the persistent backend

1. Add a `--persist` flag or `Config.storage_path: Option<PathBuf>`.
2. When the flag is set, construct `SpookyDb::new(path)` and pass it as the `DbBackend`.
3. When unset, construct the in-memory `Database` wrapper.
4. **Test**: Run SSP's test suite with both backends.

### Phase 3: Remove in-memory backend

1. Once the persistent backend is validated, remove `Table.rows`, the in-memory `Database` struct, and the `DbBackend` wrapper for it.
2. Change `Circuit.db` to `SpookyDb` directly (remove trait indirection if desired).
3. Remove the `--persist` flag; persistence is always on.

### Key constraint during migration

The `DbBackend` trait's `get_record_bytes` returns `Option<Vec<u8>>`, not `Result`. The in-memory wrapper should never fail (serialize errors are bugs), so `unwrap` / `expect` is acceptable there. The `SpookyDb` implementation (`src/db/db.rs:457-458`) flattens `Result<Option<Vec<u8>>>` to `Option<Vec<u8>>` via `.ok().flatten()`.

---

## Cargo.toml Changes

Add to SSP's `Cargo.toml` under `[dependencies]`:

```toml
# Path dependency during development (adjust path as needed)
spooky_db_module = { path = "../spooky_db_module" }

# Or, if published to a registry:
# spooky_db_module = "0.1.0"
```

The module's transitive dependencies that SSP must be compatible with:

| Dependency | Version | Purpose |
|---|---|---|
| `redb` | `3.1.0` | Embedded B-tree KV store |
| `smol_str` | `0.3.5` (with `serde` feature) | Already used by SSP |
| `rustc-hash` | `2.1.1` | FxHasher; already used by SSP |
| `serde_json` | `1.0.149` | Bridge type for SpookyValue conversion |
| `xxhash-rust` | `0.8.15` (with `xxh64` feature) | Field name hashing (internal to module) |
| `cbor4ii` | `1.2.2` | CBOR encoding for nested values (internal to module) |
| `arrayvec` | `0.7.6` | Fixed-capacity field collection (internal to module) |

SSP likely already depends on `smol_str`, `rustc-hash`, `serde`, and `serde_json`. Ensure version compatibility. The `redb` dependency is new and adds approximately 200KB to the binary.

---

## Known Friction Points

### 1. Field name recovery is impossible

The binary format stores only xxh64 hashes of field names. Any code path that needs to "dump all fields" of a record requires an external list of field names. This affects:

- `get_row_value` in view.rs: must be told which fields to extract.
- Any debug/logging that prints full record contents.
- Schema inference from stored records.

**Resolution**: Maintain a per-table field name registry in SSP (e.g., `FastMap<SmolStr, Vec<SmolStr>>` populated from the first ingested record per table, or from the schema definition). Pass field names to `get_record_typed` or iterate with `SpookyRecord::get_field` for each known name.

### 2. SpookyValue type collision

Both crates export `SpookyValue`. Any file that imports both will get a name collision.

**Resolution**: Use explicit paths:
```rust
use ssp::SpookyValue as SspValue;
use spooky_db_module::spooky_value::SpookyValue as ModuleValue;
```
Or avoid importing the module's `SpookyValue` entirely -- SSP code should rarely need it directly. Use `serde_json::Value` as the bridge type.

### 3. FastMap name collision

`spooky_db_module::spooky_value::FastMap` is a `BTreeMap`. `spooky_db_module::db::types::FastMap` is an FxHashMap. SSP's `FastMap` is also an FxHashMap. Only import `db::types::FastMap` (which matches SSP's type).

**Resolution**: Never import `spooky_db_module::spooky_value::FastMap`. Use `spooky_db_module::db::types::FastMap` or SSP's own `FastMap` (they are the same type).

### 4. ZSet borrow lifetime vs mutation

`get_table_zset` returns `Option<&ZSet>`, borrowing from `SpookyDb`. This borrow conflicts with `&mut self` methods like `apply_mutation`. SSP's view evaluation currently borrows `&table.zset` while also reading rows -- this works because rows and zset are in separate fields.

**Resolution**: The current SSP flow processes mutations and view evaluation sequentially (mutate, then evaluate). As long as no `&mut self` call happens during view evaluation, the `&ZSet` borrow is safe. If SSP needs concurrent mutation + evaluation, clone the ZSet snapshot before evaluation (one allocation per view tick).

### 5. get_record_bytes allocates

`get_record_bytes` returns `Vec<u8>` (copied from redb's memory-mapped region). This is one allocation per record read. For join probe loops with many hits, this may be significant.

**Resolution**: For join hot paths, consider caching recently read records in a `FastMap<SmolStr, Vec<u8>>` scoped to the view evaluation tick. Alternatively, use redb's read transaction directly for batch reads (requires exposing a lower-level API from SpookyDb).

### 6. 32-field limit

Records with more than 32 top-level fields will fail serialization with `RecordError::TooManyFields`.

**Resolution**: This is unlikely to be hit in practice (SSP records are typically small). If needed, increase the `ArrayVec` capacity constant in `src/serialization.rs` (search for `ArrayVec<..., 32>`). This is a one-line change but increases stack usage per serialization call.

### 7. `to_value()` returns `SpookyValue::Null`

The `SpookyReadable::to_value()` method (`src/spooky_record/read_op.rs:200`) always returns `SpookyValue::Null`. Do not use it.

**Resolution**: Use `get_record_typed(table, id, fields)` or build the value manually from typed accessors. This is a structural limitation of the format (no field names stored).

### 8. Operation enum duplication

SSP and the module both define `Operation { Create, Update, Delete }` with identical semantics. They are different Rust types.

**Resolution**: Write a trivial conversion function:
```rust
fn translate_op(ssp_op: ssp::Operation) -> spooky_db_module::db::Operation {
    match ssp_op {
        ssp::Operation::Create => spooky_db_module::db::Operation::Create,
        ssp::Operation::Update => spooky_db_module::db::Operation::Update,
        ssp::Operation::Delete => spooky_db_module::db::Operation::Delete,
    }
}
```

### 9. Version field convention

The module supports an optional `version: Option<u64>` in `DbMutation` and stores it in `VERSION_TABLE`. SSP does not currently use record versioning. The `"spooky_rv"` magic field name convention (documented in CLAUDE.md) is not enforced by the module -- version is passed explicitly.

**Resolution**: Ignore versioning initially (pass `version: None`). Add versioning later if SSP needs conflict detection or sync.

### 10. Startup cost

`SpookyDb::new()` scans all records to rebuild ZSets. For large databases, this adds startup latency (approximately 20-100ms per million records).

**Resolution**: Acceptable for most use cases. If startup time becomes critical, consider persisting ZSet snapshots separately (not yet implemented in the module).

---

## What the Module Does NOT Cover

The following remain entirely in SSP and are not affected by this integration:

- **View evaluation logic** (`view.rs`): Scan, Filter, Join, Aggregate operators. The module provides data access; view semantics stay in SSP.
- **Circuit serialization** (`circuit.rs`): `load_from_json`, `save_to_json`, circuit config. The module persists record data, not circuit definitions.
- **Dependency tracking** (`circuit.rs`): `dependency_list: FastMap<TableName, DependencyList>` is unchanged.
- **View definitions and queries**: Query parsing, view creation, operator trees. These are SSP-only.
- **SurrealDB connection**: If SSP has a SurrealDB adapter, it is unrelated to this module.
- **SpookyValue semantics**: SSP's SpookyValue comparison, ordering, and display logic. The module's SpookyValue is not used at the SSP layer.
- **Networking / API layer**: HTTP endpoints, WebSocket handlers, etc.
- **DBSP incremental computation**: Delta propagation, change detection, fixpoint iteration. The module only provides storage; incremental semantics stay in SSP.
