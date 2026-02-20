# SpookyDb — Final Architecture

> Analysis of `my_brainstorms.md` + adjustments for correctness and performance.
> Generated: 2026-02-20

---

## TL;DR — Three Rules (unchanged from brainstorm)

1. **One write transaction per batch** — never `begin_write()` inside a loop
2. **ZSet always in memory** — zero I/O for view membership queries
3. **Records always on disk** — point lookups only when a specific key is needed

---

## Issues Found in the Brainstorm

### [CRITICAL] Hard contradiction between Part 1 and Part 2

Part 1 (performance-first concept) says:
> "ZSet always in memory — ZSET_TABLE in redb is only written on checkpoint/shutdown"

Part 2 (integration architecture), Section 8, says:
> "ZSet stays in redb, not memory. The ZSet is the source of truth."

Part 2, Section 4 then lists `ZSET_TABLE` as a required redb table with per-record lookup methods.

**These are mutually exclusive.** The brainstorm implements both simultaneously, which produces an incoherent design.

**Resolution: Part 1 is correct.** The ZSet is iterated in full on every view evaluation tick (the `eval_snapshot(Scan)` path). If ZSet lives in redb, that's an O(N) range scan on every tick. If it lives in memory, it's an O(1) HashMap lookup followed by an in-order iterator. For a system doing continuous view evaluation this difference is decisive. **Eliminate ZSET_TABLE entirely.**

---

### [CRITICAL] `get_record_value` cannot be implemented as written

Both parts propose:
```rust
pub fn get_record_value(&self, table: &str, id: &str) -> Result<Option<SpookyValue>, SpookyDbError>
```

This returns a `SpookyValue::Object` with named keys. It cannot work because:
- The binary SpookyRecord format stores only xxh64 **hashes** of field names, not the names themselves
- `SpookyReadable::to_value()` always returns `SpookyValue::Null` — this is documented as a permanent constraint
- A `SpookyValue::Object { "name": "Alice" }` cannot be reconstructed from a hash alone

**Resolution:** Replace `get_record_value` with two functions:
1. `get_record_bytes(table, id) -> Result<Option<Vec<u8>>, SpookyDbError>` — returns raw bytes; caller wraps in `SpookyRecord::new(&bytes, count)` and uses typed field accessors (`get_str`, `get_i64`, etc.) by name
2. `get_record_typed(table, id, fields: &[&str]) -> Result<Option<SpookyValue>, SpookyDbError>` — reconstructs a partial `SpookyValue::Object` for only the named fields provided by the caller; unknown fields (hashes with no matching name) are skipped

This constraint comes from the record format. It is not a limitation of the db layer.

---

### [DESIGN] `DbMutation` should hold pre-serialized bytes, not CBOR

Both parts propose:
```rust
pub struct DbMutation {
    pub cbor: Option<cbor4ii::core::Value>,  // Part 1
}
// or
pub enum DbMutation {
    Upsert { data: Vec<u8> },               // Part 2
}
```

Part 1 serializes CBOR → SpookyRecord **inside** `apply_batch`, during the write transaction. This is the wrong place. The write lock is exclusive in redb — holding it longer to do CPU work (serialization) blocks all other writers.

**Resolution:** `DbMutation` holds `data: Vec<u8>` (already-serialized SpookyRecord bytes). Caller serializes via `from_cbor(&cbor)` **before** calling `apply_batch`. The transaction then only does I/O: insert bytes into redb. This minimizes write lock hold time.

```
Caller:
  serialize all records (CPU, no lock)
  → apply_batch(mutations)
      begin_write()    ← lock acquired here
      insert bytes     ← pure I/O
      commit()         ← lock released here
```

---

### [DESIGN] `TABLES_TABLE` is redundant — eliminate it

Part 2 adds a fourth redb table:
```rust
const TABLES_TABLE: TableDefinition<&str, u64> = TableDefinition::new("tables");
```

This is unnecessary. On startup, `rebuild_zsets_from_records()` does a sequential scan of `RECORDS_TABLE` and parses all `"table:id"` keys. This gives us every table name for free in the same pass. The `SpookyDb::zsets` field itself is the table registry: `zsets.keys()` = all known tables, `zsets.get(name).map(|z| z.len())` = record count per table.

Adding `TABLES_TABLE` means two writes on every insert (RECORDS_TABLE + TABLES_TABLE) and an extra table to keep consistent. No benefit.

---

### [DESIGN] `BatchMutationResult` uses `String` instead of `SmolStr`

```rust
// Brainstorm (wrong):
pub membership_deltas: FastMap<String, ZSet>,
pub content_updates:   FastMap<String, FastHashSet<SmolStr>>,

// Correct:
pub membership_deltas: FastMap<SmolStr, ZSet>,
pub content_updates:   FastMap<SmolStr, FastHashSet<SmolStr>>,
```

`SmolStr` is already used everywhere for table names. Using `String` here creates an unnecessary conversion and type inconsistency.

---

### [DESIGN] Method receiver inconsistency for ZSet mutations

Part 2 declares ZSet write operations on `&self`:
```rust
pub fn apply_zset_delta(&self, key: &str, delta: i64) -> Result<i64, SpookyDbError>
pub fn set_zset_weight(&self, key: &str, weight: i64) -> Result<(), SpookyDbError>
```

But `SpookyDb::zsets` is not behind an `Arc<Mutex>` — it's a plain `FastMap` in the struct. Mutating it requires `&mut self`. The `&self` signature only works if ZSet were in redb (which it isn't in the chosen design). All ZSet-mutating methods must be `&mut self`.

---

### [PERF] `get_table_zset` in Part 2 allocates on every call

Part 2's version:
```rust
pub fn get_table_zset(&self, table: &str) -> Result<FastMap<SmolStr, i64>, SpookyDbError>
```

This allocates a new `FastMap` from a redb range scan on every call. `eval_snapshot(Scan)` calls this on every view tick. For a table with 100k records, that's 100k `SmolStr` allocations per tick.

Part 1's version is correct:
```rust
pub fn get_table_zset(&self, table: &str) -> Option<&ZSet>
```

Zero allocations. Returns a borrow directly from `self.zsets`. The view layer borrows it for the duration of a single tick.

---

### [PERF] `iter_table_zset` returning `Vec` is fine but naming is misleading

The brainstorm has both `iter_table_zset` (returns `Vec`) and `get_table_zset` (returns `FastMap`). With in-memory ZSets, `get_table_zset` returns `&ZSet` which already supports `.iter()`. There's no need for a separate `iter_table_zset` function — it's redundant. The caller calls `db.get_table_zset(table)?.iter()`.

---

### [COSMETIC] Key format invariant undocumented

Key format `"table_name:record_id"` is used throughout but the constraint that **table names must not contain `:`** is never stated. This is important because `split_once(':')` on startup correctly handles IDs that contain `:` (it splits on the first occurrence), but a table name with `:` would silently corrupt the namespace. Document this constraint in the code.

---

## Final Architecture

### Struct

```rust
pub struct SpookyDb {
    /// On-disk store. Single owner — Circuit holds SpookyDb exclusively.
    /// No Arc, no Mutex. Same ownership model as the old Database.
    db: redb::Database,

    /// Hot ZSet per table. Fully in memory.
    /// Key: table name → Value: (record_id → weight)
    /// Rebuilt from RECORDS_TABLE on startup via rebuild_zsets_from_records().
    /// Zero I/O for all ZSet reads (view evaluation).
    /// INVARIANT: table name must not contain ':'.
    zsets: FastMap<SmolStr, ZSet>,
}
```

No `Arc`, no `Mutex`, no `ZSET_TABLE`, no `TABLES_TABLE`.

---

### redb Tables (two only)

```rust
/// Key: "table_name:record_id"  →  Value: SpookyRecord bytes
const RECORDS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("records");

/// Key: "table_name:record_id"  →  Value: version u64
const VERSION_TABLE: TableDefinition<&str, u64> = TableDefinition::new("versions");
```

**Why flat string keys over composite `(&str, &str)` keys?**
- Flat key `"table:id"` enables O(log N) prefix range scans with standard string ranges
- No tuple key ordering complexity (the `get_all_zset` break-on-table-change hack in `db_current.rs` disappears)
- Matches the existing `make_zset_key()` convention already in use — zero migration cost
- Simpler redb TableDefinition type (no composite key trait requirements)

---

### Supporting Types

```rust
/// All errors from the db layer in one place.
pub enum SpookyDbError {
    Redb(redb::Error),
    Serialization(String),
    InvalidKey(String),   // table name contains ':', or key format is wrong
}

impl From<redb::Error> for SpookyDbError {
    fn from(e: redb::Error) -> Self { SpookyDbError::Redb(e) }
}

/// A single mutation for use in apply_batch.
/// data MUST be pre-serialized SpookyRecord bytes (from from_cbor / serialize_into).
/// Serialization happens BEFORE begin_write() to minimize write lock hold time.
pub struct DbMutation {
    pub table:   SmolStr,
    pub id:      SmolStr,
    pub op:      Operation,
    pub data:    Option<Vec<u8>>,  // None for Delete; Some(bytes) for Create/Update
    pub version: Option<u64>,      // Explicit version, or extracted from "spooky_rv" field
}

pub enum Operation {
    Create,   // weight += 1
    Update,   // weight unchanged (record bytes replaced)
    Delete,   // weight -= 1, record removed
}

impl Operation {
    pub fn weight(&self) -> i64 {
        match self {
            Operation::Create => 1,
            Operation::Delete => -1,
            Operation::Update => 0,
        }
    }
}

/// Return type for apply_batch. All deltas accumulated in one pass.
pub struct BatchMutationResult {
    /// Per-table ZSet weight deltas. Key: table name.
    pub membership_deltas: FastMap<SmolStr, ZSet>,
    /// Per-table set of record IDs whose content changed (Create or Update).
    pub content_updates: FastMap<SmolStr, FastHashSet<SmolStr>>,
    /// Tables that had any mutation (for invalidation purposes).
    pub changed_tables: Vec<SmolStr>,
}

/// For bulk_load. Data must be pre-serialized SpookyRecord bytes.
pub struct BulkRecord {
    pub table: SmolStr,
    pub id:    SmolStr,
    pub data:  Vec<u8>,
}
```

---

### Complete Function List

#### Construction

```rust
/// Open or create the database at path.
/// Initializes redb tables on first open.
/// Rebuilds in-memory ZSets from RECORDS_TABLE scan on every open.
/// INVARIANT: table names in RECORDS_TABLE keys must not contain ':'.
pub fn new(path: impl AsRef<Path>) -> Result<Self, SpookyDbError>

/// Internal. Sequential scan of RECORDS_TABLE on startup.
/// Parses "table:id" keys, sets zsets[table][id] = 1 for every key found.
/// O(N records) sequential read — fast even for millions of records.
/// Also populates self.zsets with all table names (no TABLES_TABLE needed).
fn rebuild_zsets_from_records(&mut self) -> Result<(), SpookyDbError>
```

**Why weight=1 for all records on rebuild?**
The valid ZSet weights in this system are 1 (present) and absent (removed). A record exists in RECORDS_TABLE if and only if it has weight 1. A deleted record has its RECORDS_TABLE entry removed. So the rebuild invariant holds: every key in RECORDS_TABLE = weight 1.

---

#### Write Operations — all `&mut self` (ZSet mutation required)

```rust
/// Single mutation: record write + ZSet update in one redb write transaction.
/// `data` must be pre-serialized SpookyRecord bytes.
/// Returns (zset_key, weight_delta) for circuit.rs to accumulate into BatchDeltas.
pub fn apply_mutation(
    &mut self,
    table: &str,
    op: Operation,
    id: &str,
    data: Option<&[u8]>,   // None for Delete; Some(bytes) for Create/Update
    version: Option<u64>,
) -> Result<(SmolStr, i64), SpookyDbError>

/// Batch mutations in ONE write transaction (one fsync).
/// `mutations[i].data` must be pre-serialized by the caller BEFORE calling this.
/// This is the critical performance path: N mutations = 1 transaction = 1 fsync.
pub fn apply_batch(
    &mut self,
    mutations: Vec<DbMutation>,
) -> Result<BatchMutationResult, SpookyDbError>

/// Bulk initial load: single write transaction, sets all ZSet weights to 1.
/// Use for startup hydration / init_load in circuit.rs.
pub fn bulk_load(
    &mut self,
    records: impl IntoIterator<Item = BulkRecord>,
) -> Result<(), SpookyDbError>
```

**Write path for apply_batch (step by step):**
```
Caller pre-serializes all records (CPU, no lock held):
    for each mutation: from_cbor(&cbor) → Vec<u8>

apply_batch(mutations):
    1. begin_write()                          ← write lock acquired
    2. open RECORDS_TABLE + VERSION_TABLE     ← table handles (cheap)
    3. for each mutation:
        a. update self.zsets[table][id]       ← in-memory, O(1)
        b. insert/remove RECORDS_TABLE        ← redb B-tree insert
        c. insert/remove VERSION_TABLE        ← redb B-tree insert
        d. accumulate membership_deltas       ← in-memory
    4. commit()                               ← fsync, write lock released
    5. return BatchMutationResult
```

---

#### Read Operations — all `&self` (no ZSet mutation)

```rust
/// Raw bytes for a record. Primary read path for SpookyRecord usage.
/// Caller wraps result in SpookyRecord::new(&bytes, field_count) for zero-copy field access.
/// Opens and closes a redb read transaction internally.
/// redb read transactions are cheap (no lock contention with other readers).
pub fn get_record_bytes(
    &self,
    table: &str,
    id: &str,
) -> Result<Option<Vec<u8>>, SpookyDbError>

/// Reconstruct a partial SpookyValue::Object from a stored record.
/// Only fields whose names are in `fields` are included in the result.
/// Fields not in `fields` are skipped (their hashes have no matching name to use as key).
/// Returns None if the record does not exist.
/// Use this for view evaluation paths that need a named SpookyValue (compatibility layer).
pub fn get_record_typed(
    &self,
    table: &str,
    id: &str,
    fields: &[&str],
) -> Result<Option<SpookyValue>, SpookyDbError>

/// Version for sync conflict detection.
pub fn get_version(
    &self,
    table: &str,
    id: &str,
) -> Result<Option<u64>, SpookyDbError>
```

**Why `get_record_bytes` is the primary path:**

The caller knows which fields it needs. Rather than deserializing a full `SpookyValue` and then accessing one field, the caller can:
```rust
let bytes = db.get_record_bytes("users", "alice")?.unwrap();
let (buf, count) = from_bytes(&bytes)?;
let record = SpookyRecord::new(buf, count);
let age = record.get_i64("age");       // O(log N) binary search
let name = record.get_str("name");     // O(log N) binary search
// or, for hot paths with FieldSlot:
let age_slot = record.resolve("age");
let age = record.get_i64_at(&age_slot); // O(1) after first resolve
```

Zero extra allocations beyond the `Vec<u8>` copy from redb (unavoidable — data must outlive the read transaction).

---

#### ZSet Operations — pure memory, all O(1)

```rust
/// Full ZSet for a table. Pure memory read, zero I/O.
/// Returns None if the table has no records (or was never registered).
/// This is what eval_snapshot(Scan) borrows for the duration of a view tick.
/// BORROW LIFETIME: valid until the next &mut self call (apply_mutation / apply_batch).
pub fn get_table_zset(&self, table: &str) -> Option<&ZSet>

/// Weight for a single record key. Pure memory read, zero I/O.
/// Returns 0 if absent (standard ZSet semantics).
pub fn get_zset_weight(&self, table: &str, id: &str) -> i64

/// Apply a pre-computed ZSet delta in memory only (no redb write).
/// Used when the caller has already committed records and only needs to sync
/// the in-memory ZSet (e.g. after a checkpoint load).
pub fn apply_zset_delta_memory(&mut self, table: &str, delta: &ZSet)
```

Note: There is **no `set_zset_weight`** as a public function. ZSet weights are only changed as a side effect of `apply_mutation` / `apply_batch` / `bulk_load`. Keeping ZSet mutations private to write operations ensures the ZSet never drifts from RECORDS_TABLE.

---

#### Table Info — pure memory, all O(1)

```rust
/// Does this table have at least one record?
/// Pure memory: checks self.zsets.contains_key(table).
pub fn table_exists(&self, table: &str) -> bool

/// All known table names (derived from self.zsets.keys()).
pub fn table_names(&self) -> impl Iterator<Item = &SmolStr>

/// Record count for a table (from in-memory ZSet size).
/// O(1) — ZSet entries = records present.
pub fn table_len(&self, table: &str) -> usize

/// Explicitly register an empty table.
/// Creates an empty ZSet entry for the table (so table_exists() returns true
/// even before the first record is inserted).
pub fn ensure_table(&mut self, table: &str)
```

---

### Write Path Summary

```
╔══════════════════════════════════════════════════════════════════╗
║  apply_batch (hot path)                                          ║
╠══════════════════════════════════════════════════════════════════╣
║  Caller (before call):                                           ║
║    from_cbor(&cbor_value) → Vec<u8>  [CPU, no lock]             ║
║                                                                  ║
║  Inside apply_batch:                                             ║
║    begin_write()            [write lock acquired]                ║
║    for each mutation:                                            ║
║      zsets[table][id] += weight      [memory, O(1)]             ║
║      RECORDS_TABLE.insert(key, data) [redb B-tree, O(log N)]    ║
║      VERSION_TABLE.insert(key, ver)  [redb B-tree, O(log N)]    ║
║    commit()                 [1 fsync, write lock released]       ║
╚══════════════════════════════════════════════════════════════════╝
```

---

### Read Path Summary

```
╔══════════════════════════════════════════════════════════════════╗
║  ZSet read (eval_snapshot Scan operator)                         ║
╠══════════════════════════════════════════════════════════════════╣
║  get_table_zset("users") → &ZSet   [memory, O(1), zero I/O]    ║
╠══════════════════════════════════════════════════════════════════╣
║  Record read (get_row_value, predicate eval)                     ║
╠══════════════════════════════════════════════════════════════════╣
║  get_record_bytes("users", id) → Vec<u8>                        ║
║    begin_read()             [no lock contention with readers]    ║
║    RECORDS_TABLE.get(key)   [redb B-tree, O(log N)]             ║
║    val.to_vec()             [1 allocation, copy out of redb]    ║
║    txn dropped              [no explicit close needed]           ║
║                                                                  ║
║  SpookyRecord::new(&bytes, count)   [zero-copy view]            ║
║  record.get_str("field")            [O(log N) or O(1) w/ slot] ║
╚══════════════════════════════════════════════════════════════════╝
```

---

### Startup Recovery

```
SpookyDb::new(path):
  1. redb::Database::create(path)    ← opens or creates file
  2. ensure RECORDS_TABLE + VERSION_TABLE exist (write txn)
  3. rebuild_zsets_from_records():
       begin_read()
       for each (key, _) in RECORDS_TABLE.iter():
           let (table, id) = key.split_once(':')   ← first ':' only
           self.zsets.entry(table).or_default().insert(id, 1)
       txn dropped
```

Sequential scan of a B-tree reads pages in order. For 1M records at ~20 bytes/key, this is roughly 20MB of sequential I/O — typically 20–100ms on an SSD.

---

### What Happens to `db_current.rs`

Delete it. The new `db.rs` replaces it entirely.

Functions from `db_current.rs` that carry over (adapted):
- `apply_batch` — keeping the one-transaction-per-batch idea, but with pre-serialized bytes
- `get_version` — identical purpose
- `ensure_table` — same name, different implementation (in-memory only)

Functions from `db_current.rs` that are dropped:
| Function | Reason |
|---|---|
| `reserve()` | No-op — artifact of in-memory HashMap preallocation |
| `len()` / `is_empty()` | Was O(N) redb scan — replaced by O(1) `table_len()` from ZSet |
| `contains_key()` | Was broken (called `get()` → `to_value()` → Null) — `get_zset_weight > 0` replaces it |
| `get() -> SpookyValue` | Impossible without field names — replaced by `get_record_bytes` |
| `apply_mutation_impl` (private) | Replaced by the unified `apply_mutation` / `apply_batch` design |
| `get_all_zset()` with redb scan | Replaced by `get_table_zset()` returning `&ZSet` (memory) |

---

### DbBackend Trait (for incremental migration)

```rust
/// Thin adapter trait. Implement for both the old in-memory Database
/// and the new SpookyDb. Lets circuit.rs migrate incrementally.
pub trait DbBackend {
    /// Zero-copy ZSet access. Borrowed from memory — zero I/O.
    fn get_table_zset(&self, table: &str) -> Option<&ZSet>;

    /// Raw bytes for a record. Caller wraps in SpookyRecord.
    fn get_record_bytes(&self, table: &str, id: &str) -> Option<Vec<u8>>;

    /// Register an empty table.
    fn ensure_table(&mut self, table: &str);

    /// Single mutation: record write + ZSet update.
    fn apply_mutation(
        &mut self,
        table: &str,
        op: Operation,
        id: &str,
        data: Option<&[u8]>,
        version: Option<u64>,
    ) -> (SmolStr, i64);

    /// Batch mutations in one transaction.
    fn apply_batch(&mut self, mutations: Vec<DbMutation>) -> BatchMutationResult;

    /// Bulk initial load.
    fn bulk_load(&mut self, records: impl IntoIterator<Item = BulkRecord>);

    /// Weight for one record. Returns 0 if absent.
    fn get_zset_weight(&self, table: &str, id: &str) -> i64;
}
```

Note: `get_table_zset` returns `Option<&ZSet>` not `Cow<'_, ZSet>`. The `Cow` approach from the brainstorm was only needed if ZSet could be either in-memory (borrowed) or from redb (owned). Since we've committed to in-memory ZSets for both the old and new backend, `&ZSet` is sufficient.

---

### File Structure

```
src/db/
  mod.rs        ← exports SpookyDb, DbMutation, Operation, BatchMutationResult,
                   BulkRecord, SpookyDbError, DbBackend
  db.rs         ← SpookyDb implementation (the new file)
  types.rs      ← SpookyDbError, DbMutation, Operation, BatchMutationResult,
                   BulkRecord, FastMap/ZSet type aliases
                   (clean up: remove FieldType, SpookyFieldSchema, SpookyDBSchema)
  backend.rs    ← DbBackend trait (new, optional — add when needed for migration)
```

---

### Implementation Order

| Step | What | Why |
|---|---|---|
| 1 | Clean `types.rs`: add `Operation`, `DbMutation`, `BatchMutationResult`, `BulkRecord`; implement `From<redb::Error> for SpookyDbError`; delete dead structs | Unblocks everything |
| 2 | `SpookyDb::new()` with correct struct initialization (`Self { db, zsets: FastMap::default() }`) | Fixes compile error |
| 3 | `rebuild_zsets_from_records()` | Core startup logic |
| 4 | `get_record_bytes()` + `get_version()` | Basic read path, unblocks test |
| 5 | `apply_mutation()` (single record + ZSet update) | Single-record write path |
| 6 | `apply_batch()` (one txn for N records) | The performance-critical path |
| 7 | `bulk_load()` (single txn for init hydration) | Startup write path |
| 8 | `get_table_zset()`, `get_zset_weight()`, `table_len()`, `table_exists()`, `table_names()`, `ensure_table()` | ZSet + table info API |
| 9 | `get_record_typed()` (partial SpookyValue reconstruction with field names) | Compatibility layer for view.rs |
| 10 | `DbBackend` trait + `impl DbBackend for SpookyDb` | Migration wire-up |

---

### Comparison: Brainstorm vs Final

| Topic | Brainstorm | Final | Change |
|---|---|---|---|
| ZSet location | Contradictory (Part 1: memory, Part 2: redb) | Memory only | **Resolved: Part 1 wins** |
| ZSET_TABLE | Present in Part 2 | Eliminated | **Removed** |
| TABLES_TABLE | Present in Part 2 | Eliminated | **Redundant — ZSet keys are table names** |
| `get_record_value` | Returns `SpookyValue` | Replaced by `get_record_bytes` + `get_record_typed(fields)` | **Required by format constraint** |
| `DbMutation.cbor` | `cbor4ii::core::Value` | `Vec<u8>` (pre-serialized) | **Serialize before lock** |
| `BatchMutationResult` | `FastMap<String, ZSet>` | `FastMap<SmolStr, ZSet>` | **Type consistency** |
| ZSet write receiver | `&self` (wrong) | `&mut self` | **In-memory mutation requires mut** |
| `get_table_zset` return | `Result<FastMap>` (owned alloc) | `Option<&ZSet>` (borrowed) | **Zero allocation** |
| `DbBackend.get_table_zset` | `Cow<'_, ZSet>` | `Option<&ZSet>` | **Cow unnecessary with in-memory** |
| redb tables | 2–4 tables (inconsistent) | 2 tables: RECORDS + VERSIONS | **Simplified** |
| Key format | `"table:id"` flat string | `"table:id"` flat string | **Unchanged — correct** |
| Struct ownership | `SpookyDb` no Arc (Part 1) / confused (Part 2) | No Arc, no Mutex — owned by Circuit | **Confirmed Part 1** |
