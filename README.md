# 🎃 SpookyDB Module

[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://codspeed.io/timothybesel/spooky_db_module?utm_source=badge)

A high-performance, zero-copy binary record format for Rust. SpookyDB serializes structured data into a compact hybrid format with **O(log n) field lookups** and **nanosecond-level mutation** — no parsing required until you access a field.

## Architecture

```
┌──────────────────────────────────────────────────────┐
│  SpookyValue (in-memory)                             │
│  ├── Null, Bool, Number(i64/u64/f64), Str            │
│  ├── Array(Vec<SpookyValue>)                         │
│  └── Object(BTreeMap<SmolStr, SpookyValue>)           │
└──────────────┬───────────────────┬───────────────────┘
               │                   │
     SpookyRecord::serialize   SpookyRecordMut::from_spooky_value
               │                   │
               ▼                   ▼
┌──────────────────────┐  ┌────────────────────────────┐
│  SpookyRecord<'a>    │  │  SpookyRecordMut            │
│  (immutable, &[u8])  │  │  (mutable, Vec<u8>)         │
│  • zero-copy reads   │  │  • in-place updates         │
│  • no allocations    │  │  • add/remove fields        │
│  • Copy trait        │  │  • owns its buffer          │
└──────────────────────┘  └────────────────────────────┘
```

### Binary Format

```
┌────────────────────── Header (20 bytes) ──────────────────┐
│  field_count: u32 (LE)  |  reserved: [u8; 16]             │
├────────────────── Index (20 bytes × N) ──────────────────┤
│  name_hash:   u64 (LE)   ← SORTED for binary search      │
│  data_offset: u32 (LE)                                    │
│  data_length: u32 (LE)                                    │
│  type_tag:    u8                                          │
│  _padding:    [u8; 3]                                     │
├──────────────────── Field Data ──────────────────────────┤
│  Flat types: native LE bytes (i64, u64, f64, bool)        │
│  Strings: raw UTF-8 bytes                                 │
│  Nested objects/arrays: CBOR-encoded                      │
└───────────────────────────────────────────────────────────┘
```

## Usage

### Creating Records

```rust
use spooky_db_module::spooky_value::SpookyValue;
use spooky_db_module::spooky_record::SpookyRecord;
use spooky_db_module::spooky_record_mut::SpookyRecordMut;

// From a SpookyValue
let data = SpookyValue::Object(/* ... */);
let bytes = SpookyRecord::serialize(&data).unwrap();

// Immutable zero-copy access
let record = SpookyRecord::from_bytes(&bytes).unwrap();
let name = record.get_str("name");       // Option<&str> — zero-copy
let age  = record.get_i64("age");        // Option<i64>
let ok   = record.get_bool("active");    // Option<bool>

// Mutable in-place access  
let mut rec = SpookyRecordMut::from_vec(bytes).unwrap();
rec.set_i64("age", 29).unwrap();                       // ~6 ns
rec.set_str("name", "Bobby").unwrap();                  // ~13 ns (same len)
rec.add_field("new", &SpookyValue::from(true)).unwrap();// ~191 ns
rec.remove_field("old").unwrap();                       // ~146 ns
```

### SpookyRecord (Immutable)

| Method | Returns | Description |
|---|---|---|
| `serialize(&SpookyValue)` | `Result<Vec<u8>>` | Serialize object to binary |
| `from_bytes(&[u8])` | `Result<Self>` | Zero-copy wrap byte slice |
| `get_str(name)` | `Option<&str>` | Zero-copy string access |
| `get_i64(name)` | `Option<i64>` | Read i64 field |
| `get_u64(name)` | `Option<u64>` | Read u64 field |
| `get_f64(name)` | `Option<f64>` | Read f64 field |
| `get_bool(name)` | `Option<bool>` | Read bool field |
| `get_field(name)` | `Option<SpookyValue>` | Deserialize any field |
| `get_raw(name)` | `Option<FieldRef>` | Raw field reference |
| `field_type(name)` | `Option<u8>` | Type tag without decoding |
| `get_number_as_f64(name)` | `Option<f64>` | Any numeric → f64 |
| `has_field(name)` | `bool` | Existence check |
| `iter_fields()` | `FieldIter` | Iterate raw fields |
| `field_count()` | `u32` | Number of fields |

### SpookyRecordMut (Mutable)

| Method | Description |
|---|---|
| `from_spooky_value(&SpookyValue)` | Create from value |
| `from_vec(Vec<u8>)` | Take ownership of buffer |
| `new_empty()` | Empty record |
| `set_i64(name, val)` | In-place i64 overwrite |
| `set_u64(name, val)` | In-place u64 overwrite |
| `set_f64(name, val)` | In-place f64 overwrite |
| `set_bool(name, val)` | In-place bool overwrite |
| `set_str(name, val)` | String set (splice if needed) |
| `set_str_exact(name, val)` | Same-length string only |
| `set_field(name, &SpookyValue)` | Generic setter |
| `set_null(name)` | Set field to null |
| `add_field(name, &SpookyValue)` | Add new field |
| `remove_field(name)` | Remove field |
| `as_record()` | Borrow as `SpookyRecord` |

### SpookyValue

Dynamically-typed value enum with full `Eq`/`Ord`/`Hash` support:

```rust
pub enum SpookyValue {
    Null,
    Bool(bool),
    Number(SpookyNumber),  // I64 | U64 | F64
    Str(SmolStr),
    Array(Vec<SpookyValue>),
    Object(BTreeMap<SmolStr, SpookyValue>),
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

### Field Migration

| Operation | Median | Throughput | Description |
|---|---|---|---|
| `add_field` | **191.18 ns** | ~5.2M ops/sec | Rebuild with sorted insertion |
| `remove_field` | **146.28 ns** | ~6.8M ops/sec | Rebuild without field |

### Throughput Summary

```
  ┌─────────────────────────────────────────────────────────────┐
  │ Operation          │ Speed           │ Category             │
  ├────────────────────┼─────────────────┼──────────────────────┤
  │ Typed reads        │ ~94-111M ops/s  │ Zero-copy, 0 allocs  │
  │ In-place sets      │ ~118-155M ops/s │ Zero-alloc overwrites│
  │ String splice      │ ~36-85M ops/s   │ Buffer resize        │
  │ Add/Remove field   │ ~5-7M ops/s     │ Full rebuild         │
  │ Full serialize     │ ~250-260K recs/s│ CBOR parse + layout  │
  └─────────────────────────────────────────────────────────────┘
```

### Run Benchmarks

```bash
# All benchmarks
cargo bench

# Specific group
cargo bench --bench spooky_bench -- reading_values

# Quick smoke test
cargo bench --bench spooky_bench -- --test

# View HTML reports
open target/criterion/report/index.html
```

## Dependencies

| Crate | Purpose |
|---|---|
| `ciborium` | CBOR encoding for nested objects/arrays |
| `xxhash-rust` | Fast 64-bit hashing for field name lookups |
| `smol_str` | Small-string-optimized string type |
| `serde` | Serialization framework |
| `serde_json` | JSON support |

## License

MIT
