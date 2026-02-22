# SpookyDB Module

[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://codspeed.io/timothybesel/spooky_db_module?utm_source=badge)

Structured records that are slow to serialize and expensive to parse at scale make streaming pipelines bottleneck on I/O and allocation — SpookyDB stores records in a binary format that requires no parsing before field access, keeps membership state in memory for zero-I/O view queries, and persists to an embedded ACID store with one fsync per batch.

**What it does**: O(log n) field lookups with no parsing, O(1) cached access via FieldSlots, nanosecond-level in-place mutation, ZSet-guarded reads that skip disk for absent records, and an LRU row cache that serves recently-written records with zero I/O.

**What makes it different**: The binary format separates the sorted hash index from field data, so typed reads are a hash lookup + two-pointer dereference on a `&[u8]` — no serde, no enum traversal, no allocation. All three value types (`SpookyValue`, `serde_json::Value`, `cbor4ii::core::Value`) serialize to the same format without intermediate conversion.

---

## Architecture

SpookyDB uses a **hybrid binary format** that combines native encoding for flat fields with CBOR for nested data. It abstracts over value types using the `RecordSerialize` and `RecordDeserialize` traits, allowing seamless interoperability between `SpookyValue`, `serde_json::Value`, and `cbor4ii::core::Value`. The persistence layer stores serialized records in redb with in-memory ZSets for zero-I/O membership queries and a bounded LRU row cache for zero-I/O reads on recently-written records.

```
   ┌─────────────────────────────────────────────────────────────┐
   │  Generic Values (RecordSerialize / RecordDeserialize)       │
   │  ├── SpookyValue                                            │
   │  ├── serde_json::Value                                      │
   │  └── cbor4ii::core::Value                                   │
   └──────────────┬──────────────────────────────────────────────┘
                  │                              ▲
        serialization::serialize                 │
                  │                  deserialization::decode_field
                  ▼                              │
┌────────────────────────────────────────────────┴───────────────────┐
│   ┌────────────────────────────┐  ┌────────────────────────────┐   │
│   │     SpookyRecord<'a>       │  │     SpookyRecordMut        │   │
│   │     (immutable, &[u8])     │  │     (mutable, Vec<u8>)     │   │
│   │     • zero-copy reads      │  │     • in-place updates     │   │
│   │     • no allocations       │  │     • add/remove fields    │   │
│   │     • Copy trait           │  │     • generic setters      │   │
│   └────────────────────────────┘  └────────────────────────────┘   │
└──────────────────────────┬─────────────────────────────────────────┘
                           │
                  SpookyDb::apply_batch / get_record_bytes
                           │
                           ▼
┌────────────────────────────────────────────────────────────────────┐
│   SpookyDb (Persistence Layer)                                     │
│   ├── RECORDS_TABLE  ── redb ── "table:id" → &[u8]                 │
│   ├── VERSION_TABLE  ── redb ── "table:id" → u64                   │
│   ├── ZSets (in-memory) ── FastMap<SmolStr, ZSet>                  │
│   │   • zero I/O membership queries                                │
│   │   • rebuilt from RECORDS_TABLE on startup                      │
│   └── LRU row cache ── bounded in-memory Vec<u8> per record        │
│       • write-through on Create/Update/bulk_load                   │
│       • cache miss falls back to redb                              │
└────────────────────────────────────────────────────────────────────┘
```

### Binary Format

```
┌────────────────────── Header (20 bytes) ──────────────────┐
│  field_count: u32 (LE)  |  reserved: [u8; 16]             │
├────────────────── Index (20 bytes × N) ───────────────────┤
│  name_hash:   u64 (LE)   ← SORTED for binary search       │
│  data_offset: u32 (LE)                                    │
│  data_length: u32 (LE)                                    │
│  type_tag:    u8                                          │
│  _padding:    [u8; 3]                                     │
├──────────────────── Field Data ───────────────────────────┤
│  Flat types: native LE bytes (i64, u64, f64, bool)        │
│  Strings: raw UTF-8 bytes                                 │
│  Nested objects/arrays: CBOR-encoded                      │
└───────────────────────────────────────────────────────────┘
```

Field names are hashed with xxh64 and stored in the sorted index. The names themselves are not stored — they cannot be recovered from a serialized record without an external schema. This is a deliberate performance trade-off: field lookups are a hash + binary search on a `u64` slice with no string comparisons.

> See also: [Architecture](docs/ARCHITECTURE.md) | [Full API Reference](docs/API.md)

---

## Quick Start

### Installation

Add to `Cargo.toml`:

```toml
[dependencies]
spooky_db_module = { path = "..." }
```

**What SpookyDb does to your system**: opening a database creates or opens a single `.redb` file at the path you provide. No other files are written. No background threads are spawned. No network connections are made. To remove all data, delete the `.redb` file. No special permissions beyond normal file I/O are required.

---

## Usage

### Creating Records

```rust
use spooky_db_module::serialization::{serialize, from_bytes};
use spooky_db_module::spooky_record::SpookyRecord;
use spooky_db_module::spooky_record::read_op::SpookyReadable;
use spooky_db_module::spooky_record::record_mut::SpookyRecordMut;
use spooky_db_module::spooky_value::SpookyValue;
use smol_str::SmolStr;
use std::collections::BTreeMap;

// 1. Serialize from a BTreeMap<SmolStr, V> where V: RecordSerialize
let mut map = BTreeMap::new();
map.insert(SmolStr::new("name"), SpookyValue::Str(SmolStr::new("Alice")));
map.insert(SmolStr::new("age"),  SpookyValue::Number(30i64.into()));
let (bytes, count) = serialize(&map).unwrap();

// 2. Immutable zero-copy access (borrows the buffer, implements Copy)
let record = SpookyRecord::new(&bytes, count);
let name = record.get_str("name");           // Option<&str> — zero-copy
let age  = record.get_i64("age");            // Option<i64>
let val: Option<SpookyValue> = record.get_field("age"); // generic

// 3. Mutable in-place access
let mut rec = SpookyRecordMut::new(bytes.clone(), count);
rec.set_i64("age", 29).unwrap();             // ~6 ns
rec.set_str("name", "Bobby").unwrap();       // ~13 ns (same length)

// 4. Generic setters (any RecordSerialize type)
rec.add_field("active", &true).unwrap();
```

### FieldSlot Cached Access (O(1))

For hot paths where the same fields are read or written repeatedly (e.g. DBSP change detection), resolve a field once and access it via a cached `FieldSlot` — up to 14x faster than by-name lookups:

```rust
use spooky_db_module::spooky_record::read_op::SpookyReadable;
use spooky_db_module::spooky_record::record_mut::SpookyRecordMut;

// Resolve once — O(log n) binary search, caches offset + type tag
let age_slot = rec.resolve("age").unwrap();

// Read via slot — O(1), no hashing, no search (~1.5 ns)
let age = rec.get_i64_at(&age_slot);         // Some(29)

// Write via slot — O(1), in-place (~0.6 ns)
rec.set_i64_at(&age_slot, 30).unwrap();

// Slots are invalidated by structural mutations (add_field, remove_field,
// variable-length string splice). Staleness is caught by debug_assert in
// all _at methods — zero overhead in release builds.
```

### Buffer Reuse for Bulk Serialization

Eliminate per-record heap allocations when serializing many records (~17% faster):

```rust
use spooky_db_module::serialization::serialize_into;
use smol_str::SmolStr;
use std::collections::BTreeMap;

// One allocation; reused across all records
let mut buf = Vec::new();
for map in incoming_stream {
    let _field_count = serialize_into(&map, &mut buf).unwrap();
    store.put(key, &buf); // buf is reused on the next iteration
}
```

### SpookyRecord (Immutable)

`SpookyRecord<'a>` borrows `&'a [u8]` and implements `Copy`. All read methods come from the `SpookyReadable` trait.

| Method | Returns | Description |
|---|---|---|
| `get_str(name)` | `Option<&str>` | Zero-copy string access |
| `get_i64(name)` | `Option<i64>` | Read i64 field |
| `get_u64(name)` | `Option<u64>` | Read u64 field |
| `get_f64(name)` | `Option<f64>` | Read f64 field |
| `get_bool(name)` | `Option<bool>` | Read bool field |
| `get_field::<V>(name)` | `Option<V>` | Generic: deserialize any field to `V` |
| `get_raw(name)` | `Option<FieldRef>` | Raw field reference (zero-copy) |
| `get_number_as_f64(name)` | `Option<f64>` | Any numeric type promoted to f64 |
| `has_field(name)` | `bool` | Existence check |
| `field_type(name)` | `Option<u8>` | Raw type tag |
| `iter_fields()` | `FieldIter` | Iterate all raw fields |
| `field_count()` | `usize` | Number of fields |
| `resolve(name)` | `Option<FieldSlot>` | Cache field position for O(1) future access |
| `get_i64_at(&slot)` | `Option<i64>` | O(1) cached read |
| `get_u64_at(&slot)` | `Option<u64>` | O(1) cached read |
| `get_f64_at(&slot)` | `Option<f64>` | O(1) cached read |
| `get_bool_at(&slot)` | `Option<bool>` | O(1) cached read |
| `get_str_at(&slot)` | `Option<&str>` | O(1) cached zero-copy read |

### SpookyRecordMut (Mutable)

`SpookyRecordMut` owns `Vec<u8>` and carries a `generation` counter. Fixed-width writes leave the generation unchanged. Structural mutations (add/remove field, variable-length string splice) increment it, invalidating any outstanding `FieldSlot`.

| Method | Description |
|---|---|
| `new(Vec<u8>, usize)` | Create from existing buffer |
| `new_empty()` | Create empty record |
| `as_record()` | Zero-copy `SpookyRecord<'_>` view over the mutable buffer |
| **By-name writes** | |
| `set_i64(name, val)` | In-place i64 overwrite |
| `set_u64(name, val)` | In-place u64 overwrite |
| `set_f64(name, val)` | In-place f64 overwrite |
| `set_bool(name, val)` | In-place bool overwrite |
| `set_str(name, val)` | In-place if same byte length, splice if different |
| `set_str_exact(name, val)` | Same-length only; returns `LengthMismatch` otherwise |
| `set_field::<V>(name, &V)` | Generic: set any `RecordSerialize` value |
| `set_null(name)` | Set field to null |
| `add_field::<V>(name, &V)` | Generic: add a new field (full buffer rebuild) |
| `remove_field(name)` | Remove field (full buffer rebuild) |
| **FieldSlot cached access** | |
| `resolve(name)` | Resolve field into a `FieldSlot` |
| `get_*_at(&slot)` | O(1) cached read (inherited from `SpookyReadable`) |
| `set_i64_at(&slot, val)` | O(1) cached write |
| `set_u64_at(&slot, val)` | O(1) cached write |
| `set_f64_at(&slot, val)` | O(1) cached write |
| `set_bool_at(&slot, val)` | O(1) cached write |
| `set_str_at(&slot, val)` | O(1) same-length write; returns `LengthMismatch` on length change |

### Supported Types

`RecordSerialize` and `RecordDeserialize` are implemented for all three value types — records can be written from or read into any of them without intermediate conversion:

- `SpookyValue`: native dynamic enum (`Null`, `Bool`, `Number(SpookyNumber)`, `Str(SmolStr)`, `Array`, `Object`)
- `serde_json::Value`: standard JSON types
- `cbor4ii::core::Value`: low-level CBOR types

---

## Persistence Layer

SpookyDb provides transactional disk persistence backed by [redb](https://github.com/cberner/redb), an embedded key-value store. Records are stored as pre-serialized SpookyRecord bytes with flat composite keys (`"table_name:record_id"`). ZSets (membership weight maps) live entirely in memory for zero-I/O view evaluation and are rebuilt from a full table scan on startup. A bounded LRU row cache serves recently-written records with zero I/O.

### Design Rules

> 1. **One write transaction per batch** — `apply_batch` groups N mutations into a single redb write transaction (one fsync), regardless of how many records or tables are touched.
> 2. **ZSets always in memory** — membership queries (`get_table_zset`, `get_zset_weight`) never touch disk. ZSets are rebuilt from `RECORDS_TABLE` on startup.
> 3. **LRU row cache** — recently written records are served from a bounded in-memory LRU cache (default 10 000 records). Cache misses fall back to redb. `get_row_record` returns `None` on miss — it is not guaranteed to return bytes if a record exists but has been evicted.

Table names must not contain `':'`. Record IDs may contain `':'` (the key format uses `split_once` on the first `':'`).

### Write Path

Pre-serialize all records on the caller side (CPU work, no lock held), then submit the batch for a single transactional commit:

```rust
use spooky_db_module::db::db::SpookyDb;
use spooky_db_module::db::types::{DbMutation, Operation};
use spooky_db_module::serialization::serialize;
use spooky_db_module::spooky_value::SpookyValue;
use smol_str::SmolStr;
use std::collections::BTreeMap;

// Open or create database (default cache: 10 000 records)
let mut db = SpookyDb::new("my_data.redb").unwrap();

// Pre-serialize records (CPU work, no lock)
let mut map = BTreeMap::new();
map.insert(SmolStr::new("name"), SpookyValue::Str(SmolStr::new("Alice")));
map.insert(SmolStr::new("age"),  SpookyValue::Number(30i64.into()));
let (bytes, _count) = serialize(&map).unwrap();

// Build a batch of mutations
let mutations = vec![
    DbMutation {
        table: SmolStr::new("users"),
        id: SmolStr::new("user:abc123"),
        op: Operation::Create,
        data: Some(bytes),
        version: Some(1),
    },
];

// Single transaction, single fsync
let result = db.apply_batch(mutations).unwrap();
// result.membership_deltas  — per-table ZSet weight changes
// result.content_updates    — per-table sets of updated IDs
// result.changed_tables     — list of affected table names
```

### Read Path

For the write-then-read pipeline hot path, records written in the same tick are always in the LRU cache — zero I/O, zero allocation:

```rust
use spooky_db_module::db::db::SpookyDb;
use spooky_db_module::serialization::from_bytes;
use spooky_db_module::spooky_record::SpookyRecord;
use spooky_db_module::spooky_record::read_op::SpookyReadable;

// Zero I/O — pure memory ZSet lookup for view evaluation
let zset = db.get_table_zset("users");

// Fast path: record in LRU cache — zero I/O, borrowed SpookyRecord<'_>
if let Some(record) = db.get_row_record("users", "user:abc123") {
    let name = record.get_str("name"); // Option<&str>, zero-copy
    let age  = record.get_i64("age");  // Option<i64>
}

// Fallback: cache miss — ZSet guard → redb read, returns owned Vec<u8>
if let Some(bytes) = db.get_record_bytes("users", "user:abc123") {
    let (buf, count) = from_bytes(&bytes).unwrap();
    let record = SpookyRecord::new(buf, count);
    let age = record.get_i64("age");
}

// Partial reconstruction — supply field names; they are not stored in the binary format
let partial = db
    .get_record_typed("users", "user:abc123", &["name", "age"])
    .unwrap();
```

### SpookyDb API

#### Construction

| Method | Description |
|---|---|
| `SpookyDb::new(path)` | Open/create database with default config (10 000 record LRU cache). Rebuilds all ZSets from `RECORDS_TABLE` on startup — O(N records). Cache starts cold. |
| `SpookyDb::new_with_config(path, SpookyDbConfig)` | Open/create with explicit configuration (e.g. custom `cache_capacity`). |

#### Write Operations (`&mut self`)

| Method | Signature | Description |
|---|---|---|
| `apply_mutation` | `(table, op, id, data: Option<&[u8]>, version: Option<u64>) -> Result<(SmolStr, i64), SpookyDbError>` | Single record + ZSet update in one transaction |
| `apply_batch` | `(mutations: Vec<DbMutation>) -> Result<BatchMutationResult, SpookyDbError>` | **N records in ONE transaction (one fsync)** — the critical performance path |
| `bulk_load` | `(records: Vec<BulkRecord>) -> Result<(), SpookyDbError>` | Initial hydration — all records in one transaction; sets every ZSet weight to 1 |

#### Read Operations (`&self`)

| Method | Returns | Description |
|---|---|---|
| `get_row_record(table, id)` | `Option<SpookyRecord<'_>>` | Zero-copy borrowed record. Cache-only: returns `None` on cache miss even if the record exists on disk. |
| `get_record_bytes(table, id)` | `Option<Vec<u8>>` | ZSet guard → LRU peek → redb fallback on miss. Never returns bytes for a deleted or absent record. |
| `get_record_typed(table, id, fields: &[&str])` | `Result<Option<SpookyValue>, SpookyDbError>` | Partial field reconstruction; only the named fields are recovered (names are not stored in the binary format). |
| `get_version(table, id)` | `Result<Option<u64>, SpookyDbError>` | Read the stored version number for a record |

#### ZSet Operations (pure memory, zero I/O)

| Method | Description |
|---|---|
| `get_table_zset(table)` | Full `&ZSet` borrow for view evaluation (Scan operator) |
| `get_zset_weight(table, id)` | Membership weight; 0 if absent |

#### Table Info (pure memory, O(1))

| Method | Description |
|---|---|
| `table_exists(table)` | `true` if the table has at least one record with positive ZSet weight |
| `table_names()` | Iterator over all registered table names |
| `table_len(table)` | Number of records with positive ZSet weight |
| `ensure_table(table)` | Pre-allocate the ZSet slot before bulk operations. Returns `Err(InvalidKey)` if table name contains `':'`. |

### Supporting Types

**`SpookyDbConfig`** — passed to `SpookyDb::new_with_config`:

| Field | Type | Default | Description |
|---|---|---|---|
| `cache_capacity` | `NonZeroUsize` | `10_000` | Maximum records in the LRU row cache. Records beyond this limit are evicted and re-read from redb on demand. |

**`Operation`** — the mutation kind for each record in a batch:

| Variant | ZSet Effect | Record Effect |
|---|---|---|
| `Create` | weight += 1 | Insert record bytes |
| `Update` | weight unchanged | Replace record bytes |
| `Delete` | weight -= 1 | Remove record bytes |

**`DbMutation`** — a single unit of work within a batch:

| Field | Type | Description |
|---|---|---|
| `table` | `SmolStr` | Target table name (must not contain `':'`) |
| `id` | `SmolStr` | Record identifier |
| `op` | `Operation` | Create, Update, or Delete |
| `data` | `Option<Vec<u8>>` | Pre-serialized SpookyRecord bytes; `None` for Delete |
| `version` | `Option<u64>` | Explicit version number; `None` = leave existing version unchanged |

**`BatchMutationResult`** — returned by `apply_batch`:

| Field | Type | Description |
|---|---|---|
| `membership_deltas` | `FastMap<SmolStr, ZSet>` | Per-table ZSet weight deltas |
| `content_updates` | `FastMap<SmolStr, FastHashSet<SmolStr>>` | Per-table set of record IDs whose bytes were written |
| `changed_tables` | `Vec<SmolStr>` | All tables with at least one mutation (deduplicated) |

**`BulkRecord`** — used by `bulk_load` for initial hydration:

| Field | Type | Description |
|---|---|---|
| `table` | `SmolStr` | Target table name |
| `id` | `SmolStr` | Record identifier |
| `data` | `Vec<u8>` | Pre-serialized SpookyRecord bytes |
| `version` | `Option<u64>` | Written to `VERSION_TABLE` when `Some`; skipped when `None` |

### DbBackend Trait

`SpookyDb` implements the `DbBackend` trait, which abstracts over the storage backend. This allows swapping between on-disk persistence and alternative backends (e.g. pure in-memory for testing) without changing caller code. The trait is object-safe — `Box<dyn DbBackend>` compiles.

```rust
pub trait DbBackend {
    fn get_table_zset(&self, table: &str) -> Option<&ZSet>;
    fn get_record_bytes(&self, table: &str, id: &str) -> Option<Vec<u8>>;
    fn get_row_record_bytes<'a>(&'a self, table: &str, id: &str) -> Option<&'a [u8]>;
    fn ensure_table(&mut self, table: &str) -> Result<(), SpookyDbError>;
    fn apply_mutation(
        &mut self,
        table: &str,
        op: Operation,
        id: &str,
        data: Option<&[u8]>,
        version: Option<u64>,
    ) -> Result<(SmolStr, i64), SpookyDbError>;
    fn apply_batch(
        &mut self,
        mutations: Vec<DbMutation>,
    ) -> Result<BatchMutationResult, SpookyDbError>;
    fn bulk_load(
        &mut self,
        records: Vec<BulkRecord>,
    ) -> Result<(), SpookyDbError>;
    fn get_zset_weight(&self, table: &str, id: &str) -> i64;
    fn get_record_typed(
        &self,
        table: &str,
        id: &str,
        fields: &[&str],
    ) -> Result<Option<SpookyValue>, SpookyDbError>;
}
```

---

## Benchmarks

Measured with [Criterion.rs](https://github.com/bheisler/criterion.rs).

### Test Data

The benchmark uses a **341-byte CBOR payload** with 12 top-level fields covering every supported type:

```json
{
  "id": "user:abc123",           // string
  "name": "Alice",               // string
  "age": 28,                     // i64
  "count": 1000,                 // u64
  "score": 99.5,                 // f64
  "active": true,                // bool
  "deleted": false,              // bool
  "metadata": null,              // null
  "tags": ["developer", "rust", "database"],                 // array of strings
  "profile": {                                                // nested object (3 levels deep)
    "bio": "Software engineer",
    "avatar": "https://example.com/avatar.jpg",
    "settings": {
      "theme": "dark",
      "notifications": true,
      "privacy": { "public": false, "level": 3 }
    }
  },
  "history": [                                                // array of objects
    {"action": "login", "timestamp": 1234567890},
    {"action": "update", "timestamp": 1234567900}
  ],
  "mixed_array": [42, "text", true, {"nested": "value"}]     // mixed-type array
}
```

| Format | Size |
|---|---|
| CBOR input | **341 bytes** |
| SpookyRecord binary | **~580 bytes** (header + sorted index + field data) |

### Creating Records

| Operation | Median | Throughput |
|---|---|---|
| `SpookyRecord::serialize` | **3.90 µs** | ~256K records/sec |
| `SpookyRecordMut::from_spooky_value` | **3.84 µs** | ~260K records/sec |
| `SpookyRecordMut::new_empty` | **17.18 ns** | ~58.2M records/sec |
| `SpookyRecordMut::from_vec` | **41.21 ns** | ~24.3M records/sec |

### Reading Values

| Operation | Median | Throughput | Allocs |
|---|---|---|---|
| `SpookyRecord::get_str` | **10.60 ns** | ~94.3M reads/sec | 0 |
| `SpookyRecord::get_i64` | **10.62 ns** | ~94.2M reads/sec | 0 |
| `SpookyRecord::get_bool` | **9.84 ns** | ~101.7M reads/sec | 0 |
| `SpookyRecord::get_field` | **30.92 ns** | ~32.3M reads/sec | 1 |
| `SpookyRecordMut::get_str` | **9.53 ns** | ~104.9M reads/sec | 0 |
| `SpookyRecordMut::get_i64` | **9.02 ns** | ~110.9M reads/sec | 0 |
| `SpookyRecordMut::get_u64` | **9.08 ns** | ~110.1M reads/sec | 0 |
| `SpookyRecordMut::get_f64` | **11.97 ns** | ~83.6M reads/sec | 0 |
| `SpookyRecordMut::get_bool` | **9.10 ns** | ~109.9M reads/sec | 0 |
| `SpookyRecordMut::get_field` | **31.72 ns** | ~31.5M reads/sec | 1 |

### Setting Values

| Operation | Median | Throughput | Description |
|---|---|---|---|
| `set_i64` | **6.44 ns** | ~155.3M writes/sec | In-place overwrite |
| `set_u64` | **8.46 ns** | ~118.2M writes/sec | In-place overwrite |
| `set_f64` | **6.53 ns** | ~153.2M writes/sec | In-place overwrite |
| `set_bool` | **8.16 ns** | ~122.5M writes/sec | In-place overwrite |
| `set_str` (same len) | **13.17 ns** | ~75.9M writes/sec | In-place overwrite |
| `set_str` (diff len) | **27.88 ns** | ~35.9M writes/sec | Splice + fixup |
| `set_str_exact` | **11.82 ns** | ~84.6M writes/sec | Same-length guaranteed |
| `set_field` | **26.26 ns** | ~38.1M writes/sec | Generic path |
| `set_null` | **10.07 ns** | ~99.3M writes/sec | In-place overwrite |

### FieldSlot: Cached Access vs By-Name

FieldSlots eliminate the O(log n) binary search by caching the resolved field position. The slot stores the data offset, length, and type tag from the initial `resolve()` call. Subsequent `_at` accessors skip hashing and searching entirely — they index directly into the buffer.

A `generation` counter on `SpookyRecordMut` tracks layout changes. Fixed-width writes (`set_i64`, `set_bool`, same-length `set_str`) don't change layout, so slots remain valid. Structural mutations (`add_field`, `remove_field`, variable-length splice) bump the generation, invalidating all outstanding slots. Staleness is caught by `debug_assert` in `_at` methods — zero overhead in release builds.

| Operation | By Name | FieldSlot | Speedup |
|---|---|---|---|
| `get_i64` | **9.13 ns** | **1.48 ns** | 6.2x |
| `get_str` | **9.55 ns** | **3.87 ns** | 2.5x |
| `get_bool` | **9.10 ns** | **0.94 ns** | 9.7x |
| `get_f64` | **11.65 ns** | **1.00 ns** | 11.6x |
| `set_i64` | **8.84 ns** | **0.64 ns** | 13.8x |
| `set_str` (same len) | **9.74 ns** | **4.37 ns** | 2.2x |

### Buffer Reuse: Bulk Serialization

`serialize_into` reuses a caller-provided `Vec<u8>`, clearing it but retaining its heap allocation. This eliminates the per-record `Vec::new()` + allocation cost that dominates when serializing many records in sequence (sync ingestion, snapshot rebuild). The buffer naturally grows to the high-water mark and stays there.

| Operation | Fresh Alloc | Reused Buffer | Improvement |
|---|---|---|---|
| `serialize` | **528.30 ns** | **440.29 ns** | 17% faster |
| `from_spooky_value` | **519.85 ns** | **429.34 ns** | 17% faster |

### Field Migration

| Operation | Median | Throughput | Description |
|---|---|---|---|
| `add_field` | **191.18 ns** | ~5.2M ops/sec | Rebuild with sorted insertion |
| `remove_field` | **146.28 ns** | ~6.8M ops/sec | Rebuild without field |

### Throughput Summary

```
  ┌──────────────────────────────────────────────────────────────────────┐
  │ Operation              │ Speed              │ Category               │
  ├────────────────────────┼────────────────────┼────────────────────────┤
  │ FieldSlot reads        │ ~670M-1.06B ops/s  │ O(1) cached, 0 allocs  │
  │ FieldSlot writes       │ ~228M-1.56B ops/s  │ O(1) cached, 0 allocs  │
  │ Typed reads (by name)  │ ~86-111M ops/s     │ O(log n), 0 allocs     │
  │ In-place sets          │ ~118-155M ops/s    │ Zero-alloc overwrites  │
  │ String splice          │ ~36-85M ops/s      │ Buffer resize          │
  │ Add/Remove field       │ ~5-7M ops/s        │ Full rebuild           │
  │ Serialize (reuse buf)  │ ~2.3M recs/s       │ Buffer reuse           │
  │ Serialize (fresh)      │ ~1.9M recs/s       │ Allocates per record   │
  └──────────────────────────────────────────────────────────────────────┘
```

### Run Benchmarks

```bash
# All benchmarks
cargo bench

# Specific group
cargo bench --bench spooky_bench -- reading_values
cargo bench --bench spooky_bench -- fieldslot
cargo bench --bench spooky_bench -- buffer_reuse

# Quick smoke test (no timing, just correctness)
cargo bench --bench spooky_bench -- --test

# View HTML reports
open target/criterion/report/index.html
```

---

## Dependencies

| Crate | Version | Purpose |
|---|---|---|
| `redb` | `3.1.0` | Embedded transactional key-value store for record persistence |
| `lru` | `0.12` | Bounded LRU row cache for zero-I/O reads on recently-written records |
| `rustc-hash` | `2.1.1` | FxHasher-based `HashMap`/`HashSet` for in-memory ZSets |
| `cbor4ii` | `1.2.2` | Fast, zero-copy CBOR encoding/decoding |
| `xxhash-rust` | `0.8.15` | Fast 64-bit hashing for field name lookups |
| `smol_str` | `0.3.5` | Small-string-optimized string type |
| `arrayvec` | `0.7.6` | Stack-allocated sort buffer (caps records at 32 fields) |
| `serde` | `1.0` | Serialization framework |
| `serde_json` | `1.0` | JSON support |
| `thiserror` | `1` | Error type derivation |

---

## License

MIT
