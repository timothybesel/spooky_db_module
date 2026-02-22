# 👻 SpookyDB Module

[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://codspeed.io/timothybesel/spooky_db_module?utm_source=badge)

A high-performance, zero-copy binary record format for Rust with embedded persistence. SpookyDB serializes structured data into a compact hybrid format with **O(log n) field lookups**, **O(1) cached access via FieldSlots**, **nanosecond-level mutation**, and **transactional disk persistence** via redb — no parsing required until you access a field.

## Architecture

SpookyDB uses a **hybrid binary format** that combines native encoding for flat fields with CBOR for nested data. It abstracts over value types using the `RecordSerialize` and `RecordDeserialize` traits, allowing seamless interoperability between `SpookyValue`, `serde_json::Value`, and `cbor4ii::core::Value`. The persistence layer stores serialized records in redb with in-memory ZSets for zero-I/O membership queries.

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
│   └── ZSets (in-memory) ── FastMap<SmolStr, ZSet>                  │
│       • zero I/O membership queries                                │
│       • rebuilt from RECORDS_TABLE on startup                      │
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

## Usage

### Creating Records

```rust
use spooky_db_module::spooky_value::SpookyValue;
use spooky_db_module::serialization::{serialize, from_cbor};
use spooky_db_module::spooky_record::SpookyRecord;
use spooky_db_module::spooky_record_mut::SpookyRecordMut;
use serde_json::json;

// 1. Serialize from SpookyValue
let data = SpookyValue::from(json!({"name": "Alice", "age": 30}));
let (bytes, count) = serialize(&data.as_map().unwrap()).unwrap();

// 2. Immutable zero-copy access
let record = SpookyRecord::new(&bytes, count);
let name = record.get_str("name");                // Option<&str> — zero-copy
let age  = record.get_i64("age");                 // Option<i64>
let val: SpookyValue = record.get_field("age").unwrap(); // Generic get

// 3. Mutable in-place access
let mut rec = SpookyRecordMut::new(bytes, count);
rec.set_i64("age", 29).unwrap();                  // ~6 ns
rec.set_str("name", "Bobby").unwrap();            // ~13 ns (same len)

// 4. Generic Setters (works with any RecordSerialize type)
rec.add_field("active", &true).unwrap();          // generic boolean
rec.set_field("meta", &json!({"foo": "bar"})).unwrap(); // generic JSON
```

### FieldSlot Cached Access (O(1))

For hot paths where the same fields are read/written repeatedly (e.g. DBSP change detection), resolve a field once and access it via cached `FieldSlot` — **up to 14x faster** than by-name lookups:

```rust
// Resolve once — O(log n) binary search
let age_slot = rec.resolve("age").unwrap();

// Read via slot — O(1), no hashing, no search (~1 ns)
let age = rec.get_i64_at(&age_slot);       // Some(29)

// Write via slot — O(1), in-place (~0.6 ns)
rec.set_i64_at(&age_slot, 30).unwrap();
```

### Buffer Reuse for Bulk Serialization

Eliminate per-record heap allocations when serializing many records (**~17% faster**):

```rust
use spooky_db_module::serialization::serialize_into;

// Serialize thousands of records with one allocation
let mut buf = Vec::new();
for record in incoming_stream {
    serialize_into(&record, &mut buf)?;
    store.put(key, &buf);  // buf reused on next iteration
}
```

### SpookyRecord (Immutable)

| Method | Returns | Description |
|---|---|---|
| `get_str(name)` | `Option<&str>` | Zero-copy string access |
| `get_i64(name)` | `Option<i64>` | Read i64 field |
| `get_u64(name)` | `Option<u64>` | Read u64 field |
| `get_f64(name)` | `Option<f64>` | Read f64 field |
| `get_bool(name)` | `Option<bool>` | Read bool field |
| `get_field<V>(name)` | `Option<V>` | **Generic**: Deserialize any field to `V` |
| `get_raw(name)` | `Option<FieldRef>` | Raw field reference |
| `get_number_as_f64(name)` | `Option<f64>` | Any numeric → f64 |
| `has_field(name)` | `bool` | Existence check |
| `iter_fields()` | `FieldIter` | Iterate raw fields |
| `field_count()` | `u32` | Number of fields |

### SpookyRecordMut (Mutable)

| Method | Description |
|---|---|
| `new(Vec<u8>, usize)` | Create from existing buffer |
| `new_empty()` | Create empty record |
| **By-name access** | |
| `set_i64(name, val)` | In-place i64 overwrite |
| `set_u64(name, val)` | In-place u64 overwrite |
| `set_f64(name, val)` | In-place f64 overwrite |
| `set_bool(name, val)` | In-place bool overwrite |
| `set_str(name, val)` | String set (splice if needed) |
| `set_str_exact(name, val)` | Same-length string only |
| `set_field<V>(name, &V)` | **Generic**: Set any `RecordSerialize` value |
| `set_null(name)` | Set field to null |
| `add_field<V>(name, &V)` | **Generic**: Add new field |
| `remove_field(name)` | Remove field |
| **FieldSlot cached access** | |
| `resolve(name)` | Resolve field → `FieldSlot` |
| `get_*_at(&slot)` | O(1) cached read |
| `set_*_at(&slot, val)` | O(1) cached write |

### Supported Types

The module supports generic serialization via `RecordSerialize` and `RecordDeserialize`:

- `SpookyValue`: Dynamic value enum (Null, Bool, Number, Str, Array, Object)
- `serde_json::Value`: Standard JSON types
- `cbor4ii::core::Value`: Low-level CBOR types

## Persistence Layer

SpookyDb provides transactional disk persistence backed by [redb](https://github.com/cberner/redb), an embedded key-value store. Records are stored as pre-serialized SpookyRecord bytes with flat composite keys (`"table_name:record_id"`). ZSets (membership weight maps) are kept entirely in memory for zero-I/O view evaluation, and rebuilt from a full table scan on startup.

### Design Rules

> 1. **One write transaction per batch** — `apply_batch` groups N mutations into a single redb write transaction (one fsync), regardless of how many records or tables are touched.
> 2. **ZSets always in memory** — membership queries (`get_table_zset`, `get_zset_weight`) never touch disk. ZSets are rebuilt from `RECORDS_TABLE` on startup.
> 3. **Records always on disk** — record bytes live in redb. The ZSet acts as an O(1) guard: reads for absent records skip redb entirely.

Table names must not contain `':'`. Record IDs may contain `':'` (the key format uses `split_once` on the first `':'`).

### Write Path

Pre-serialize all records on the caller side (CPU work, no lock held), then submit the batch for a single transactional commit:

```rust
use spooky_db_module::db::{SpookyDb, DbMutation, Operation};
use spooky_db_module::serialization::serialize;
use spooky_db_module::spooky_value::SpookyValue;
use serde_json::json;

// Open or create database
let mut db = SpookyDb::new("my_data.redb").unwrap();

// Pre-serialize records (CPU work, no lock)
let val = SpookyValue::from(json!({"name": "Alice", "age": 30, "spooky_rv": 1}));
let (bytes, _count) = serialize(&val.as_map().unwrap()).unwrap();

// Build a batch of mutations
let mutations = vec![
    DbMutation {
        table: "users".into(),
        id: "user:abc123".into(),
        op: Operation::Create,
        data: Some(bytes),
        version: Some(1),
    },
    // ... more mutations
];

// Single transaction, single fsync
let result = db.apply_batch(mutations).unwrap();
// result.membership_deltas  — per-table ZSet weight changes
// result.content_updates    — per-table sets of updated IDs
// result.changed_tables     — list of affected table names
```

### Read Path

ZSet-guarded reads skip redb entirely when a record is absent:

```rust
use spooky_db_module::serialization::from_bytes;
use spooky_db_module::spooky_record::SpookyRecord;

// Zero I/O — pure memory lookup for view evaluation
let zset = db.get_table_zset("users");

// ZSet-guarded read — O(1) check before touching disk
if let Some(bytes) = db.get_record_bytes("users", "user:abc123").unwrap() {
    let (buf, count) = from_bytes(&bytes).unwrap();
    let record = SpookyRecord::new(buf, count);
    let name = record.get_str("name");   // Option<&str>, zero-copy
    let age  = record.get_i64("age");    // Option<i64>
}

// Partial reconstruction — only named fields are recovered
let partial = db.get_record_typed("users", "user:abc123", &["name", "age"]).unwrap();
```

### SpookyDb API

#### Construction

| Method | Description |
|---|---|
| `SpookyDb::new(path)` | Open/create database, initialize tables, rebuild ZSets from disk (O(N) startup scan) |

#### Write Operations (`&mut self`)

| Method | Description |
|---|---|
| `apply_mutation(table, op, id, data, version)` | Single record + ZSet update in one transaction |
| `apply_batch(mutations: Vec<DbMutation>)` | **N records in ONE transaction (one fsync)** — the critical performance path |
| `bulk_load(records)` | Initial hydration from an iterator of `BulkRecord`, single transaction |

#### Read Operations (`&self`)

| Method | Description |
|---|---|
| `get_record_bytes(table, id)` | ZSet-guarded raw bytes fetch; returns `None` without touching redb if absent |
| `get_record_typed(table, id, fields)` | Partial field reconstruction; only the named fields are recovered |
| `get_version(table, id)` | Read the version number for a record |

#### ZSet Operations (pure memory, zero I/O)

| Method | Description |
|---|---|
| `get_table_zset(table)` | Full `&ZSet` borrow for view evaluation (Scan operator) |
| `get_zset_weight(table, id)` | Membership weight; 0 if absent |
| `apply_zset_delta_memory(table, delta)` | In-memory delta application for checkpoint recovery |

#### Table Info (pure memory, O(1))

| Method | Description |
|---|---|
| `table_exists(table)` | Check if a table has been registered |
| `table_names()` | Iterator over all registered table names |
| `table_len(table)` | Number of records with positive ZSet weight |
| `ensure_table(table)` | Register an empty table before first insert |

### Supporting Types

**`Operation`** — the mutation kind for each record in a batch:

| Variant | ZSet Effect | Record Effect |
|---|---|---|
| `Create` | weight += 1 | Insert record bytes |
| `Update` | weight unchanged | Replace record bytes |
| `Delete` | weight -= 1 | Remove record bytes |

**`DbMutation`** — a single unit of work within a batch:

| Field | Type | Description |
|---|---|---|
| `table` | `SmolStr` | Target table name |
| `id` | `SmolStr` | Record identifier |
| `op` | `Operation` | Create, Update, or Delete |
| `data` | `Option<Vec<u8>>` | Pre-serialized SpookyRecord bytes; `None` for Delete |
| `version` | `Option<u64>` | Explicit version number; `None` = leave unchanged |

**`BatchMutationResult`** — returned by `apply_batch`:

| Field | Type | Description |
|---|---|---|
| `membership_deltas` | `FastMap<SmolStr, ZSet>` | Per-table ZSet weight deltas |
| `content_updates` | `FastMap<SmolStr, FastHashSet<SmolStr>>` | Per-table set of updated record IDs |
| `changed_tables` | `Vec<SmolStr>` | List of all affected table names |

**`BulkRecord`** — used by `bulk_load` for initial hydration:

| Field | Type | Description |
|---|---|---|
| `table` | `SmolStr` | Target table name |
| `id` | `SmolStr` | Record identifier |
| `data` | `Vec<u8>` | Pre-serialized SpookyRecord bytes |

### DbBackend Trait

`SpookyDb` implements the `DbBackend` trait, which abstracts over the storage backend. This allows swapping between on-disk persistence and alternative backends (e.g., pure in-memory for testing) without changing caller code:

```rust
pub trait DbBackend {
    fn get_table_zset(&self, table: &str) -> Option<&ZSet>;
    fn get_record_bytes(&self, table: &str, id: &str) -> Option<Vec<u8>>;
    fn ensure_table(&mut self, table: &str);
    fn apply_mutation(&mut self, ...) -> Result<(SmolStr, i64), SpookyDbError>;
    fn apply_batch(&mut self, mutations: Vec<DbMutation>) -> Result<BatchMutationResult, SpookyDbError>;
    fn bulk_load(&mut self, records: impl IntoIterator<Item=BulkRecord>) -> Result<(), SpookyDbError>;
    fn get_zset_weight(&self, table: &str, id: &str) -> i64;
}
```

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
| `get_i64` | **9.13 ns** | **1.48 ns** | 6.2× |
| `get_str` | **9.55 ns** | **3.87 ns** | 2.5× |
| `get_bool` | **9.10 ns** | **0.94 ns** | 9.7× |
| `get_f64` | **11.65 ns** | **1.00 ns** | 11.6× |
| `set_i64` | **8.84 ns** | **0.64 ns** | 13.8× |
| `set_str` (same len) | **9.74 ns** | **4.37 ns** | 2.2× |

### Buffer Reuse: Bulk Serialization

`serialize_into` and `from_spooky_value_into` reuse a caller-provided `Vec<u8>`, clearing it but retaining its heap allocation. This eliminates the per-record `Vec::new()` + allocation cost that dominates when serializing many records in sequence (sync ingestion, snapshot rebuild). The buffer naturally grows to the high-water mark and stays there.

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

# Quick smoke test
cargo bench --bench spooky_bench -- --test

# View HTML reports
open target/criterion/report/index.html
```

## Dependencies

| Crate | Purpose |
|---|---|
| `redb` | Embedded transactional key-value store for record persistence |
| `rustc-hash` | FxHasher-based `HashMap`/`HashSet` for in-memory ZSets |
| `cbor4ii` | Fast, zero-copy CBOR encoding/decoding |
| `xxhash-rust` | Fast 64-bit hashing for field name lookups |
| `smol_str` | Small-string-optimized string type |
| `serde` | Serialization framework |
| `serde_json` | JSON support |

## License

MIT
