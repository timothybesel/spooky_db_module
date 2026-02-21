# SpookyDb — LRU Row Cache Implementation Plan

> Option B: Bounded LRU cache — write-through with redb read fallback.
> Replaces `rows: FastMap<TableName, RowStore>` (unbounded, full copy of all record bytes)
> with `row_cache: lru::LruCache<(SmolStr, SmolStr), Vec<u8>>` (bounded by record count).

---

## 1. Problem Statement

The current implementation stores every record's serialized bytes in a `FastMap`:

```
rows: FastMap<TableName, RowStore>
     = HashMap<SmolStr, HashMap<SmolStr, Vec<u8>>>
```

At 20 GB of record data, this requires ~20 GB of RAM in addition to the redb
memory-mapped file. For large databases this is impractical.

**Goal**: bound peak memory to `capacity × avg_record_bytes` while keeping hot-path
reads (recently written / frequently accessed records) at in-memory speed.

---

## 2. Design: Write-Through LRU Cache

### Cache key and value

```
key:   (table_name: SmolStr, record_id: SmolStr)
value: Vec<u8>  — serialized SpookyRecord bytes (same as current RowStore value)
```

### Eviction policy

Least-Recently-Used by **write time** (not read time). Why:

- Writes always call `cache.put(key, bytes)` via `&mut self` — promotes entry.
- Reads call `cache.peek(key)` via `&self` — returns `Option<&V>` without updating recency.
  LRU promotion on reads requires `&mut self` (`cache.get()`), which conflicts with the
  existing `DbBackend` trait's `&self` contract on read methods.

**Consequence**: entries that are only read (not re-written) will eventually be evicted
by newer writes. For a streaming pipeline where data is written then immediately read,
this is fine — writes keep the working set warm. For heavy read-only access to old data,
callers get a redb fallback (slower but correct).

### Read paths

| Method | Cache hit | Cache miss |
|---|---|---|
| `get_record_bytes(&self)` | `peek()` → clone | redb read (no cache update) |
| `get_row_record(&self)` | `peek()` → borrow | `None` (caller must use `get_record_bytes`) |
| `get_row_record_bytes(&self)` | `peek()` → `&[u8]` | `None` |

### Write paths

| Operation | Cache action |
|---|---|
| `Create` / `Update` (with data) | `cache.put((table, id), bytes)` |
| `Delete` | `cache.pop((table, id))` |
| `bulk_load` | `cache.put(...)` per record (capped at capacity) |

### Startup

`rebuild_from_records` populates **only the ZSet** (as before for the ZSet; previously
also populated `rows`). The LRU cache starts **cold**. First reads after startup go to
redb; the cache warms as records are written or as a cache-populating read path is used
(see §9 Optional Enhancement).

Benefit: startup is faster and uses much less memory for large databases.

---

## 3. File Changes (in order)

### Step 0 — Add `lru` dependency (`Cargo.toml`)

```toml
# Before
smol_str = { version = "0.3.5", features = ["serde"] }

# After — add lru after smol_str
lru = "0.12"
smol_str = { version = "0.3.5", features = ["serde"] }
```

`lru` 0.12 uses `hashbrown` internally and is `no_std` compatible. No feature flags needed.

---

### Step 1 — Update `types.rs`

#### 1a. Remove `RowStore` type alias

`RowStore` is only used in `db.rs` (struct field + `get_row_record_bytes` impl).
After the change it is no longer needed.

```rust
// REMOVE this line:
pub type RowStore = FastMap<RowKey, Vec<u8>>;
```

#### 1b. Add `SpookyDbConfig` struct

Add after the `TableName` type alias:

```rust
use std::num::NonZeroUsize;

/// Configuration for `SpookyDb::new_with_config`.
pub struct SpookyDbConfig {
    /// Maximum number of records to keep in the LRU row cache.
    ///
    /// When this limit is reached, the least-recently-written record is evicted.
    /// Evicted records remain on disk in redb and are re-read on the next access.
    ///
    /// Default: 10 000 records (~10–500 MB depending on average record size).
    /// Set to 0 to use the default.
    pub cache_capacity: NonZeroUsize,
}

impl Default for SpookyDbConfig {
    fn default() -> Self {
        Self {
            cache_capacity: NonZeroUsize::new(10_000).unwrap(),
        }
    }
}
```

---

### Step 2 — Update `db.rs` imports

```rust
// Before:
use super::types::{
    BatchMutationResult, BulkRecord, DbMutation, FastHashSet, FastMap, Operation, RowStore,
    SpookyDbError, TableName, ZSet,
};

// After (remove RowStore, add NonZeroUsize and SpookyDbConfig):
use super::types::{
    BatchMutationResult, BulkRecord, DbMutation, FastHashSet, FastMap, Operation,
    SpookyDbConfig, SpookyDbError, TableName, ZSet,
};
use std::num::NonZeroUsize;
```

---

### Step 3 — Replace struct field

```rust
// Before:
pub struct SpookyDb {
    db: RedbDatabase,
    zsets: FastMap<SmolStr, ZSet>,

    /// In-memory row cache. Key: table_name → (record_id → SpookyRecord bytes).
    rows: FastMap<TableName, RowStore>,
}

// After:
pub struct SpookyDb {
    db: RedbDatabase,
    zsets: FastMap<SmolStr, ZSet>,

    /// Bounded LRU row cache. Key: (table_name, record_id) → SpookyRecord bytes.
    ///
    /// Write-through: populated on every Create/Update/bulk_load. Evicts
    /// least-recently-written entries when capacity is reached. Cache misses
    /// fall back to a redb read (no in-memory update on miss from `&self` methods).
    ///
    /// The cache starts cold on every open — ZSet is rebuilt from a full scan
    /// but record bytes are NOT pre-loaded. Cache warms as records are written.
    row_cache: lru::LruCache<(SmolStr, SmolStr), Vec<u8>>,
}
```

---

### Step 4 — Update constructors

#### 4a. `new()` — keep signature, delegate to `new_with_config`

```rust
// Before:
pub fn new(path: impl AsRef<Path>) -> Result<Self, SpookyDbError> {
    let db = RedbDatabase::create(path)?;
    // ... table init ...
    let mut spooky = SpookyDb {
        db,
        zsets: FastMap::default(),
        rows: FastMap::default(),
    };
    spooky.rebuild_from_records()?;
    Ok(spooky)
}

// After:
/// Open or create the database at `path` with default cache capacity (10 000 records).
pub fn new(path: impl AsRef<Path>) -> Result<Self, SpookyDbError> {
    Self::new_with_config(path, SpookyDbConfig::default())
}

/// Open or create the database at `path` with explicit configuration.
///
/// # Cache capacity
///
/// `config.cache_capacity` bounds peak memory for record bytes. Records beyond
/// this limit are evicted from memory and re-read from redb on demand.
/// Setting a capacity larger than the total number of records is equivalent
/// to the old full-memory design (but without the startup pre-load cost).
pub fn new_with_config(
    path: impl AsRef<Path>,
    config: SpookyDbConfig,
) -> Result<Self, SpookyDbError> {
    let db = RedbDatabase::create(path)?;

    {
        let write_txn = db.begin_write()?;
        let _ = write_txn.open_table(RECORDS_TABLE)?;
        let _ = write_txn.open_table(VERSION_TABLE)?;
        write_txn.commit()?;
    }

    let mut spooky = SpookyDb {
        db,
        zsets: FastMap::default(),
        row_cache: lru::LruCache::new(config.cache_capacity),
    };
    spooky.rebuild_from_records()?;
    Ok(spooky)
}
```

---

### Step 5 — Update `rebuild_from_records`

Remove `rows` population. Only populate ZSet.

```rust
// Before:
fn rebuild_from_records(&mut self) -> Result<(), SpookyDbError> {
    let read_txn = self.db.begin_read()?;
    let table = read_txn.open_table(RECORDS_TABLE)?;
    for entry in table.iter()? {
        let (key_guard, val_guard) = entry?;
        let key_str: &str = key_guard.value();
        if let Some((table_name, id)) = key_str.split_once(':') {
            let t = SmolStr::new(table_name);
            let i = SmolStr::new(id);
            let bytes = val_guard.value().to_vec();

            self.zsets.entry(t.clone()).or_default().insert(i.clone(), 1);
            self.rows.entry(t).or_default().insert(i, bytes);  // ← REMOVE
        }
    }
    Ok(())
}

// After:
/// Sequential scan of RECORDS_TABLE on startup.
///
/// Rebuilds `zsets` (weight=1 per key) from a full scan — O(N records).
/// The LRU row cache starts cold; it warms on the first write or explicit
/// cache-populate call.
///
/// Startup memory: only ZSet keys (SmolStr per record_id) — no record bytes loaded.
fn rebuild_from_records(&mut self) -> Result<(), SpookyDbError> {
    let read_txn = self.db.begin_read()?;
    let table = read_txn.open_table(RECORDS_TABLE)?;
    for entry in table.iter()? {
        let (key_guard, _val_guard) = entry?;
        let key_str: &str = key_guard.value();
        if let Some((table_name, id)) = key_str.split_once(':') {
            let t = SmolStr::new(table_name);
            let i = SmolStr::new(id);
            self.zsets.entry(t).or_default().insert(i, 1);
        }
    }
    Ok(())
}
```

---

### Step 6 — Update `apply_mutation` (in-memory update block)

```rust
// Before (after commit):
let zset = self.zsets.entry(SmolStr::new(table)).or_default();
let row_table = self.rows.entry(SmolStr::new(table)).or_default();

if matches!(op, Operation::Delete) {
    zset.remove(id);
    row_table.remove(id);
} else {
    zset.insert(SmolStr::new(id), 1);
    if let Some(bytes) = data {
        row_table.insert(SmolStr::new(id), bytes.to_vec());
    }
}

// After:
let zset = self.zsets.entry(SmolStr::new(table)).or_default();

if matches!(op, Operation::Delete) {
    zset.remove(id);
    self.row_cache.pop(&(SmolStr::new(table), SmolStr::new(id)));
} else {
    zset.insert(SmolStr::new(id), 1);
    if let Some(bytes) = data {
        self.row_cache.put(
            (SmolStr::new(table), SmolStr::new(id)),
            bytes.to_vec(),
        );
    }
}
```

---

### Step 7 — Update `apply_batch` (in-memory update loop)

```rust
// Before (per-mutation in-memory update):
let zset = self.zsets.entry(table.clone()).or_default();
let row_table = self.rows.entry(table.clone()).or_default();

if matches!(op, Operation::Delete) {
    zset.remove(&id);
    row_table.remove(&id);
    // ...
} else {
    zset.insert(id.clone(), 1);
    if let Some(bytes) = data {
        row_table.insert(id.clone(), bytes);
    }
    // ...
}

// After:
let zset = self.zsets.entry(table.clone()).or_default();

if matches!(op, Operation::Delete) {
    zset.remove(&id);
    self.row_cache.pop(&(table.clone(), id.clone()));
    // ...
} else {
    zset.insert(id.clone(), 1);
    if let Some(bytes) = data {
        self.row_cache.put((table.clone(), id.clone()), bytes);
    }
    // ...
}
```

Note: `table` and `id` are `SmolStr` in the loop body (destructured from `DbMutation`),
so `.clone()` is SmolStr clone — inline for short strings (≤22 bytes), heap for longer.

---

### Step 8 — Update `bulk_load` (in-memory update block)

```rust
// Before:
for BulkRecord { table, id, data, .. } in records {
    self.zsets.entry(table.clone()).or_default().insert(id.clone(), 1);
    self.rows.entry(table).or_default().insert(id, data);
}

// After:
for BulkRecord { table, id, data, .. } in records {
    self.zsets.entry(table.clone()).or_default().insert(id.clone(), 1);
    self.row_cache.put((table, id), data);
}
```

`bulk_load` may insert more records than `cache_capacity`. The LRU will evict old entries
automatically as new ones are inserted — this is correct behavior. The ZSet always
contains all records regardless of cache state.

---

### Step 9 — Rewrite `get_record_bytes`

This is the most important change: add redb fallback on cache miss.

```rust
// Before:
pub fn get_record_bytes(&self, table: &str, id: &str) -> Option<Vec<u8>> {
    self.rows.get(table)?.get(id).cloned()
}

// After:
/// Fetch SpookyRecord bytes for a record.
///
/// **Fast path** (cache hit): `peek()` from LRU row cache — zero I/O, ~50 ns.
/// **Slow path** (cache miss): redb read transaction — ~1–10 µs on warm OS cache.
///
/// Returns `None` if the record is absent from the ZSet (deleted or never written).
///
/// Note: cache misses do NOT populate the cache (requires `&self`). The cache is
/// written only by Create/Update/bulk_load paths. To populate the cache after a miss,
/// re-write the record or call `warm_cache` (see §9 Optional Enhancement).
pub fn get_record_bytes(&self, table: &str, id: &str) -> Option<Vec<u8>> {
    // ZSet guard — zero I/O, avoids unnecessary redb open for absent records.
    let present = self
        .zsets
        .get(table)
        .and_then(|z| z.get(id))
        .copied()
        .unwrap_or(0)
        > 0;
    if !present {
        return None;
    }

    // Cache hit — peek does not update LRU recency (requires &mut self).
    let cache_key = (SmolStr::new(table), SmolStr::new(id));
    if let Some(bytes) = self.row_cache.peek(&cache_key) {
        return Some(bytes.clone());
    }

    // Cache miss — fall back to redb.
    let db_key = make_key(table, id);
    let read_txn = self.db.begin_read().ok()?;
    let tbl = read_txn.open_table(RECORDS_TABLE).ok()?;
    tbl.get(db_key.as_str())
        .ok()?
        .map(|guard| guard.value().to_vec())
}
```

**Error swallowing**: redb errors become `None` (consistent with current `Option` return type).
If caller needs error distinction, use `get_version` (which returns `Result`) as a pattern
for a future `get_record_bytes_result(&self) -> Result<Option<Vec<u8>>, SpookyDbError>`.

---

### Step 10 — Rewrite `get_row_record`

```rust
// Before:
pub fn get_row_record<'a>(&'a self, table: &str, id: &str) -> Option<SpookyRecord<'a>> {
    let bytes = self.rows.get(table)?.get(id)?;
    let (buf, count) = from_bytes(bytes).ok()?;
    Some(SpookyRecord::new(buf, count))
}

// After:
/// Zero-copy borrowed SpookyRecord for the view evaluation hot path.
///
/// Returns `Some(SpookyRecord<'a>)` if and only if the record is in the LRU
/// row cache. Returns `None` if the record is absent OR if it exists on disk
/// but has been evicted from the cache.
///
/// **Cache miss fallback**: call `get_record_bytes(table, id)` for a heap-
/// allocated copy that reads from redb if necessary.
///
/// For the streaming pipeline hot path (write then read in the same tick),
/// records are always in cache — no redb I/O.
pub fn get_row_record<'a>(&'a self, table: &str, id: &str) -> Option<SpookyRecord<'a>> {
    // ZSet guard first.
    let present = self
        .zsets
        .get(table)
        .and_then(|z| z.get(id))
        .copied()
        .unwrap_or(0)
        > 0;
    if !present {
        return None;
    }

    // Cache-only — peek returns &Vec<u8> with lifetime 'a.
    let cache_key = (SmolStr::new(table), SmolStr::new(id));
    let bytes = self.row_cache.peek(&cache_key)?;
    let (buf, count) = from_bytes(bytes).ok()?;
    Some(SpookyRecord::new(buf, count))
}
```

**Semantic change**: previously `None` meant "record deleted / never existed". Now it also
means "record exists on disk but was evicted from cache". Document this clearly. Callers
in the hot path (view evaluation) are unaffected because they always write before reading.

---

### Step 11 — Update `DbBackend` impl: `get_row_record_bytes`

```rust
// Before:
fn get_row_record_bytes<'a>(&'a self, table: &str, id: &str) -> Option<&'a [u8]> {
    self.rows.get(table)?.get(id).map(|v| v.as_slice())
}

// After:
fn get_row_record_bytes<'a>(&'a self, table: &str, id: &str) -> Option<&'a [u8]> {
    // Cache-only — same semantics as get_row_record (None on eviction).
    let cache_key = (SmolStr::new(table), SmolStr::new(id));
    self.row_cache.peek(&cache_key).map(|v| v.as_slice())
}
```

---

### Step 12 — Update `DbBackend` trait doc comment for `get_row_record_bytes`

```rust
// Before:
/// Zero-copy borrowed record access. Returns `None` if the record is absent.
///
/// Default implementation returns `None` (falls back to `get_record_bytes` for
/// backends without an in-memory row cache). Backends with an in-memory row
/// cache should override this for hot-path efficiency.
fn get_row_record_bytes<'a>(&'a self, _table: &str, _id: &str) -> Option<&'a [u8]> {
    None
}

// After:
/// Zero-copy borrowed record access. Returns `None` if the record is absent
/// **or if the record exists on disk but is not in the in-memory row cache**.
///
/// Callers must not treat `None` as proof that the record does not exist —
/// use `get_zset_weight` or `get_record_bytes` for authoritative presence checks.
///
/// Default implementation returns `None`. Backends with a row cache should
/// override this for hot-path efficiency. Fall back to `get_record_bytes` on `None`.
fn get_row_record_bytes<'a>(&'a self, _table: &str, _id: &str) -> Option<&'a [u8]> {
    None
}
```

---

## 4. Tests to Update

### Remove (tests that depended on startup pre-load)

```rust
// test_row_cache_rebuilt_on_reopen — CHANGE this test:
// Old assertion: after reopen, get_record_bytes returns cached bytes.
// New assertion: after reopen, get_record_bytes STILL works (via redb fallback).
// The test logic stays the same — only the mechanism changes.
```

Specifically in `test_row_cache_rebuilt_on_reopen`:

```rust
// After reopen, row cache must be rebuilt from redb.
let db2 = SpookyDb::new(&db_path)?;
assert_eq!(db2.get_record_bytes("users", "alice"), Some(data));  // still works (redb fallback)
// But get_row_record returns None for a cold cache:
assert!(db2.get_row_record("users", "alice").is_none());  // ← NEW assertion
```

### New tests to add

```rust
#[test]
fn test_cache_miss_falls_back_to_redb() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempfile::tempdir()?;
    let db_path = tmp_dir.path().join("test.redb");
    let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
    let (data, _) = from_cbor(&cbor)?;

    // Write a record and close the DB.
    {
        let mut db = SpookyDb::new(&db_path)?;
        db.apply_mutation("users", Operation::Create, "alice", Some(&data), None)?;
    }

    // Reopen — cache is cold but ZSet is rebuilt.
    let db2 = SpookyDb::new(&db_path)?;
    assert_eq!(db2.get_zset_weight("users", "alice"), 1);  // ZSet present

    // get_row_record returns None (cold cache).
    assert!(db2.get_row_record("users", "alice").is_none());

    // get_record_bytes falls back to redb — still returns data.
    let fetched = db2.get_record_bytes("users", "alice").expect("redb fallback must work");
    assert_eq!(fetched, data);

    Ok(())
}

#[test]
fn test_cache_eviction_correctness() -> Result<(), Box<dyn std::error::Error>> {
    // Create a SpookyDb with cache capacity 2, insert 3 records.
    // The 3rd insert evicts the 1st. Verify:
    //   - ZSet still has all 3 records.
    //   - get_record_bytes still works for the evicted record (redb fallback).
    //   - get_row_record returns None for the evicted record.
    let tmp = NamedTempFile::new()?;
    let mut db = SpookyDb::new_with_config(
        tmp.path(),
        SpookyDbConfig { cache_capacity: std::num::NonZeroUsize::new(2).unwrap() },
    )?;

    let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
    let (data, _) = from_cbor(&cbor)?;

    db.apply_mutation("t", Operation::Create, "r1", Some(&data), None)?;
    db.apply_mutation("t", Operation::Create, "r2", Some(&data), None)?;
    db.apply_mutation("t", Operation::Create, "r3", Some(&data), None)?;  // evicts r1

    // All 3 present in ZSet.
    assert_eq!(db.get_zset_weight("t", "r1"), 1);
    assert_eq!(db.get_zset_weight("t", "r2"), 1);
    assert_eq!(db.get_zset_weight("t", "r3"), 1);

    // get_record_bytes works for all 3 (redb fallback for evicted r1).
    assert!(db.get_record_bytes("t", "r1").is_some());
    assert!(db.get_record_bytes("t", "r2").is_some());
    assert!(db.get_record_bytes("t", "r3").is_some());

    // get_row_record: r1 is evicted (None), r2 and r3 are in cache (Some).
    assert!(db.get_row_record("t", "r1").is_none(), "r1 should be evicted");
    assert!(db.get_row_record("t", "r2").is_some(), "r2 should still be cached");
    assert!(db.get_row_record("t", "r3").is_some(), "r3 should be cached");

    Ok(())
}

#[test]
fn test_cache_capacity_bounds_memory() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = NamedTempFile::new()?;
    let cap = std::num::NonZeroUsize::new(5).unwrap();
    let mut db = SpookyDb::new_with_config(
        tmp.path(),
        SpookyDbConfig { cache_capacity: cap },
    )?;

    let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
    let (data, _) = from_cbor(&cbor)?;

    // Insert 10 records into a cache of capacity 5.
    for i in 0u32..10 {
        let id = format!("r{i}");
        db.apply_mutation("t", Operation::Create, &id, Some(&data), None)?;
    }

    // ZSet has all 10.
    assert_eq!(db.table_len("t"), 10);

    // Cache has at most 5.
    let cached_count = (0u32..10)
        .filter(|i| db.get_row_record("t", &format!("r{i}")).is_some())
        .count();
    assert!(cached_count <= 5, "cache exceeded capacity: {cached_count} entries cached");

    // But get_record_bytes works for all 10 (redb fallback).
    for i in 0u32..10 {
        let id = format!("r{i}");
        assert!(
            db.get_record_bytes("t", &id).is_some(),
            "redb fallback failed for r{i}"
        );
    }

    Ok(())
}

#[test]
fn test_delete_removes_from_cache() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = NamedTempFile::new()?;
    let mut db = SpookyDb::new(tmp.path())?;
    let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
    let (data, _) = from_cbor(&cbor)?;

    db.apply_mutation("t", Operation::Create, "r1", Some(&data), None)?;
    assert!(db.get_row_record("t", "r1").is_some());

    db.apply_mutation("t", Operation::Delete, "r1", None, None)?;
    // Both ZSet and cache must be gone.
    assert_eq!(db.get_zset_weight("t", "r1"), 0);
    assert!(db.get_row_record("t", "r1").is_none());
    assert!(db.get_record_bytes("t", "r1").is_none());  // ZSet guard prevents redb read

    Ok(())
}
```

---

## 5. Tests to Remove / Retire

```rust
// test_row_cache_rebuilt_on_reopen:
//   Assertion `db2.get_row_record(...).is_some()` was implicitly testing that cache
//   was pre-populated on startup. This must be changed to `is_none()`.
//   The `get_record_bytes` assertion still passes (redb fallback).

// test_row_cache_populated_on_create: Still valid — write-through still populates cache.

// test_row_cache_evicted_on_delete: Still valid — delete removes from cache.
```

---

## 6. Benchmark Strategy

Add to `benches/spooky_bench.rs`:

```rust
// Group: lru_cache
// Benchmarks to add:

// bench_cache_hit:
//   - Insert N records, then get_record_bytes for all N in a loop.
//   - Measures: LRU peek + clone cost (~50–100 ns expected).
//   - Compare against: bench_redb_read (cache miss path).

// bench_cache_miss:
//   - Insert N records, reopen DB (cold cache), then get_record_bytes for all N.
//   - Measures: ZSet guard + redb read + Vec allocation.
//   - Expected: 1–10 µs per read depending on OS page cache state.

// bench_lru_eviction:
//   - Set capacity = 100, insert 1000 records, then read all 1000.
//   - First 100 reads: redb (evicted); last 900: mix.
//   - Shows: cost of redb fallback at realistic eviction rates.

// bench_bulk_load_lru:
//   - bulk_load 10k records into a capacity-1k cache.
//   - Measures: throughput, LRU eviction overhead.
```

**Target numbers** (SSD, warm OS page cache, M-series Mac):
- Cache hit (`peek` + clone): < 100 ns
- Cache miss (redb read transaction): < 10 µs
- `apply_batch` (1000 mutations, capacity 10k): same as current (all fit in cache)

---

## 7. Implementation Order

Execute steps in this exact order. Run `cargo test` after each step.

| Step | File | Change | Tests must pass |
|---|---|---|---|
| 0 | `Cargo.toml` | Add `lru = "0.12"` | `cargo build` |
| 1 | `types.rs` | Remove `RowStore`, add `SpookyDbConfig` | `cargo build` |
| 2 | `db.rs` imports | Add `SpookyDbConfig`, remove `RowStore` | `cargo build` |
| 3 | `db.rs` struct | Replace `rows` with `row_cache` | `cargo build` (will fail — callers) |
| 4 | `db.rs` constructors | Add `new_with_config`, update `new` | continuing |
| 5 | `db.rs` `rebuild_from_records` | ZSet-only, remove rows population | continuing |
| 6 | `db.rs` `apply_mutation` | Replace `row_table.*` with `row_cache.*` | continuing |
| 7 | `db.rs` `apply_batch` | Replace `row_table.*` with `row_cache.*` | continuing |
| 8 | `db.rs` `bulk_load` | Replace `self.rows.*` with `row_cache.*` | `cargo test` — all pass |
| 9 | `db.rs` `get_record_bytes` | Add redb fallback | `cargo test` — all pass |
| 10 | `db.rs` `get_row_record` | ZSet guard + `peek` + doc update | `cargo test` — all pass |
| 11 | `db.rs` `get_row_record_bytes` impl | Use `peek` | `cargo test` — all pass |
| 12 | `db.rs` tests | Update `test_row_cache_rebuilt_on_reopen` | `cargo test` — all pass |
| 13 | `db.rs` tests | Add 4 new tests (§4) | `cargo test` — all pass + 4 new |
| 14 | `CLAUDE.md` | Update Layer 4 and Technical Debt | N/A |

---

## 8. DbBackend Trait: No Signature Changes Needed

- `get_record_bytes(&self, ...) -> Option<Vec<u8>>`: unchanged — redb fallback is an implementation detail
- `get_row_record_bytes<'a>(&'a self, ...) -> Option<&'a [u8]>`: unchanged signature — semantics update only (can return `None` for cache miss)
- All write method signatures unchanged

The trait remains object-safe. `Box<dyn DbBackend>` continues to compile.

---

## 9. Optional Enhancement: Read-Through Cache

After the mechanical change above is in and tests pass, optionally add a
**read-through** variant that populates the cache on miss. This requires `&mut self`.

```rust
/// Read-through: fetch bytes and populate cache on miss.
///
/// Use this on bulk read paths where you want the cache to warm from reads,
/// not just writes. ~10 µs on first call (redb), ~50 ns thereafter (cache).
pub fn get_record_bytes_cached(
    &mut self,
    table: &str,
    id: &str,
) -> Option<Vec<u8>> {
    // ZSet guard.
    let present = self
        .zsets
        .get(table)
        .and_then(|z| z.get(id))
        .copied()
        .unwrap_or(0)
        > 0;
    if !present {
        return None;
    }

    // Cache hit (updates recency).
    let cache_key = (SmolStr::new(table), SmolStr::new(id));
    if self.row_cache.contains(&cache_key) {
        return self.row_cache.get(&cache_key).cloned();
    }

    // Cache miss — read from redb and populate cache.
    let db_key = make_key(table, id);
    let bytes = {
        let read_txn = self.db.begin_read().ok()?;
        let tbl = read_txn.open_table(RECORDS_TABLE).ok()?;
        tbl.get(db_key.as_str())
            .ok()?
            .map(|guard| guard.value().to_vec())?
    };

    self.row_cache.put(cache_key, bytes.clone());
    Some(bytes)
}
```

Add to `DbBackend` trait with a default implementation that calls `get_record_bytes`.
This is backward-compatible — existing impls get the non-caching version automatically.

---

## 10. Alternative: `quick-cache` (Performance Upgrade)

If Criterion benchmarks show the `lru` crate is a bottleneck under high write
throughput, consider `quick-cache = "0.6"` as a drop-in replacement:

- Uses a concurrent shard design (faster under `&mut self` writes)
- Similar API (`LruCache` → `Cache`)
- `peek()` available for `&self` access
- Slightly different eviction semantics (S3-FIFO vs LRU)

Only switch after benchmarking — `lru` 0.12 is well-optimized for single-threaded use.

---

## 11. Summary of Semantic Changes

| Behaviour | Before (full cache) | After (LRU) |
|---|---|---|
| Startup memory | O(N × record_bytes) | O(N × key_size) only |
| Cold read after startup | Memory — zero I/O | redb I/O — ~1–10 µs |
| Hot read (recently written) | Memory — zero I/O | LRU `peek` — zero I/O |
| `get_row_record` returns `None` | Only if record deleted | Also if evicted from cache |
| `get_record_bytes` on evicted | Memory — zero I/O | redb I/O — ~1–10 µs |
| Max memory (record bytes) | Unbounded | `capacity × avg_record_size` |
| `DbBackend` trait changes | — | None (signatures unchanged) |
| Test count | 133 | 133 + 4 new = 137 |
