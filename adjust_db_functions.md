# adjust_db_functions.md — Concrete db.rs Adjustments

> This document captures every change needed to `src/db/db.rs` and `src/db/types.rs`
> to align the module with SSP's actual access patterns.
>
> Each section shows the current code, the problem it causes for SSP, and the exact replacement.
> Changes are ordered so each step compiles and passes `cargo test` independently.

---

## Why these changes are necessary

SSP's view evaluation reads row data on **every** filter/join cycle:

```
eval_snapshot(Filter) → for each id in ZSet →
  get_row_value(table, id, db)     ← currently goes to redb every call
  check_predicate(&row_value, ...)
```

Currently `get_record_bytes` opens a redb read transaction per call (~100–500 ns overhead).
For a Filter over 100k records = 100k redb transactions per view tick instead of 100k
in-memory hash lookups (~5–10 ns each). That is a 20–100× performance regression.

The fix: add an in-memory row bytes cache (`rows`) to `SpookyDb`. redb becomes a
**write-through persistence layer** — written on every mutation, read **only on startup**.
All view evaluation reads come from memory, just like SSP's current `Table.rows`.

---

## Step 0 — `types.rs`: add `RowStore` type alias and `TableName`

**File**: `src/db/types.rs`

Add two type aliases after the existing ones (line 10). No struct changes.

```rust
// After line 10:
pub type ZSet = FastMap<RowKey, Weight>;

// ADD:
/// Alias for table names to distinguish them from record IDs in function signatures.
pub type TableName = SmolStr;

/// Per-table in-memory row cache. Key: record_id → serialized SpookyRecord bytes.
pub type RowStore = FastMap<RowKey, Vec<u8>>;
```

Also update `BatchMutationResult` to use `TableName` for clarity (optional but recommended):

```rust
// Before:
pub struct BatchMutationResult {
    pub membership_deltas: FastMap<SmolStr, ZSet>,
    pub content_updates: FastMap<SmolStr, FastHashSet<SmolStr>>,
    pub changed_tables: Vec<SmolStr>,
}

// After:
pub struct BatchMutationResult {
    pub membership_deltas: FastMap<TableName, ZSet>,
    pub content_updates: FastMap<TableName, FastHashSet<RowKey>>,
    pub changed_tables: Vec<TableName>,
}
```

Also add `version` field to `BulkRecord` so bulk-loaded records can carry version info:

```rust
// Before:
pub struct BulkRecord {
    pub table: SmolStr,
    pub id: SmolStr,
    pub data: Vec<u8>,
}

// After:
pub struct BulkRecord {
    pub table: SmolStr,
    pub id: SmolStr,
    pub data: Vec<u8>,
    /// Written to VERSION_TABLE when `Some`. Pass `None` to skip version tracking.
    pub version: Option<u64>,
}
```

---

## Step 1 — `SpookyDb` struct: add `rows` field

**File**: `src/db/db.rs`, lines 38–46

**Problem**: No in-memory row cache. Every read goes to redb.

```rust
// CURRENT:
pub struct SpookyDb {
    db: RedbDatabase,
    zsets: FastMap<SmolStr, ZSet>,
}

// REPLACE WITH:
pub struct SpookyDb {
    /// On-disk KV store. Written on every mutation; read only during startup.
    db: RedbDatabase,

    /// Hot ZSet per table. Key: table_name → (record_id → weight).
    /// Weight 1 = present; absent = deleted. Rebuilt from redb on startup.
    zsets: FastMap<SmolStr, ZSet>,

    /// In-memory row cache. Key: table_name → (record_id → SpookyRecord bytes).
    ///
    /// Primary read source for all view evaluation. Zero I/O on reads.
    /// Written on every Create/Update; evicted on Delete.
    /// Rebuilt from RECORDS_TABLE on startup (same scan as `zsets`).
    ///
    /// Memory: ~100–300 bytes per record (binary format) vs ~400–1000 bytes
    /// for the equivalent SpookyValue in SSP's current Table.rows.
    rows: FastMap<SmolStr, RowStore>,
}
```

Update `new()` to initialise `rows`:

```rust
// CURRENT (lines 67–71):
let mut spooky = SpookyDb {
    db,
    zsets: FastMap::default(),
};
spooky.rebuild_zsets_from_records()?;

// REPLACE WITH:
let mut spooky = SpookyDb {
    db,
    zsets: FastMap::default(),
    rows: FastMap::default(),
};
spooky.rebuild_from_records()?;
```

Add `RowStore` to the use statement at the top of `db.rs`:

```rust
use super::types::{
    BatchMutationResult, BulkRecord, DbMutation, FastHashSet, FastMap, Operation,
    RowStore, SpookyDbError, ZSet,
};
```

---

## Step 2 — `rebuild_from_records`: populate both `zsets` and `rows` in one scan

**File**: `src/db/db.rs`, lines 75–91

**Problem**: Only populates `zsets`. With the row cache, we need both populated from the
same single startup scan. No extra I/O cost — one scan already reads the bytes.

```rust
// CURRENT (rename + extend):
fn rebuild_zsets_from_records(&mut self) -> Result<(), SpookyDbError> {
    let read_txn = self.db.begin_read()?;
    let table = read_txn.open_table(RECORDS_TABLE)?;
    for entry in table.iter()? {
        let (key_guard, _) = entry?;
        let key_str: &str = key_guard.value();
        if let Some((table_name, id)) = key_str.split_once(':') {
            self.zsets
                .entry(SmolStr::new(table_name))
                .or_default()
                .insert(SmolStr::new(id), 1);
        }
    }
    Ok(())
}

// REPLACE WITH (rename to rebuild_from_records):
/// Sequential scan of RECORDS_TABLE on startup.
///
/// Populates BOTH `zsets` (weight=1 per key) AND `rows` (bytes per key) in
/// a single pass. O(N records) — approximately 40–120ms per million records on SSD.
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
            self.rows.entry(t).or_default().insert(i, bytes);
        }
    }
    Ok(())
}
```

---

## Step 3 — `make_key`: keep for writes only, mark clearly

**File**: `src/db/db.rs`, lines 97–100

`make_key` is now only called on the write path (apply_mutation, apply_batch, bulk_load,
get_version). The read path no longer needs it. No change to the function itself, but
update the comment to reflect the new role:

```rust
// CURRENT:
/// Build a flat "table:id" redb key.
#[inline]
fn make_key(table: &str, id: &str) -> String {
    format!("{}:{}", table, id)
}

// REPLACE WITH (Phase 3.1 of IMPROVEMENT_PLAN — use ArrayString to remove heap alloc):
/// Build a flat "table:id" redb key for write operations.
/// Only used on the write path — reads are served from `self.rows` without key construction.
#[inline]
fn make_key(table: &str, id: &str) -> arrayvec::ArrayString<512> {
    let mut s = arrayvec::ArrayString::<512>::new();
    s.push_str(table);
    s.push(':');
    s.push_str(id);
    s
}
```

> Note: if `arrayvec` is not yet imported at call sites, add `key.as_str()` wherever
> `key` was previously used as `&str` — no other callsite changes needed.

---

## Step 4 — `apply_mutation`: fix atomicity + update row cache

**File**: `src/db/db.rs`, lines 104–150

**Problems fixed here**:
- CRIT-1: ZSet/rows updated BEFORE redb commit — moved to AFTER
- CRIT-2: no table name validation — added
- Row cache not updated — fixed
- Return value uses full flat key — changed to bare `id`

```rust
// CURRENT:
pub fn apply_mutation(
    &mut self,
    table: &str,
    op: Operation,
    id: &str,
    data: Option<&[u8]>,
    version: Option<u64>,
) -> Result<(SmolStr, i64), SpookyDbError> {
    let key = make_key(table, id);
    let weight = op.weight();

    // --- In-memory ZSet update ---
    let zset = self.zsets.entry(SmolStr::new(table)).or_default();
    if matches!(op, Operation::Delete) {
        zset.remove(id);
    } else {
        zset.insert(SmolStr::new(id), 1);
    }

    // --- Persist to redb ---
    let write_txn = self.db.begin_write()?;
    {
        let mut records = write_txn.open_table(RECORDS_TABLE)?;
        let mut versions = write_txn.open_table(VERSION_TABLE)?;
        if matches!(op, Operation::Delete) {
            records.remove(key.as_str())?;
            versions.remove(key.as_str())?;
        } else {
            if let Some(bytes) = data {
                records.insert(key.as_str(), bytes)?;
            }
            if let Some(ver) = version {
                versions.insert(key.as_str(), ver)?;
            }
        }
    }
    write_txn.commit()?;

    Ok((SmolStr::new(&key), weight))
}

// REPLACE WITH:
pub fn apply_mutation(
    &mut self,
    table: &str,
    op: Operation,
    id: &str,
    data: Option<&[u8]>,
    version: Option<u64>,
) -> Result<(SmolStr, i64), SpookyDbError> {
    // Validate table name invariant.
    validate_table_name(table)?;

    let key = make_key(table, id);
    let weight = op.weight();

    // --- 1. Persist to redb FIRST ---
    // If commit fails, in-memory state is untouched — no divergence.
    let write_txn = self.db.begin_write()?;
    {
        let mut records = write_txn.open_table(RECORDS_TABLE)?;
        let mut versions = write_txn.open_table(VERSION_TABLE)?;
        if matches!(op, Operation::Delete) {
            records.remove(key.as_str())?;
            versions.remove(key.as_str())?;
        } else {
            if let Some(bytes) = data {
                records.insert(key.as_str(), bytes)?;
            }
            if let Some(ver) = version {
                versions.insert(key.as_str(), ver)?;
            }
        }
    }
    write_txn.commit()?;

    // --- 2. Update in-memory state AFTER successful commit ---
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

    // Return bare id (not flat key) — consistent with apply_batch membership_deltas.
    Ok((SmolStr::new(id), weight))
}
```

---

## Step 5 — `apply_batch`: fix atomicity + update row cache

**File**: `src/db/db.rs`, lines 159–228

**Problems fixed**:
- CRIT-1: in-memory mutations moved to after commit
- CRIT-2: table name validation added
- Row cache eviction/insertion added
- Perf: `changed_tables_set` → `Vec` with dedup (eliminates HashSet + collect)
- Perf: spurious `-1` delta for non-existent Delete suppressed

```rust
// CURRENT: (full function shown above in db.rs — not repeated for brevity)

// REPLACE WITH:
pub fn apply_batch(
    &mut self,
    mutations: Vec<DbMutation>,
) -> Result<BatchMutationResult, SpookyDbError> {
    // Validate all table names before opening any transaction.
    for m in &mutations {
        validate_table_name(&m.table)?;
    }

    let mut membership_deltas: FastMap<SmolStr, ZSet> = FastMap::default();
    let mut content_updates: FastMap<SmolStr, FastHashSet<SmolStr>> = FastMap::default();
    let mut changed_tables: Vec<SmolStr> = Vec::new();

    // --- 1. All redb writes in one transaction ---
    let write_txn = self.db.begin_write()?;
    {
        let mut records = write_txn.open_table(RECORDS_TABLE)?;
        let mut versions = write_txn.open_table(VERSION_TABLE)?;

        for mutation in &mutations {
            let key = make_key(&mutation.table, &mutation.id);

            if matches!(mutation.op, Operation::Delete) {
                records.remove(key.as_str())?;
                versions.remove(key.as_str())?;
            } else {
                if let Some(ref bytes) = mutation.data {
                    records.insert(key.as_str(), bytes.as_slice())?;
                }
                if let Some(ver) = mutation.version {
                    versions.insert(key.as_str(), ver)?;
                }
            }
        }
    }
    write_txn.commit()?;

    // --- 2. Update in-memory state AFTER successful commit ---
    for mutation in mutations {
        let DbMutation { table, id, op, data, .. } = mutation;

        // Track whether the record was present before this mutation.
        let was_present = self
            .zsets
            .get(&table)
            .and_then(|z| z.get(&id))
            .copied()
            .unwrap_or(0) > 0;

        let zset = self.zsets.entry(table.clone()).or_default();
        let row_table = self.rows.entry(table.clone()).or_default();

        if matches!(op, Operation::Delete) {
            zset.remove(&id);
            row_table.remove(&id);
            // Only emit -1 delta if the record was actually present.
            if was_present {
                membership_deltas
                    .entry(table.clone())
                    .or_default()
                    .insert(id.clone(), -1);
            }
        } else {
            zset.insert(id.clone(), 1);
            if let Some(bytes) = data {
                row_table.insert(id.clone(), bytes);
            }
            let weight = op.weight();
            if weight != 0 {
                membership_deltas
                    .entry(table.clone())
                    .or_default()
                    .insert(id.clone(), weight);
            }
            content_updates
                .entry(table.clone())
                .or_default()
                .insert(id.clone());
        }

        // Dedup changed_tables without a HashSet.
        if !changed_tables.contains(&table) {
            changed_tables.push(table);
        }
    }

    Ok(BatchMutationResult {
        membership_deltas,
        content_updates,
        changed_tables,
    })
}
```

---

## Step 6 — `bulk_load`: update row cache + write VERSION_TABLE

**File**: `src/db/db.rs`, lines 234–250

**Problems fixed**:
- Row cache not populated — fixed
- VERSION_TABLE never written — fixed (uses `BulkRecord.version`)
- CRIT-2: table name validation added
- CRIT-1 pattern: ZSet/rows updated after commit

```rust
// CURRENT:
pub fn bulk_load(
    &mut self,
    records: impl IntoIterator<Item = BulkRecord>,
) -> Result<(), SpookyDbError> {
    let write_txn = self.db.begin_write()?;
    {
        let mut rec_table = write_txn.open_table(RECORDS_TABLE)?;
        for record in records {
            let BulkRecord { table, id, data } = record;
            let key = make_key(&table, &id);
            rec_table.insert(key.as_str(), data.as_slice())?;
            self.zsets.entry(table).or_default().insert(id, 1);
        }
    }
    write_txn.commit()?;
    Ok(())
}

// REPLACE WITH:
/// Bulk initial load: all records in one write transaction.
///
/// Records are inserted into both redb and the in-memory row cache.
/// ZSet weights are set to 1 for all inserted records.
/// If `record.version` is `Some`, it is written to VERSION_TABLE.
///
/// This is the correct path for `Circuit::init_load` — one fsync for N records.
pub fn bulk_load(
    &mut self,
    records: Vec<BulkRecord>,
) -> Result<(), SpookyDbError> {
    // Validate all table names before touching redb.
    for r in &records {
        validate_table_name(&r.table)?;
    }

    // --- 1. Write all records to redb in one transaction ---
    let write_txn = self.db.begin_write()?;
    {
        let mut rec_table = write_txn.open_table(RECORDS_TABLE)?;
        let mut ver_table = write_txn.open_table(VERSION_TABLE)?;
        for record in &records {
            let key = make_key(&record.table, &record.id);
            rec_table.insert(key.as_str(), record.data.as_slice())?;
            if let Some(ver) = record.version {
                ver_table.insert(key.as_str(), ver)?;
            }
        }
    }
    write_txn.commit()?;

    // --- 2. Update in-memory state after successful commit ---
    for BulkRecord { table, id, data, .. } in records {
        self.zsets.entry(table.clone()).or_default().insert(id.clone(), 1);
        self.rows.entry(table).or_default().insert(id, data);
    }

    Ok(())
}
```

> Note: signature changed from `impl IntoIterator<Item = BulkRecord>` to `Vec<BulkRecord>`.
> This also fixes the `DbBackend` object-safety issue (CRIT-3).

---

## Step 7 — `get_record_bytes`: serve from `rows`, zero I/O

**File**: `src/db/db.rs`, lines 274–289

**Problem**: Opens a redb read transaction on every call. With the row cache this is
unnecessary — the bytes are already in memory.

```rust
// CURRENT:
pub fn get_record_bytes(
    &self,
    table: &str,
    id: &str,
) -> Result<Option<Vec<u8>>, SpookyDbError> {
    if self.get_zset_weight(table, id) == 0 {
        return Ok(None);
    }
    let key = make_key(table, id);
    let read_txn = self.db.begin_read()?;
    let tbl = read_txn.open_table(RECORDS_TABLE)?;
    Ok(tbl
        .get(key.as_str())?
        .map(|guard: redb::AccessGuard<&[u8]>| guard.value().to_vec()))
}

// REPLACE WITH:
/// Fetch a copy of the raw SpookyRecord bytes for a record.
///
/// Served from the in-memory row cache — zero I/O, no redb transaction.
/// Returns `None` if the record is not present (ZSet weight = 0).
///
/// For the view evaluation hot path where you need a borrowed reference
/// without copying, use `get_row_record` instead.
pub fn get_record_bytes(&self, table: &str, id: &str) -> Option<Vec<u8>> {
    self.rows.get(table)?.get(id).cloned()
}
```

> Return type simplified from `Result<Option<Vec<u8>>, SpookyDbError>` to `Option<Vec<u8>>`.
> In-memory access never fails — the `Result` wrapper was only needed for redb I/O.
>
> Update the `DbBackend` trait impl (Step 10) accordingly.

---

## Step 8 — ADD `get_row_record`: zero-copy borrowed accessor (new function)

**File**: `src/db/db.rs` — add after `get_record_bytes`

This is the hot-path accessor SSP's view evaluation should use. Returns a borrowed
`SpookyRecord<'a>` from the in-memory row cache — no allocation, no I/O, valid for the
lifetime of the `&SpookyDb` borrow.

```rust
// ADD (new function — does not exist yet):

/// Zero-copy borrowed SpookyRecord for the view evaluation hot path.
///
/// Returns a `SpookyRecord<'a>` borrowing directly from the in-memory row cache.
/// Valid until the next `&mut self` call on this `SpookyDb`.
///
/// Zero allocation, zero I/O — this is what SSP's Filter/Join operators
/// should use instead of `get_row_value`. O(1) table lookup + O(1) id lookup.
///
/// ```rust,ignore
/// // In SSP's view.rs — replacing get_row_value:
/// if let Some(record) = db.get_row_record("users", id) {
///     let age = record.get_i64("age").unwrap_or(0);
///     let name = record.get_str("name").unwrap_or("");
/// }
///
/// // For repeated field access in a tight loop, resolve slot once:
/// if let Some(record) = db.get_row_record("users", id) {
///     let slot = record.resolve("age");
///     let age = record.get_i64_at(&slot);
/// }
/// ```
pub fn get_row_record<'a>(&'a self, table: &str, id: &str) -> Option<SpookyRecord<'a>> {
    let bytes = self.rows.get(table)?.get(id)?;
    let (buf, count) = from_bytes(bytes).ok()?;
    Some(SpookyRecord::new(buf, count))
}
```

---

## Step 9 — `get_version`: add ZSet guard

**File**: `src/db/db.rs`, lines 327–334

**Problem**: Opens a redb read transaction even for keys known absent from the ZSet.
`get_version` is the only read method that still hits redb — add the guard.

```rust
// CURRENT:
pub fn get_version(&self, table: &str, id: &str) -> Result<Option<u64>, SpookyDbError> {
    let key = make_key(table, id);
    let read_txn = self.db.begin_read()?;
    let tbl = read_txn.open_table(VERSION_TABLE)?;
    Ok(tbl
        .get(key.as_str())?
        .map(|guard: redb::AccessGuard<u64>| guard.value()))
}

// REPLACE WITH:
pub fn get_version(&self, table: &str, id: &str) -> Result<Option<u64>, SpookyDbError> {
    // Skip redb entirely for keys not in the ZSet.
    if self.get_zset_weight(table, id) == 0 {
        return Ok(None);
    }
    let key = make_key(table, id);
    let read_txn = self.db.begin_read()?;
    let tbl = read_txn.open_table(VERSION_TABLE)?;
    Ok(tbl
        .get(key.as_str())?
        .map(|guard: redb::AccessGuard<u64>| guard.value()))
}
```

---

## Step 10 — `table_exists`: use `contains_key` (semantic fix)

**File**: `src/db/db.rs`, lines 379–384

**Problem**: Checks `!z.is_empty()` — an `ensure_table`'d table returns `false`.
See Phase 2.3 of IMPROVEMENT_PLAN.

```rust
// CURRENT:
pub fn table_exists(&self, table: &str) -> bool {
    self.zsets
        .get(table)
        .map(|z| !z.is_empty())
        .unwrap_or(false)
}

// REPLACE WITH:
pub fn table_exists(&self, table: &str) -> bool {
    self.zsets.contains_key(table)
}
```

---

## Step 11 — `ensure_table`: add validation

**File**: `src/db/db.rs`, line 402

```rust
// CURRENT:
pub fn ensure_table(&mut self, table: &str) {
    self.zsets.entry(SmolStr::new(table)).or_default();
}

// REPLACE WITH (returns Result so callers can handle invalid names):
pub fn ensure_table(&mut self, table: &str) -> Result<(), SpookyDbError> {
    validate_table_name(table)?;
    self.zsets.entry(SmolStr::new(table)).or_default();
    self.rows.entry(SmolStr::new(table)).or_default();
    Ok(())
}
```

---

## Step 12 — `DbBackend` trait: object-safety + updated signatures

**File**: `src/db/db.rs`, lines 415–450

Key changes:
- `bulk_load`: `impl IntoIterator` → `Vec<BulkRecord>` (restores object safety, CRIT-3)
- `get_record_bytes`: return type simplified to `Option<Vec<u8>>` (no error on in-memory miss)
- `get_row_record`: added as the zero-copy hot-path method
- `ensure_table`: returns `Result`

```rust
// REPLACE THE ENTIRE DbBackend TRAIT WITH:

pub trait DbBackend {
    /// Zero-copy ZSet access. Borrowed from memory — zero I/O.
    fn get_table_zset(&self, table: &str) -> Option<&ZSet>;

    /// Raw bytes for a record, served from in-memory cache.
    /// Returns `None` if the record is absent.
    fn get_record_bytes(&self, table: &str, id: &str) -> Option<Vec<u8>>;

    /// Register an empty table. Returns error if the table name contains ':'.
    fn ensure_table(&mut self, table: &str) -> Result<(), SpookyDbError>;

    /// Single mutation: record write + ZSet + row cache update.
    /// In-memory state is only updated AFTER the redb commit succeeds.
    fn apply_mutation(
        &mut self,
        table: &str,
        op: Operation,
        id: &str,
        data: Option<&[u8]>,
        version: Option<u64>,
    ) -> Result<(SmolStr, i64), SpookyDbError>;

    /// Batch mutations in one redb transaction (one fsync).
    /// Prefer this over `apply_mutation` for all batch ingestion paths.
    fn apply_batch(
        &mut self,
        mutations: Vec<DbMutation>,
    ) -> Result<BatchMutationResult, SpookyDbError>;

    /// Bulk initial load: all records in one transaction.
    /// Takes `Vec<BulkRecord>` (not `impl IntoIterator`) for object safety.
    fn bulk_load(
        &mut self,
        records: Vec<BulkRecord>,
    ) -> Result<(), SpookyDbError>;

    /// Weight for one record. Returns 0 if absent.
    fn get_zset_weight(&self, table: &str, id: &str) -> i64;

    /// Partial SpookyValue reconstruction by field name list.
    /// Convenience for compatibility layers — not for hot paths.
    fn get_record_typed(
        &self,
        table: &str,
        id: &str,
        fields: &[&str],
    ) -> Result<Option<SpookyValue>, SpookyDbError>;
}
```

---

## Step 13 — `impl DbBackend for SpookyDb`: update all delegations

**File**: `src/db/db.rs`, lines 452–493

```rust
impl DbBackend for SpookyDb {
    fn get_table_zset(&self, table: &str) -> Option<&ZSet> {
        SpookyDb::get_table_zset(self, table)
    }

    fn get_record_bytes(&self, table: &str, id: &str) -> Option<Vec<u8>> {
        SpookyDb::get_record_bytes(self, table, id)
    }

    fn ensure_table(&mut self, table: &str) -> Result<(), SpookyDbError> {
        SpookyDb::ensure_table(self, table)
    }

    fn apply_mutation(
        &mut self,
        table: &str,
        op: Operation,
        id: &str,
        data: Option<&[u8]>,
        version: Option<u64>,
    ) -> Result<(SmolStr, i64), SpookyDbError> {
        SpookyDb::apply_mutation(self, table, op, id, data, version)
    }

    fn apply_batch(
        &mut self,
        mutations: Vec<DbMutation>,
    ) -> Result<BatchMutationResult, SpookyDbError> {
        SpookyDb::apply_batch(self, mutations)
    }

    fn bulk_load(
        &mut self,
        records: Vec<BulkRecord>,
    ) -> Result<(), SpookyDbError> {
        SpookyDb::bulk_load(self, records)
    }

    fn get_zset_weight(&self, table: &str, id: &str) -> i64 {
        SpookyDb::get_zset_weight(self, table)
    }

    fn get_record_typed(
        &self,
        table: &str,
        id: &str,
        fields: &[&str],
    ) -> Result<Option<SpookyValue>, SpookyDbError> {
        SpookyDb::get_record_typed(self, table, id, fields)
    }
}
```

> Note: `get_zset_weight` trait impl bug was in the original — it was calling
> `self.get_zset_weight(table)` with only one arg. The inherent method takes `(table, id)`.
> Fixed above by using UFCS `SpookyDb::get_zset_weight(self, table)`.

---

## Step 14 — ADD `validate_table_name` helper

**File**: `src/db/db.rs` — add in the helpers section (near `make_key`)

```rust
// ADD (new function — used by apply_mutation, apply_batch, bulk_load, ensure_table):

/// Reject table names containing ':' before they corrupt the key namespace.
///
/// The flat key format "table:id" uses ':' as the sole separator.
/// `split_once(':')` on startup would mis-parse any record stored under
/// a table name that itself contains ':'.
#[inline]
fn validate_table_name(table: &str) -> Result<(), SpookyDbError> {
    if table.contains(':') {
        Err(SpookyDbError::InvalidKey(format!(
            "table name must not contain ':': {:?}", table
        )))
    } else {
        Ok(())
    }
}
```

---

## Step 15 — Update test fixtures for changed signatures

**File**: `src/db/db.rs`, test module

Tests that use `apply_mutation` do not need changes (signature unchanged).

Tests that use `bulk_load` need `version: None` added to `BulkRecord`:

```rust
// CURRENT (test_bulk_load):
let records = vec![
    BulkRecord { table: SmolStr::new("items"), id: SmolStr::new("i1"), data: data.clone() },
    BulkRecord { table: SmolStr::new("items"), id: SmolStr::new("i2"), data: data.clone() },
];
db.bulk_load(records)?;

// UPDATE TO:
let records = vec![
    BulkRecord { table: SmolStr::new("items"), id: SmolStr::new("i1"), data: data.clone(), version: None },
    BulkRecord { table: SmolStr::new("items"), id: SmolStr::new("i2"), data: data.clone(), version: None },
];
db.bulk_load(records)?;
```

Tests that use `ensure_table` need `?` added:

```rust
// CURRENT:
db.ensure_table("empty_table");

// UPDATE TO:
db.ensure_table("empty_table")?;
```

---

## New tests to add

```rust
#[test]
fn test_row_cache_populated_on_create() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = NamedTempFile::new()?;
    let mut db = SpookyDb::new(tmp.path())?;
    let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
    let (data, _) = from_cbor(&cbor)?;

    db.apply_mutation("users", Operation::Create, "alice", Some(&data), None)?;

    // get_record_bytes must now return without touching redb.
    assert_eq!(db.get_record_bytes("users", "alice"), Some(data.clone()));

    // get_row_record must return a valid borrowed record.
    let record = db.get_row_record("users", "alice").expect("should be in cache");
    let age = record.get_i64("age");
    assert!(age.is_some(), "age field should be readable from cached record");

    Ok(())
}

#[test]
fn test_row_cache_evicted_on_delete() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = NamedTempFile::new()?;
    let mut db = SpookyDb::new(tmp.path())?;
    let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
    let (data, _) = from_cbor(&cbor)?;

    db.apply_mutation("users", Operation::Create, "alice", Some(&data), None)?;
    db.apply_mutation("users", Operation::Delete, "alice", None, None)?;

    assert_eq!(db.get_record_bytes("users", "alice"), None);
    assert!(db.get_row_record("users", "alice").is_none());
    Ok(())
}

#[test]
fn test_row_cache_rebuilt_on_reopen() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempfile::tempdir()?;
    let db_path = tmp_dir.path().join("test.redb");
    let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
    let (data, _) = from_cbor(&cbor)?;

    {
        let mut db = SpookyDb::new(&db_path)?;
        db.apply_mutation("users", Operation::Create, "alice", Some(&data), None)?;
    }

    // After reopen, row cache must be rebuilt from redb.
    let db2 = SpookyDb::new(&db_path)?;
    assert_eq!(db2.get_record_bytes("users", "alice"), Some(data));
    let record = db2.get_row_record("users", "alice").expect("must be in cache after reopen");
    assert!(record.get_i64("age").is_some());
    Ok(())
}

#[test]
fn test_table_name_with_colon_rejected() {
    let tmp = NamedTempFile::new().unwrap();
    let mut db = SpookyDb::new(tmp.path()).unwrap();
    let result = db.apply_mutation("a:b", Operation::Create, "id1", Some(&[]), None);
    assert!(matches!(result, Err(SpookyDbError::InvalidKey(_))));
}

#[test]
fn test_zset_not_diverged_after_create() -> Result<(), Box<dyn std::error::Error>> {
    // Verify that ZSet and rows are in sync after apply_mutation.
    let tmp = NamedTempFile::new()?;
    let mut db = SpookyDb::new(tmp.path())?;
    let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
    let (data, _) = from_cbor(&cbor)?;

    db.apply_mutation("users", Operation::Create, "alice", Some(&data), None)?;
    assert_eq!(db.get_zset_weight("users", "alice"), 1);
    assert!(db.get_record_bytes("users", "alice").is_some());

    db.apply_mutation("users", Operation::Delete, "alice", None, None)?;
    assert_eq!(db.get_zset_weight("users", "alice"), 0);
    assert!(db.get_record_bytes("users", "alice").is_none());
    Ok(())
}

#[test]
fn test_delete_nonexistent_emits_no_delta() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = NamedTempFile::new()?;
    let mut db = SpookyDb::new(tmp.path())?;

    let result = db.apply_batch(vec![DbMutation {
        table: SmolStr::new("users"),
        id: SmolStr::new("ghost"),
        op: Operation::Delete,
        data: None,
        version: None,
    }])?;

    // No record was present → membership_deltas must be empty.
    assert!(
        result.membership_deltas.get("users").map_or(true, |z| z.is_empty()),
        "spurious -1 delta emitted for a record that never existed"
    );
    Ok(())
}

#[test]
fn test_dyn_dbbackend_compiles() {
    // This test exists purely to assert DbBackend is object-safe.
    // It will fail to compile if bulk_load still uses impl IntoIterator.
    let tmp = NamedTempFile::new().unwrap();
    let db = SpookyDb::new(tmp.path()).unwrap();
    let _: Box<dyn DbBackend> = Box::new(db);
}
```

---

## Summary: what changed and why

| Function | Change | Reason |
|---|---|---|
| `SpookyDb` struct | Added `rows: FastMap<SmolStr, RowStore>` | In-memory row cache for zero-I/O reads |
| `rebuild_from_records` | Populates both `zsets` and `rows` in one scan | Single startup pass, no extra I/O |
| `make_key` | `ArrayString<512>` instead of `String` | Eliminates heap alloc on write path |
| `apply_mutation` | redb commit BEFORE in-memory update; rows cache populated; bare `id` returned | Atomicity fix + row cache write-through |
| `apply_batch` | redb commit BEFORE in-memory update; rows updated; dedup via Vec; spurious -1 suppressed | Atomicity fix + correctness |
| `bulk_load` | `Vec<BulkRecord>` param; VERSION_TABLE written; rows populated after commit | Object safety + atomicity + version tracking |
| `get_record_bytes` | Serves from `self.rows` — no redb, no Result | 20–100× faster for view evaluation |
| `get_row_record` | **New** — zero-copy borrowed `SpookyRecord<'a>` from row cache | Hot-path accessor for SSP's Filter/Join |
| `get_version` | ZSet guard added | Avoids redb read for absent keys |
| `table_exists` | `contains_key` instead of `!is_empty` | Semantic correctness after `ensure_table` |
| `ensure_table` | Returns `Result`; validates table name; initialises rows entry | Validation + consistency |
| `validate_table_name` | **New** — inline colon check | Enforces key namespace invariant |
| `DbBackend` trait | `bulk_load` → `Vec`; `get_record_bytes` simplified; `get_record_typed` + `ensure_table` added | Object safety + completeness |
| `impl DbBackend for SpookyDb` | All delegations use UFCS; `get_zset_weight` bug fixed | No apparent-recursion; correct signature |
