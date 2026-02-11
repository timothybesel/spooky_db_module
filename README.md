# 🎃 SpookyDB Module

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

Measured with [Criterion.rs](https://github.com/bheisler/criterion.rs) on a 12-field record with nested objects, arrays, and all value types.

### Creating Records

| Operation | Median | Description |
|---|---|---|
| `SpookyRecord::serialize` | **6.31 µs** | Full CBOR → binary pipeline |
| `SpookyRecordMut::from_spooky_value` | **6.24 µs** | Full CBOR → mutable record |
| `SpookyRecordMut::new_empty` | **17.18 ns** | Empty record allocation |
| `SpookyRecordMut::from_vec` | **41.21 ns** | Wrap existing buffer |

### Reading Values

| Operation | Median | Allocs |
|---|---|---|
| `SpookyRecord::get_str` | **10.60 ns** | 0 |
| `SpookyRecord::get_i64` | **10.62 ns** | 0 |
| `SpookyRecord::get_bool` | **9.84 ns** | 0 |
| `SpookyRecord::get_field` | **30.92 ns** | 1 |
| `SpookyRecordMut::get_str` | **9.53 ns** | 0 |
| `SpookyRecordMut::get_i64` | **9.02 ns** | 0 |
| `SpookyRecordMut::get_u64` | **9.08 ns** | 0 |
| `SpookyRecordMut::get_f64` | **11.97 ns** | 0 |
| `SpookyRecordMut::get_bool` | **9.10 ns** | 0 |
| `SpookyRecordMut::get_field` | **31.72 ns** | 1 |

### Setting Values

| Operation | Median | Description |
|---|---|---|
| `set_i64` | **6.44 ns** | In-place overwrite |
| `set_u64` | **8.46 ns** | In-place overwrite |
| `set_f64` | **6.53 ns** | In-place overwrite |
| `set_bool` | **8.16 ns** | In-place overwrite |
| `set_str` (same len) | **13.17 ns** | In-place overwrite |
| `set_str` (diff len) | **27.88 ns** | Splice + fixup |
| `set_str_exact` | **11.82 ns** | Same-length guaranteed |
| `set_field` | **26.26 ns** | Generic path |
| `set_null` | **10.07 ns** | In-place overwrite |

### Field Migration

| Operation | Median | Description |
|---|---|---|
| `add_field` | **191.18 ns** | Rebuild with sorted insertion |
| `remove_field` | **146.28 ns** | Rebuild without field |

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
