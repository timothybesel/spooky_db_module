# SpookyDb — Architecture

## Project Overview

SpookyDb is a high-performance embedded record store implemented as a Rust library crate. It solves the problem of storing, mutating, and querying structured records with minimal per-operation overhead, targeting streaming and DBSP-style data pipeline workloads where field access latency and write throughput dominate. Library consumers build incremental view-evaluation loops on top of it, using the ZSet abstraction to track record membership and the binary record format to read individual fields with zero allocation. The primary design goal is performance: every hot-path operation — field lookup, in-place mutation, ZSet membership check — is measured in nanoseconds.

---

## Layer Map

| Layer | Module(s) | Responsibility | Key Types |
|-------|-----------|----------------|-----------|
| 1 — Binary Format | `src/types.rs` | Wire format constants, layout sizes, in-memory index representation, field iteration | `IndexEntry`, `FieldRef`, `FieldSlot`, `FieldIter`, `TAG_*` constants, `HEADER_SIZE`, `INDEX_ENTRY_SIZE` |
| 2 — Serialization | `src/serialization.rs`, `src/deserialization.rs`, `src/error.rs` | Encode and decode records; adapter traits for multiple value types | `RecordSerialize`, `RecordDeserialize`, `RecordError`, `serialize`, `serialize_into`, `from_spooky`, `from_cbor`, `from_bytes`, `decode_field`, `write_field_into` |
| 3 — Record Types | `src/spooky_record/`, `src/spooky_value.rs` | Zero-copy reads, in-place and structural mutations, dynamic value representation | `SpookyRecord<'a>`, `SpookyRecordMut`, `SpookyReadable`, `SpookyValue`, `SpookyNumber`, `FieldSlot` |
| 4 — Persistence | `src/db/db.rs`, `src/db/types.rs`, `src/db/mod.rs` | redb-backed durable store, in-memory ZSet, bounded LRU row cache, batch write API | `SpookyDb`, `DbBackend`, `Operation`, `DbMutation`, `BulkRecord`, `BatchMutationResult`, `ZSet`, `SpookyDbConfig`, `SpookyDbError` |

```
┌────────────────────────────────────────────────────────────────────┐
│  Caller / Pipeline                                                 │
│  (SpookyValue::Object, serde_json::Value, cbor4ii::core::Value)   │
└─────────────────────────────┬──────────────────────────────────────┘
                              │  BTreeMap<SmolStr, V: RecordSerialize>
                              ▼
┌────────────────────────────────────────────────────────────────────┐
│  Layer 2 — Serialization                                           │
│  serialize() / serialize_into() / from_spooky() / from_cbor()     │
│  write_field_into() dispatches scalars → LE bytes, nested → CBOR  │
└─────────────────────────────┬──────────────────────────────────────┘
                              │  Vec<u8> + field_count
                              ▼
┌────────────────────────────────────────────────────────────────────┐
│  Layer 1 — Binary Format                                           │
│  [Header 20B][Index N×20B sorted by xxh64][Data variable]         │
│  IndexEntry / FieldRef / FieldSlot / FieldIter                     │
└──────────────┬──────────────────────────────┬──────────────────────┘
               │ &[u8] (borrow)               │ Vec<u8> (own)
               ▼                              ▼
┌──────────────────────────┐   ┌──────────────────────────────────────┐
│  SpookyRecord<'a>        │   │  SpookyRecordMut                     │
│  (Layer 3 — immutable)   │   │  (Layer 3 — mutable)                 │
│  Copy, zero-cost         │   │  generation counter, 3 mutation paths │
│  SpookyReadable trait    │   │  SpookyReadable trait                 │
└──────────────────────────┘   └──────────────────────────────────────┘
                              │  data_buf: Vec<u8> persisted
                              ▼
┌────────────────────────────────────────────────────────────────────┐
│  Layer 4 — Persistence                                             │
│  SpookyDb { db: RedbDatabase, zsets: FastMap, row_cache: LruCache} │
│  RECORDS_TABLE (&str → &[u8])   VERSION_TABLE (&str → u64)        │
│  apply_batch → 1 redb txn → commit → update ZSet + LRU cache      │
└────────────────────────────────────────────────────────────────────┘
```

---

## Layer 1 — Binary Format (`src/types.rs`)

Every record is a flat `Vec<u8>` / `&[u8]` divided into three consecutive regions: a fixed 20-byte header, a fixed-width sorted index of 20 bytes per field, and a variable-length data section holding the raw field values.

The **header** stores a single meaningful field: `field_count` as a `u32` in little-endian byte order at offset 0, followed by 16 reserved bytes zeroed on write. Reading the field count requires a single 4-byte LE decode with no pointer chasing.

The **index** is an array of `N` entries each occupying exactly 20 bytes. Each entry encodes: an 8-byte `name_hash` (`xxh64` of the UTF-8 field name, seed 0), a 4-byte `data_offset` pointing into the data region, a 4-byte `data_length`, a 1-byte `type_tag`, and 3 bytes of padding to preserve 4-byte alignment. The index is always stored **sorted ascending by `name_hash`**. This invariant is enforced at every serialization path and enables O(log N) binary search on reads. For records with 4 or fewer fields the reader falls back to a linear scan, which is faster at that size due to branch predictor behaviour. Violating the sorted invariant silently corrupts all field lookups — the binary search will return wrong positions or `FieldNotFound` for existing fields.

The **data** region immediately follows the index. Fields are stored in sorted hash order (matching the index), packed sequentially with no delimiters. Flat types occupy their natural size in little-endian bytes. Strings are raw UTF-8 with no length prefix — the length comes from the index entry. Null occupies zero bytes. Nested arrays and objects are CBOR-encoded inline. The layout means any field can be located and read by computing a single slice `data_buf[data_offset..data_offset + data_len]` — no parsing loop, no allocation.

xxh64 hashes are used instead of storing field name strings to keep the index small and fixed-width. The trade-off is irreversible: field names cannot be recovered from a serialized buffer without an external name table. This is intentional.

### Binary Layout Diagram

```
┌─ Header (20 bytes) ──────────────────────────────────┐
│  field_count: u32 (LE)  |  _reserved: [u8; 16]        │
├─ Index (20 bytes × N) ─── SORTED by name_hash ────────┤
│  name_hash:   u64 (LE)    ← xxh64(field_name, seed=0) │
│  data_offset: u32 (LE)                                │
│  data_length: u32 (LE)                                │
│  type_tag:    u8          ← TAG_* constants            │
│  _padding:    [u8; 3]                                 │
├─ Data (variable) ──────────────────────────────────────┤
│  Flat types: native LE bytes (i64/u64/f64 = 8 bytes,  │
│              bool = 1 byte, null = 0 bytes)            │
│  Strings:    raw UTF-8                                 │
│  Nested:     CBOR-encoded (arrays + objects)           │
└────────────────────────────────────────────────────────┘
```

### Type Tags

| Constant | Value | Encoded Type | Data Size |
|----------|-------|--------------|-----------|
| `TAG_NULL` | `0` | Null / absent | 0 bytes |
| `TAG_BOOL` | `1` | Boolean | 1 byte (`0` or `1`) |
| `TAG_I64` | `2` | Signed 64-bit integer | 8 bytes (LE) |
| `TAG_F64` | `3` | IEEE 754 double | 8 bytes (LE) |
| `TAG_STR` | `4` | UTF-8 string | variable (raw bytes, no NUL) |
| `TAG_NESTED_CBOR` | `5` | Array or Object | variable (CBOR-encoded) |
| `TAG_U64` | `6` | Unsigned 64-bit integer | 8 bytes (LE) |

### Internal Types

**`IndexEntry`** (`src/types.rs`) is a parsed, owned representation of one 20-byte index slot. It holds `name_hash: u64`, `data_offset: usize`, `data_len: usize`, and `type_tag: u8`. It is produced by `SpookyReadable::read_index(i)` via unaligned pointer reads with explicit LE conversion.

**`FieldRef<'a>`** (`src/types.rs`) is a zero-copy reference to a field's raw bytes with a lifetime tied to the buffer. It holds `name_hash`, `type_tag`, and `data: &'a [u8]`. It is the return type of `get_raw` and the input to `decode_field`. No allocation occurs constructing or passing a `FieldRef`.

**`FieldSlot`** (`src/types.rs`) is a cached field position intended for O(1) repeat access. It contains `index_pos`, `data_offset`, `data_len`, `type_tag`, and a `generation: usize` counter that must match the owning `SpookyRecordMut`'s generation at every use. It is produced by `resolve(name)` and consumed by the `_at` family of getters and setters. Staleness is detected via `debug_assert` — zero overhead in release builds.

**`FieldIter<'a>`** (`src/types.rs`) is a `Copy`-friendly iterator over all fields in a record. Each `next()` call decodes one index entry and returns a `FieldRef`. It implements `ExactSizeIterator` because the total field count is known from the header.

---

## Layer 2 — Serialization (`src/serialization.rs`, `src/deserialization.rs`)

### RecordSerialize and RecordDeserialize

**`RecordSerialize`** is the adapter trait that abstracts over value types that can be written into the binary format. It extends `serde::Serialize` and adds typed inspection methods: `is_null`, `as_bool`, `as_i64`, `as_u64`, `as_f64`, `as_str`, `is_nested`. The serializer queries these methods in priority order to determine which type tag to emit.

**`RecordDeserialize`** is the symmetric adapter trait for reading. It provides constructor methods: `from_null`, `from_bool`, `from_i64`, `from_u64`, `from_f64`, `from_str`, `from_cbor_bytes`. The `decode_field` function dispatches on `type_tag` and calls the appropriate constructor.

### Serialization Pipeline

1. Caller builds a `BTreeMap<SmolStr, V>` where `V: RecordSerialize`.
2. `serialize()` or `serialize_into()` is called with that map.
3. A stack-allocated `ArrayVec<(&V, u64), 32>` is filled with `(value_ref, xxh64(key))` pairs. If the map has more than 32 entries, `RecordError::TooManyFields` is returned immediately.
4. The `ArrayVec` is sorted in place by hash — entirely on the stack with no heap allocation.
5. The buffer is pre-sized to `HEADER_SIZE + N * INDEX_ENTRY_SIZE` with zeros.
6. For each sorted `(value, hash)` pair, `write_field_into` appends encoded bytes and backfills the index entry.
7. The completed buffer and field count are returned.

### RecordSerialize Implementations

| Value Type | Null | Bool | i64 | u64 | f64 | str | Nested |
|------------|------|------|-----|-----|-----|-----|--------|
| `SpookyValue` | `Null` variant | `Bool` | `Number(I64)` | `Number(U64)` | `Number(F64)` | `Str` | `Array` / `Object` |
| `serde_json::Value` | `Null` | `Bool` | `.as_i64()` | `.as_u64()` | `.as_f64()` | `String` | `Array` / `Object` |
| `cbor4ii::core::Value` | `Null` | `Bool` | `Integer` (fits i64) | `Integer` (fits u64) | `Float` / `Integer`→f64 | `Text` | `Array` / `Map` |

All three types also implement `RecordDeserialize`. Any of the three can be written into or read out of the binary format without going through `SpookyValue` as an intermediate.

### Buffer Reuse Pattern

`serialize()` allocates a new `Vec<u8>` per call. `serialize_into(map, buf)` accepts a caller-supplied buffer, calls `buf.clear()` (retaining capacity), and fills it in place. Buffer reuse eliminates the per-record heap allocation — approximately 17% throughput improvement for bulk ingestion.

---

## Layer 3 — Record Types (`src/spooky_record/`)

### SpookyRecord<'a> — Immutable

`SpookyRecord<'a>` borrows `&'a [u8]` and derives `Copy`. Because it is `Copy`, it can be passed to functions, stored in iterators, and duplicated without runtime cost. All read operations are provided by the `SpookyReadable` trait.

### SpookyRecordMut — Mutable

`SpookyRecordMut` owns its buffer as `Vec<u8>` and adds a `generation: usize` counter that starts at 0 and increments on every layout-changing mutation. `as_record()` produces a zero-copy `SpookyRecord<'_>` view over the owned buffer.

### FieldSlot Pattern

`resolve(name)` performs a single O(log N) lookup and packages the result into a `FieldSlot`. Subsequent `_at` accessors jump directly to `data_buf[slot.data_offset..]` — approximately 2–3 ns vs ~10 ns for a by-name lookup. Each `_at` call includes `debug_assert_eq!(slot.generation, self.generation(), "stale FieldSlot")` — compiled out in release builds.

### Mutation Paths

| Path | Methods | Allocation | Generation |
|------|---------|------------|------------|
| In-place | `set_i64`, `set_u64`, `set_f64`, `set_bool`, `set_str` (same byte length), `set_str_exact`, `set_*_at` variants | 0 | unchanged |
| Splice | `set_str` (different length), `set_field` (different size), `set_null` (if size changes) | in-place `Vec` resize | +1 |
| Full rebuild | `add_field`, `remove_field` | new `Vec` via `rebuild_buffer_with` | +1 |

---

## Layer 4 — Persistence (`src/db/`)

### SpookyDb State

`SpookyDb` has three fields:

- **`db: RedbDatabase`** — the on-disk redb instance. Written on every mutation via `begin_write()` / `commit()`. Read during startup and on cache misses.
- **`zsets: FastMap<SmolStr, ZSet>`** — in-memory ZSet map. Weight `1` = present; absence = deleted. Never persisted to a dedicated redb table.
- **`row_cache: lru::LruCache<(SmolStr, SmolStr), Vec<u8>>`** — bounded LRU cache keyed by `(table_name, record_id)`. Default capacity 10,000 entries. Cold on every open.

### ZSet Design

ZSets are rebuilt from a sequential scan of `RECORDS_TABLE` on startup (`rebuild_from_records`). During operation, the ZSet is updated only after a successful redb commit. Weight is always exactly 0 (absent) or 1 (present) for single-record operations.

`get_table_zset(table)` returns `Option<&ZSet>` — zero I/O, valid until the next `&mut self` call.

### LRU Row Cache Design

Write-through: every `Create`, `Update`, and `bulk_load` record populates the cache after redb commit. Eviction order reflects write time (not read time) because all read paths use `peek()` — `DbBackend` read methods take `&self`, and `LruCache::get()` requires `&mut self`.

`get_row_record` — hot-path zero-copy accessor: ZSet guard → `peek()` → `SpookyRecord<'_>` borrowing the cache entry. Returns `None` on cache miss (no redb fallback). Caller must fall back to `get_record_bytes`.

`get_record_bytes` — ZSet guard → `peek()` → redb read on miss (clones bytes into `Vec<u8>`). Cache is NOT populated by this path.

### Write Atomicity

Redb is written and committed first. In-memory state (ZSet + LRU cache) is updated only after `commit()` returns successfully. If commit fails, in-memory state is left untouched and the error propagates to the caller.

`apply_batch` collapses N mutations into one `begin_write()` / `commit()` cycle — one fsync regardless of batch size.

### DbBackend Trait

`DbBackend` is object-safe (verified by `test_dyn_dbbackend_compiles`). All reads take `&self`; all writes take `&mut self`. A default `get_row_record_bytes` implementation returns `None`, allowing backends without a row cache to compile without implementing it.

---

## Key Design Decisions

1. **Sorted xxh64 hash index** — Field lookup is a binary search on a densely-packed `u64` array, not string comparison. Trade-off: field names cannot be recovered from the buffer. Callers must supply names at read time.

2. **32-field ArrayVec limit** — The per-record sort buffer lives entirely on the stack. Trade-off: records with more than 32 top-level fields return `RecordError::TooManyFields`. Raising the limit requires changing the `32` capacity constant in `serialize` and `read_all_index_entries`.

3. **ZSet always in memory** — All membership queries are pure memory operations. Trade-off: ZSet must be rebuilt from a full `RECORDS_TABLE` scan on startup — O(N) in record count (~20–80 ms per million records on SSD).

4. **LRU row cache bounded, cold on startup** — Prevents unbounded memory growth for large databases. Trade-off: cold reads after startup go to redb until records are written. For streaming pipelines (write before read in the same tick), cache hit rate is effectively 100% on the hot path.

5. **Write-through, not read-through** — Reads use `peek()` to satisfy `&self` on `DbBackend`. Eviction order reflects write time. Cache misses from reads do not populate the cache.

6. **Pre-serialize before `begin_write()`** — `DbMutation.data` carries pre-serialized bytes. All CPU-bound serialization happens before the redb write lock is acquired, minimising lock hold time.

7. **Flat key format `"table:id"`** — `make_key` builds a stack-allocated `ArrayString<512>`. `split_once(':')` at startup extracts table name and record ID. Table names that contain `':'` are rejected with `SpookyDbError::InvalidKey`. Record IDs may contain `':'`.

8. **FieldSlot staleness via `debug_assert`** — Zero overhead in release builds. Trade-off: a stale slot in release silently reads or writes the wrong field data. Callers must re-resolve slots after any layout-changing mutation.

---

## Data Flow

### Write Path (`apply_batch`)

1. Caller builds `Vec<DbMutation>` with pre-serialized `data` bytes.
2. `apply_batch` validates all table names.
3. Mutations are sorted by table name for cache locality.
4. One `db.begin_write()` opens a single redb write transaction; all inserts/removes execute inside it.
5. `write_txn.commit()`. On failure, return error — in-memory state unchanged.
6. Second pass: update ZSet and LRU cache for each mutation.
7. Return `BatchMutationResult` with deltas, content updates, and changed table names.

### Read Path (`get_record_bytes`)

1. ZSet guard — return `None` immediately if weight is 0 or absent.
2. LRU `peek()` — if hit, clone and return.
3. Cache miss — open redb read transaction, fetch, return cloned `Vec<u8>`. Cache not populated.

### Read Path (`get_row_record` — hot path)

1. ZSet guard.
2. LRU `peek()` — if hit, decode with `from_bytes`, return `SpookyRecord<'_>` borrowing the cache entry.
3. Cache miss — return `None`. No redb fallback.

### Startup Path

1. `RedbDatabase::create(path)` opens or creates the file.
2. A write transaction ensures `RECORDS_TABLE` and `VERSION_TABLE` exist.
3. `SpookyDb` is constructed with empty ZSets and a cold LRU cache.
4. `rebuild_from_records` scans `RECORDS_TABLE`, populating ZSets only (no cache pre-load).

---

## Technical Debt & Known Limitations

1. **32-field hard limit** — Change the `ArrayVec<..., 32>` capacity in `src/serialization.rs:307` and `src/spooky_record/migration_op.rs:161`.

2. **Field names not recoverable** — `to_value()` returns `SpookyValue::Null` for all inputs; it cannot be implemented without an external name table. `get_record_typed` requires callers to supply expected field names.

3. **FastMap name collision** — `spooky_value::FastMap` is `BTreeMap`; `db::types::FastMap` is FxHasher `HashMap`. Both are public. Use explicit module paths when both are needed.

4. **VERSION_TABLE not cached** — `get_version` opens a redb read transaction on every call. Hot version reads would benefit from an in-memory `FastMap<(table, id), u64>` cache.

5. **Per-record transaction in `apply_mutation`** — One fsync per call. Use `apply_batch` for high-throughput writes.

6. **LRU reads don't update recency** — Eviction order reflects write time, not read time. Frequently-read but infrequently-written records may be evicted while recently-written records they displace are not read.

7. **`set_str_at` cannot grow** — Returns `RecordError::LengthMismatch` on byte-length change. Fall back to `set_str(name, value)` + `resolve(name)`.

---

## Getting Started (Contributor View)

### Commands

```bash
cargo build
cargo build --release
cargo test
cargo test test_roundtrip_flat_fields   # run by name substring
cargo test -- --nocapture               # show println! output
cargo clippy

cargo bench
cargo bench --bench spooky_bench -- reading_values
cargo bench --bench spooky_bench -- fieldslot
cargo bench --bench spooky_bench -- buffer_reuse
cargo bench --bench spooky_bench -- --test   # smoke-test only
open target/criterion/report/index.html
```

### Navigation Guide

1. `src/types.rs` — wire format constants (`HEADER_SIZE`, `INDEX_ENTRY_SIZE`, `TAG_*`) and internal types (`IndexEntry`, `FieldRef`, `FieldSlot`, `FieldIter`)
2. `src/serialization.rs` — `RecordSerialize`, `serialize`, `serialize_into`, `write_field_into`
3. `src/spooky_record/read_op.rs` — `SpookyReadable` trait — all read patterns including the `FieldSlot` hot path
4. `src/spooky_record/write_op.rs` — in-place and splice mutation paths
5. `src/spooky_record/migration_op.rs` — `add_field`, `remove_field`, `rebuild_buffer_with`
6. `src/db/types.rs` — `DbMutation`, `Operation`, `BatchMutationResult`, `SpookyDbError`, `SpookyDbConfig`
7. `src/db/db.rs` — `SpookyDb`, `DbBackend`, all persistence logic

Unit tests: `src/spooky_record/tests/` (record-level) and inline in `src/db/db.rs` (~20 persistence tests).
Benchmarks: `benches/spooky_bench.rs`.

### Adding a New Field Type

1. Add `TAG_YOUR_TYPE: u8 = N` in `src/types.rs`.
2. Add dispatch in `write_field_into` (`src/serialization.rs`).
3. Add `RecordSerialize` method + implement for all three value types.
4. Add match arm in `decode_field` (`src/deserialization.rs`).
5. Add `RecordDeserialize` constructor + implement for all three value types.
6. Add typed getter/setter methods on `SpookyReadable` and `SpookyRecordMut`.
7. Add tests: round-trip, `_at` accessor, type mismatch error.
