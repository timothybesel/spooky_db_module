use std::path::Path;

use arrayvec::ArrayString;
use redb::{Database as RedbDatabase, ReadableDatabase, ReadableTable, TableDefinition};
use smol_str::SmolStr;

use super::types::{
    BatchMutationResult, BulkRecord, DbMutation, FastHashSet, FastMap, Operation,
    SpookyDbConfig, SpookyDbError, ZSet,
};
use crate::serialization::from_bytes;
use crate::spooky_record::{SpookyReadable, SpookyRecord};
use crate::spooky_value::SpookyValue;

// ─── Table definitions ───────────────────────────────────────────────────────
//
// Flat string key: "table_name:record_id"
// INVARIANT: table names must not contain ':'. IDs may contain ':'; only the
// first ':' in the key is the separator (split_once(':') is used everywhere).

/// Primary record store. Key: "table:id" → Value: serialized SpookyRecord bytes.
const RECORDS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("records");

/// Version store for sync / conflict detection.
/// Key: "table:id" → Value: version u64 (read from the "spooky_rv" field or explicit).
const VERSION_TABLE: TableDefinition<&str, u64> = TableDefinition::new("versions");

// ─── SpookyDb ─────────────────────────────────────────────────────────────────

/// Persistent record store backed by redb.
///
/// **Ownership**: `SpookyDb` is meant to be owned exclusively by one component
/// (e.g. a streaming data processor). No `Arc`, no `Mutex` — callers hold `&mut self`
/// for write operations.
///
/// **ZSet**: a per-table in-memory `FastMap<record_id, weight>` that shadows
/// RECORDS_TABLE. Rebuilt from a sequential RECORDS_TABLE scan on startup.
/// All view-evaluation ZSet reads are pure memory — zero I/O.
pub struct SpookyDb {
    /// On-disk KV store. Written on every mutation; read only during startup.
    db: RedbDatabase,

    /// Hot ZSet per table. Key: table name → Value: (record_id → weight).
    /// INVARIANT: table names must not contain ':'.
    /// Weight 1 = record present; absent = deleted.
    zsets: FastMap<SmolStr, ZSet>,

    /// Bounded LRU row cache. Key: (table_name, record_id) → SpookyRecord bytes.
    ///
    /// Write-through: populated on every Create/Update/bulk_load. Evicts the
    /// least-recently-written entry when capacity is reached. On cache miss,
    /// `get_record_bytes` falls back to a redb read. The cache starts cold on
    /// every open — ZSet is rebuilt from a full scan but record bytes are NOT
    /// pre-loaded.
    row_cache: lru::LruCache<(SmolStr, SmolStr), Vec<u8>>,
}

// ─── Construction ─────────────────────────────────────────────────────────────

impl SpookyDb {
    /// Open or create the database at `path` with default cache capacity (10 000 records).
    ///
    /// Initialises redb tables on first open. Rebuilds all in-memory ZSets
    /// from a sequential RECORDS_TABLE scan — O(N records), ~20–100ms per
    /// million records on an SSD. The LRU row cache starts cold.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, SpookyDbError> {
        Self::new_with_config(path, SpookyDbConfig::default())
    }

    /// Open or create the database at `path` with explicit configuration.
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

        // Ensure tables exist (idempotent).
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

    /// Sequential scan of RECORDS_TABLE on startup.
    ///
    /// Rebuilds `zsets` (weight=1 per key) in a single pass — O(N records),
    /// approximately 20–80ms per million records on SSD. The LRU row cache
    /// starts cold; it warms as records are written or read via `get_record_bytes`.
    ///
    /// Startup memory: only ZSet keys (one SmolStr per record) — no record bytes loaded.
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
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Build a flat redb key `"table:id"` without a heap allocation.
///
/// Uses a stack-allocated `ArrayString<512>`. The combined key must fit
/// in 512 bytes — table names and ids are expected to be well under this
/// limit in practice.
///
/// # Panics
/// Panics (debug) / truncates (release) if `table.len() + 1 + id.len() > 512`.
#[inline]
fn make_key(table: &str, id: &str) -> ArrayString<512> {
    let mut key = ArrayString::<512>::new();
    key.push_str(table);
    key.push(':');
    key.push_str(id);
    key
}

/// Reject table names containing ':' before they can corrupt the flat key namespace.
///
/// The "table:id" key format uses ':' as the only separator.
/// `split_once(':')` in `rebuild_from_records` would mis-parse keys stored
/// under a table name that itself contains ':', silently moving records to the
/// wrong table on every restart.
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

// ─── Write Operations ─────────────────────────────────────────────────────────

impl SpookyDb {
    /// Apply a single mutation in its own write transaction.
    ///
    /// `data` must be pre-serialized SpookyRecord bytes.
    /// Returns `(zset_key, weight_delta)` for the caller to accumulate into a
    /// `BatchMutationResult` if needed.
    ///
    /// # Version tracking
    ///
    /// `version: None` means "do not update the version entry". The previous version
    /// entry (if any) is left unchanged. Callers must provide `version: Some(v)` on
    /// every mutation where version tracking matters, or accept that `get_version` may
    /// return a stale value after an update with `version: None`.
    pub fn apply_mutation(
        &mut self,
        table: &str,
        op: Operation,
        id: &str,
        data: Option<&[u8]>,
        version: Option<u64>,
    ) -> Result<(SmolStr, i64), SpookyDbError> {
        validate_table_name(table)?;

        let key = make_key(table, id);
        let weight = op.weight();

        // 1. Persist to redb FIRST — if commit fails, in-memory state is untouched.
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

        // 2. Update in-memory state AFTER successful commit.
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

        // Return bare id — consistent with apply_batch membership_deltas ZSet key format.
        Ok((SmolStr::new(id), weight))
    }

    /// Batch mutations in **one** write transaction (one fsync).
    ///
    /// All `DbMutation.data` fields must be pre-serialized SpookyRecord bytes.
    /// Serialize with `from_cbor` / `serialize_into` **before** calling this
    /// to minimise write-lock hold time.
    ///
    /// N mutations = 1 transaction = 1 fsync.
    pub fn apply_batch(
        &mut self,
        mutations: Vec<DbMutation>,
    ) -> Result<BatchMutationResult, SpookyDbError> {
        // Validate all table names before touching redb.
        for m in &mutations {
            validate_table_name(&m.table)?;
        }

        // Sort by table to improve cache locality on the in-memory writes.
        // O(n log n) but n is typically small (< 10k) and cheap relative to
        // redb I/O. The redb write loop also iterates the sorted slice.
        let mut mutations = mutations;
        mutations.sort_unstable_by(|a, b| a.table.cmp(&b.table));

        let mut membership_deltas: FastMap<SmolStr, ZSet> = FastMap::default();
        let mut content_updates: FastMap<SmolStr, FastHashSet<SmolStr>> = FastMap::default();
        let mut changed_tables: Vec<SmolStr> = Vec::new();

        // 1. All redb writes in one transaction.
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

        // 2. Update in-memory state AFTER successful commit.
        for mutation in mutations {
            let DbMutation { table, id, op, data, .. } = mutation;

            let was_present = self
                .zsets
                .get(&table)
                .and_then(|z| z.get(&id))
                .copied()
                .unwrap_or(0) > 0;

            let zset = self.zsets.entry(table.clone()).or_default();

            if matches!(op, Operation::Delete) {
                zset.remove(&id);
                self.row_cache.pop(&(table.clone(), id.clone()));
                if was_present {
                    membership_deltas
                        .entry(table.clone())
                        .or_default()
                        .insert(id.clone(), -1);
                }
            } else {
                zset.insert(id.clone(), 1);
                if let Some(bytes) = data {
                    self.row_cache.put((table.clone(), id.clone()), bytes);
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

            // Mutations are sorted by table, so consecutive entries share the same table.
            // Compare against the last pushed value instead of scanning the whole vec.
            if changed_tables.last() != Some(&table) {
                changed_tables.push(table);
            }
        }

        Ok(BatchMutationResult {
            membership_deltas,
            content_updates,
            changed_tables,
        })
    }

    /// Bulk initial load: all records in **one** write transaction.
    ///
    /// Sets every ZSet weight to 1 (records present). Use for startup
    /// hydration or init_load in circuit.rs.
    pub fn bulk_load(
        &mut self,
        records: Vec<BulkRecord>,
    ) -> Result<(), SpookyDbError> {
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
            self.row_cache.put((table, id), data);
        }
        Ok(())
    }
}

// ─── Read Operations ──────────────────────────────────────────────────────────

impl SpookyDb {
    /// Fetch a copy of the raw SpookyRecord bytes for a record.
    ///
    /// **Fast path** (cache hit): `peek()` from the LRU row cache — zero I/O, ~50 ns.
    /// **Slow path** (cache miss): opens a redb read transaction — ~1–10 µs on warm OS cache.
    ///
    /// Returns `None` if the record is absent from the ZSet (deleted or never written).
    ///
    /// Cache misses do NOT populate the cache (requires `&self`). The cache is written
    /// only by Create/Update/bulk_load paths. Use `get_row_record` on the write→read
    /// hot path; fall back to this method when `get_row_record` returns `None`.
    ///
    /// Usage:
    /// ```rust,ignore
    /// let bytes = db.get_record_bytes("users", "alice").unwrap();
    /// let (buf, count) = from_bytes(&bytes).unwrap();
    /// let record = SpookyRecord::new(buf, count);
    /// let age = record.get_i64("age");
    /// ```
    pub fn get_record_bytes(&self, table: &str, id: &str) -> Option<Vec<u8>> {
        // ZSet guard — avoids unnecessary redb open for absent records.
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

    /// Zero-copy borrowed SpookyRecord for the view evaluation hot path.
    ///
    /// Returns `Some(SpookyRecord<'a>)` if and only if the record is in the LRU row cache.
    /// Returns `None` if the record is absent **or** if it exists on disk but has been
    /// evicted from the cache.
    ///
    /// **Cache miss fallback**: call `get_record_bytes(table, id)` which reads from redb.
    ///
    /// For the streaming pipeline hot path (write then read in the same tick), records
    /// are always in the cache — writes populate it immediately. Zero I/O, zero allocation.
    pub fn get_row_record<'a>(&'a self, table: &str, id: &str) -> Option<SpookyRecord<'a>> {
        // ZSet guard — avoid cache lookup for absent records.
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

    /// Reconstruct a partial `SpookyValue::Object` from a stored record.
    ///
    /// Only fields whose names are listed in `fields` are included. Unknown
    /// hashes (fields not in `fields`) are silently skipped — field names are
    /// not stored in the binary format and cannot be recovered from hashes.
    ///
    /// Returns `None` if the record does not exist.
    ///
    /// Use `get_record_bytes` + `SpookyReadable` accessors on the hot path.
    /// Use this for compatibility layers that need a named `SpookyValue`.
    pub fn get_record_typed(
        &self,
        table: &str,
        id: &str,
        fields: &[&str],
    ) -> Result<Option<SpookyValue>, SpookyDbError> {
        let raw = match self.get_record_bytes(table, id) {
            Some(b) => b,
            None => return Ok(None),
        };

        let (buf, count) = from_bytes(&raw)?;
        let record = SpookyRecord::new(buf, count);

        let mut map = std::collections::BTreeMap::new();
        for &name in fields {
            if let Some(val) = record.get_field::<SpookyValue>(name) {
                map.insert(SmolStr::new(name), val);
            }
        }
        Ok(Some(SpookyValue::Object(map)))
    }

    /// Version for a record (sync / conflict detection).
    ///
    /// Returns `None` if the record has no version entry.
    ///
    /// Fast path: if the record is not in the ZSet (weight = 0), it cannot
    /// have a version entry — returns `None` without opening a redb transaction.
    pub fn get_version(&self, table: &str, id: &str) -> Result<Option<u64>, SpookyDbError> {
        // Fast path: absent from ZSet → definitely not in VERSION_TABLE.
        let present = self
            .zsets
            .get(table)
            .and_then(|z| z.get(id))
            .copied()
            .unwrap_or(0)
            > 0;
        if !present {
            return Ok(None);
        }

        // Slow path: record is present — check VERSION_TABLE (version is not
        // cached in memory; a record may exist with no version entry).
        let key = make_key(table, id);
        let read_txn = self.db.begin_read()?;
        let tbl = read_txn.open_table(VERSION_TABLE)?;
        Ok(tbl
            .get(key.as_str())?
            .map(|guard: redb::AccessGuard<u64>| guard.value()))
    }
}

// ─── ZSet Operations (pure memory, zero I/O) ─────────────────────────────────

impl SpookyDb {
    /// Full ZSet for a table. Pure memory, zero I/O.
    ///
    /// Returns `None` if the table has never had any records.
    /// The borrow is valid until the next `&mut self` call.
    ///
    /// This is what `eval_snapshot(Scan)` borrows for the duration of a view tick.
    pub fn get_table_zset(&self, table: &str) -> Option<&ZSet> {
        self.zsets.get(table)
    }

    /// Weight for a single record. Returns 0 if absent (standard ZSet semantics).
    pub fn get_zset_weight(&self, table: &str, id: &str) -> i64 {
        self.zsets
            .get(table)
            .and_then(|z| z.get(id).copied())
            .unwrap_or(0)
    }

    /// Applies a pre-computed ZSet delta to the in-memory state.
    ///
    /// This is `pub(crate)` because it is intended only for checkpoint-recovery paths
    /// where the delta has already been validated and committed to disk. Do not call
    /// this from general application code — use `apply_mutation` or `apply_batch` instead,
    /// which maintain ZSet/disk atomicity.
    #[allow(dead_code)]
    pub(crate) fn apply_zset_delta_memory(&mut self, table: &str, delta: &ZSet) {
        let zset = self.zsets.entry(SmolStr::new(table)).or_default();
        for (id, weight) in delta {
            let entry = zset.entry(id.clone()).or_insert(0);
            *entry += weight;
            debug_assert!(
                *entry == 0 || *entry == 1,
                "apply_zset_delta_memory: weight out of range after delta {weight}: got {entry}",
                entry = *entry
            );
            // Remove entries that have reached zero weight.
            if *entry == 0 {
                zset.remove(id);
            }
        }
    }
}

// ─── Table Info (pure memory, O(1)) ──────────────────────────────────────────

impl SpookyDb {
    /// Returns `true` if the table has at least one record in the in-memory ZSet.
    pub fn table_exists(&self, table: &str) -> bool {
        self.zsets
            .get(table)
            .map(|z| !z.is_empty())
            .unwrap_or(false)
    }

    /// All known table names (derived from in-memory ZSet keys).
    pub fn table_names(&self) -> impl Iterator<Item = &SmolStr> {
        self.zsets.keys()
    }

    /// Record count for a table.
    ///
    /// O(1) — ZSet entries = records present.
    pub fn table_len(&self, table: &str) -> usize {
        self.zsets.get(table).map(|z| z.len()).unwrap_or(0)
    }

    /// Ensures an in-memory ZSet entry exists for `table`.
    ///
    /// This guarantees that subsequent calls to `get_table_zset` return `Some(&ZSet)`
    /// rather than `None`, even before any records are inserted. However, `table_exists`
    /// checks whether the ZSet is non-empty — an ensured but empty table still returns
    /// `false` from `table_exists`.
    ///
    /// Use this to pre-allocate the ZSet slot before bulk operations.
    ///
    /// Returns `Err(SpookyDbError::InvalidKey)` if the table name contains `':'`.
    pub fn ensure_table(&mut self, table: &str) -> Result<(), SpookyDbError> {
        validate_table_name(table)?;
        self.zsets.entry(SmolStr::new(table)).or_default();
        Ok(())
    }
}

// ─── DbBackend trait ──────────────────────────────────────────────────────────

/// Thin adapter trait for incremental migration from the old in-memory
/// `Database` struct to `SpookyDb`. Implement for both; wire `circuit.rs`
/// against the trait.
///
/// All write operations return `Result` — a disk-full or corruption error must
/// never silently become a no-op. Callers must handle or propagate write errors.
pub trait DbBackend {
    /// Zero-copy ZSet access. Borrowed from memory — zero I/O.
    fn get_table_zset(&self, table: &str) -> Option<&ZSet>;

    /// Raw bytes for a record, served from in-memory cache.
    /// Returns `None` if the record is absent. Zero I/O — never fails.
    fn get_record_bytes(&self, table: &str, id: &str) -> Option<Vec<u8>>;

    /// Zero-copy borrowed record access. Returns `None` if the record is absent.
    ///
    /// Default implementation returns `None` (falls back to `get_record_bytes` for
    /// backends without an in-memory row cache). Backends with an in-memory row
    /// cache should override this for hot-path efficiency.
    fn get_row_record_bytes<'a>(&'a self, _table: &str, _id: &str) -> Option<&'a [u8]> {
        None
    }

    /// Register an empty table.
    ///
    /// Returns `Err(SpookyDbError::InvalidKey)` if `table` contains `':'`.
    fn ensure_table(&mut self, table: &str) -> Result<(), SpookyDbError>;

    /// Single mutation: record write + ZSet update.
    fn apply_mutation(
        &mut self,
        table: &str,
        op: Operation,
        id: &str,
        data: Option<&[u8]>,
        version: Option<u64>,
    ) -> Result<(SmolStr, i64), SpookyDbError>;

    /// Batch mutations in one transaction.
    fn apply_batch(
        &mut self,
        mutations: Vec<DbMutation>,
    ) -> Result<BatchMutationResult, SpookyDbError>;

    /// Bulk initial load.
    fn bulk_load(
        &mut self,
        records: Vec<BulkRecord>,
    ) -> Result<(), SpookyDbError>;

    /// Weight for one record. Returns 0 if absent.
    fn get_zset_weight(&self, table: &str, id: &str) -> i64;

    /// Reconstruct a partial `SpookyValue::Object` from a stored record.
    ///
    /// Only fields whose names are listed in `fields` are included. Field names
    /// are not recoverable from hashes — callers must supply the expected names.
    /// Returns `Ok(None)` if the record does not exist.
    fn get_record_typed(
        &self,
        table: &str,
        id: &str,
        fields: &[&str],
    ) -> Result<Option<SpookyValue>, SpookyDbError>;
}

impl DbBackend for SpookyDb {
    fn get_table_zset(&self, table: &str) -> Option<&ZSet> {
        self.get_table_zset(table)
    }

    fn get_record_bytes(&self, table: &str, id: &str) -> Option<Vec<u8>> {
        SpookyDb::get_record_bytes(self, table, id)
    }

    fn get_row_record_bytes<'a>(&'a self, table: &str, id: &str) -> Option<&'a [u8]> {
        // Cache-only — None on cache miss (same semantics as get_row_record).
        let cache_key = (SmolStr::new(table), SmolStr::new(id));
        self.row_cache.peek(&cache_key).map(|v| v.as_slice())
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
        self.get_zset_weight(table, id)
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

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serialization::from_cbor;
    use tempfile::NamedTempFile;

    // BENCH_CBOR: a pre-serialized CBOR map (12 fields) representing a realistic
    // user record. Used by all test helpers that need pre-built SpookyRecord bytes.
    //
    // Fields and values (as CBOR):
    //   active:      true                              (bool)
    //   age:         28                                (uint/i64)
    //   count:       1000                              (uint)
    //   deleted:     false                             (bool)
    //   history:     [{action:"login",  timestamp:1234567890},
    //                 {action:"update", timestamp:1234567900}]  (array of 2 objects)
    //   id:          "user:abc123"                     (str)
    //   metadata:    null
    //   mixed_array: [42, "text", true, {nested:"value"}]       (array of 4)
    //   name:        "Alice"                           (str)
    //   profile:     {avatar:"https://example.com/avatar.jpg",
    //                 bio:"Software engineer",
    //                 settings:{notifications:true,
    //                           privacy:{level:3, public:false},
    //                           theme:"dark"}}         (nested object)
    //   score:       99.5                              (f64)
    //   tags:        ["developer", "rust", "database"] (array of 3 strings)
    //
    // Regenerate: build a cbor4ii::core::Value::Map with the above fields, call
    // `from_cbor(&val)` to obtain the SpookyRecord bytes, and print them as a
    // Rust byte-array literal.
    const BENCH_CBOR: &[u8] = &[
        172, 102, 97, 99, 116, 105, 118, 101, 245, 99, 97, 103, 101, 24, 28, 101, 99, 111, 117,
        110, 116, 25, 3, 232, 103, 100, 101, 108, 101, 116, 101, 100, 244, 103, 104, 105, 115,
        116, 111, 114, 121, 130, 162, 102, 97, 99, 116, 105, 111, 110, 101, 108, 111, 103, 105,
        110, 105, 116, 105, 109, 101, 115, 116, 97, 109, 112, 26, 73, 150, 2, 210, 162, 102, 97,
        99, 116, 105, 111, 110, 102, 117, 112, 100, 97, 116, 101, 105, 116, 105, 109, 101, 115,
        116, 97, 109, 112, 26, 73, 150, 2, 220, 98, 105, 100, 107, 117, 115, 101, 114, 58, 97,
        98, 99, 49, 50, 51, 104, 109, 101, 116, 97, 100, 97, 116, 97, 246, 107, 109, 105, 120,
        101, 100, 95, 97, 114, 114, 97, 121, 132, 24, 42, 100, 116, 101, 120, 116, 245, 161,
        102, 110, 101, 115, 116, 101, 100, 101, 118, 97, 108, 117, 101, 100, 110, 97, 109, 101,
        101, 65, 108, 105, 99, 101, 103, 112, 114, 111, 102, 105, 108, 101, 163, 102, 97, 118,
        97, 116, 97, 114, 120, 30, 104, 116, 116, 112, 115, 58, 47, 47, 101, 120, 97, 109, 112,
        108, 101, 46, 99, 111, 109, 47, 97, 118, 97, 116, 97, 114, 46, 106, 112, 103, 99, 98,
        105, 111, 113, 83, 111, 102, 116, 119, 97, 114, 101, 32, 101, 110, 103, 105, 110, 101,
        101, 114, 104, 115, 101, 116, 116, 105, 110, 103, 115, 163, 109, 110, 111, 116, 105,
        102, 105, 99, 97, 116, 105, 111, 110, 115, 245, 103, 112, 114, 105, 118, 97, 99, 121,
        162, 101, 108, 101, 118, 101, 108, 3, 102, 112, 117, 98, 108, 105, 99, 244, 101, 116,
        104, 101, 109, 101, 100, 100, 97, 114, 107, 101, 115, 99, 111, 114, 101, 251, 64, 88,
        224, 0, 0, 0, 0, 0, 100, 116, 97, 103, 115, 131, 105, 100, 101, 118, 101, 108, 111,
        112, 101, 114, 100, 114, 117, 115, 116, 104, 100, 97, 116, 97, 98, 97, 115, 101,
    ];

    #[test]
    fn test_new_opens_empty_db() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = NamedTempFile::new()?;
        let db = SpookyDb::new(tmp.path())?;
        assert!(!db.table_exists("users"));
        assert_eq!(db.table_len("users"), 0);
        Ok(())
    }

    #[test]
    fn test_apply_mutation_create_get_delete() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = NamedTempFile::new()?;
        let mut db = SpookyDb::new(tmp.path())?;

        let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
        let (data, _) = from_cbor(&cbor)?;

        // Create
        db.apply_mutation("users", Operation::Create, "alice", Some(&data), Some(1))?;
        assert!(db.table_exists("users"));
        assert_eq!(db.table_len("users"), 1);
        assert_eq!(db.get_zset_weight("users", "alice"), 1);

        // Get raw bytes back
        let fetched = db.get_record_bytes("users", "alice").expect("should exist");
        assert_eq!(fetched, data);

        // Version
        assert_eq!(db.get_version("users", "alice")?, Some(1));

        // Delete
        db.apply_mutation("users", Operation::Delete, "alice", None, None)?;
        assert_eq!(db.get_zset_weight("users", "alice"), 0);
        assert!(db.get_record_bytes("users", "alice").is_none());
        assert_eq!(db.table_len("users"), 0);

        Ok(())
    }

    #[test]
    fn test_apply_batch_one_txn() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = NamedTempFile::new()?;
        let mut db = SpookyDb::new(tmp.path())?;

        let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
        let (data, _) = from_cbor(&cbor)?;

        let mutations = vec![
            DbMutation {
                table: SmolStr::new("users"),
                id: SmolStr::new("u1"),
                op: Operation::Create,
                data: Some(data.clone()),
                version: Some(1),
            },
            DbMutation {
                table: SmolStr::new("users"),
                id: SmolStr::new("u2"),
                op: Operation::Create,
                data: Some(data.clone()),
                version: Some(1),
            },
            DbMutation {
                table: SmolStr::new("posts"),
                id: SmolStr::new("p1"),
                op: Operation::Create,
                data: Some(data.clone()),
                version: Some(1),
            },
        ];

        let result = db.apply_batch(mutations)?;

        assert_eq!(db.table_len("users"), 2);
        assert_eq!(db.table_len("posts"), 1);
        assert_eq!(result.membership_deltas["users"].len(), 2);
        assert_eq!(result.membership_deltas["posts"].len(), 1);
        assert!(result.changed_tables.contains(&SmolStr::new("users")));
        assert!(result.changed_tables.contains(&SmolStr::new("posts")));

        Ok(())
    }

    #[test]
    fn test_bulk_load() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = NamedTempFile::new()?;
        let mut db = SpookyDb::new(tmp.path())?;

        let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
        let (data, _) = from_cbor(&cbor)?;

        let records = vec![
            BulkRecord {
                table: SmolStr::new("items"),
                id: SmolStr::new("i1"),
                data: data.clone(),
                version: None,
            },
            BulkRecord {
                table: SmolStr::new("items"),
                id: SmolStr::new("i2"),
                data: data.clone(),
                version: None,
            },
        ];

        db.bulk_load(records)?;
        assert_eq!(db.table_len("items"), 2);
        assert_eq!(db.get_zset_weight("items", "i1"), 1);
        assert_eq!(db.get_zset_weight("items", "i2"), 1);

        Ok(())
    }

    #[test]
    fn test_zset_survives_reopen() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = NamedTempFile::new()?;
        let path = tmp.path().to_path_buf();
        // Keep file alive but drop NamedTempFile handle so only the path remains.
        // Use a regular tempdir file to keep the path valid.
        let tmp_dir = tempfile::tempdir()?;
        let db_path = tmp_dir.path().join("test.redb");

        let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
        let (data, _) = from_cbor(&cbor)?;

        {
            let mut db = SpookyDb::new(&db_path)?;
            db.apply_mutation("users", Operation::Create, "alice", Some(&data), Some(1))?;
            db.apply_mutation("users", Operation::Create, "bob", Some(&data), Some(2))?;
            assert_eq!(db.table_len("users"), 2);
        }

        // Reopen — ZSet must be rebuilt from RECORDS_TABLE.
        let db2 = SpookyDb::new(&db_path)?;
        assert_eq!(db2.table_len("users"), 2);
        assert_eq!(db2.get_zset_weight("users", "alice"), 1);
        assert_eq!(db2.get_zset_weight("users", "bob"), 1);

        // Suppress unused path warning.
        let _ = path;
        Ok(())
    }

    #[test]
    fn test_get_record_typed_partial() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = NamedTempFile::new()?;
        let mut db = SpookyDb::new(tmp.path())?;

        let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
        let (data, _) = from_cbor(&cbor)?;

        db.apply_mutation("users", Operation::Create, "alice", Some(&data), None)?;

        // The CBOR fixture has an "age" field (i64 = 28) and "active" (bool).
        let val = db
            .get_record_typed("users", "alice", &["age", "active"])?
            .expect("should exist");

        assert!(matches!(val, SpookyValue::Object(_)));
        if let SpookyValue::Object(map) = val {
            // "age" and "active" should be present.
            assert!(map.contains_key("age"), "age field missing");
            assert!(map.contains_key("active"), "active field missing");
        }

        Ok(())
    }

    #[test]
    fn test_ensure_table_and_table_names() {
        let tmp = NamedTempFile::new().unwrap();
        let mut db = SpookyDb::new(tmp.path()).unwrap();

        assert!(!db.table_exists("empty_table"));
        db.ensure_table("empty_table").unwrap();
        // ensure_table creates the ZSet entry, but table_exists checks for non-empty.
        // An empty ZSet → table_exists returns false (no records yet).
        assert!(!db.table_exists("empty_table"));
        // But table_names() still lists it.
        let names: Vec<&SmolStr> = db.table_names().collect();
        assert!(names.contains(&&SmolStr::new("empty_table")));

        // Table names containing ':' must be rejected.
        assert!(matches!(
            db.ensure_table("bad:table"),
            Err(SpookyDbError::InvalidKey(_))
        ));
    }

    #[test]
    fn test_row_cache_populated_on_create() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = NamedTempFile::new()?;
        let mut db = SpookyDb::new(tmp.path())?;
        let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
        let (data, _) = from_cbor(&cbor)?;

        db.apply_mutation("users", Operation::Create, "alice", Some(&data), None)?;

        // get_record_bytes must return without touching redb.
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

        // After reopen: ZSet is rebuilt from RECORDS_TABLE; LRU cache starts cold.
        let db2 = SpookyDb::new(&db_path)?;

        // ZSet is correct — record is known present.
        assert_eq!(db2.get_zset_weight("users", "alice"), 1);

        // get_record_bytes falls back to redb on cache miss — still returns data.
        assert_eq!(db2.get_record_bytes("users", "alice"), Some(data));

        // get_row_record returns None because the cache is cold after reopen.
        assert!(
            db2.get_row_record("users", "alice").is_none(),
            "cold cache: get_row_record must return None until a write warms the entry"
        );
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
        assert_eq!(db2.get_zset_weight("users", "alice"), 1); // ZSet present

        // get_row_record returns None (cold cache after reopen).
        assert!(db2.get_row_record("users", "alice").is_none());

        // get_record_bytes falls back to redb — still returns data.
        let fetched = db2
            .get_record_bytes("users", "alice")
            .expect("redb fallback must work on cache miss");
        assert_eq!(fetched, data);

        Ok(())
    }

    #[test]
    fn test_cache_eviction_correctness() -> Result<(), Box<dyn std::error::Error>> {
        // Cache capacity 2, insert 3 records. 3rd insert evicts the 1st.
        // Verify: ZSet has all 3; get_record_bytes works for all 3 (redb fallback);
        // get_row_record returns None for the evicted record.
        let tmp = NamedTempFile::new()?;
        let mut db = SpookyDb::new_with_config(
            tmp.path(),
            SpookyDbConfig {
                cache_capacity: std::num::NonZeroUsize::new(2).unwrap(),
            },
        )?;

        let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
        let (data, _) = from_cbor(&cbor)?;

        db.apply_mutation("t", Operation::Create, "r1", Some(&data), None)?;
        db.apply_mutation("t", Operation::Create, "r2", Some(&data), None)?;
        db.apply_mutation("t", Operation::Create, "r3", Some(&data), None)?; // evicts r1

        // ZSet has all 3.
        assert_eq!(db.get_zset_weight("t", "r1"), 1);
        assert_eq!(db.get_zset_weight("t", "r2"), 1);
        assert_eq!(db.get_zset_weight("t", "r3"), 1);

        // get_record_bytes works for all 3 (redb fallback for evicted r1).
        assert!(db.get_record_bytes("t", "r1").is_some(), "redb fallback for evicted r1");
        assert!(db.get_record_bytes("t", "r2").is_some());
        assert!(db.get_record_bytes("t", "r3").is_some());

        // get_row_record: r1 evicted, r2 and r3 still in cache.
        assert!(db.get_row_record("t", "r1").is_none(), "r1 should be evicted from cache");
        assert!(db.get_row_record("t", "r2").is_some(), "r2 should still be in cache");
        assert!(db.get_row_record("t", "r3").is_some(), "r3 should be in cache");

        Ok(())
    }

    #[test]
    fn test_cache_capacity_bounds_memory() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = NamedTempFile::new()?;
        let mut db = SpookyDb::new_with_config(
            tmp.path(),
            SpookyDbConfig {
                cache_capacity: std::num::NonZeroUsize::new(5).unwrap(),
            },
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

        // get_record_bytes works for all 10 via redb fallback.
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
        assert!(db.get_row_record("t", "r1").is_some(), "r1 should be in cache after create");

        db.apply_mutation("t", Operation::Delete, "r1", None, None)?;
        // ZSet and cache must both be gone; ZSet guard prevents redb read.
        assert_eq!(db.get_zset_weight("t", "r1"), 0);
        assert!(db.get_row_record("t", "r1").is_none());
        assert!(db.get_record_bytes("t", "r1").is_none());

        Ok(())
    }

    #[test]
    fn test_get_row_record_zero_copy() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = NamedTempFile::new()?;
        let mut db = SpookyDb::new(tmp.path())?;

        let cbor: cbor4ii::core::Value = cbor4ii::serde::from_slice(BENCH_CBOR)?;
        let (data, _) = from_cbor(&cbor)?;

        // Non-existent record returns None.
        assert!(db.get_row_record("users", "alice").is_none());

        // Insert a record, then verify we can read a field from the zero-copy view.
        db.apply_mutation("users", Operation::Create, "alice", Some(&data), None)?;

        let record = db.get_row_record("users", "alice").expect("should be in cache");
        // The CBOR fixture has "age" = 28 (i64).
        let age = record.get_i64("age");
        assert!(age.is_some(), "age field should be readable from cached record");
        assert_eq!(age.unwrap(), 28);

        Ok(())
    }
}
