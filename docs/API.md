# SpookyDb — API Reference

> Complete public API for `spooky_db_module`. For architecture and design rationale, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Quick Navigation

- [Serialization](#serialization-spooky_db_moduleserializaton)
  - [Trait: RecordSerialize](#trait-recordserialize)
  - [serialize](#serialize)
  - [serialize_into](#serialize_into)
  - [from_spooky](#from_spooky)
  - [from_cbor](#from_cbor)
  - [from_bytes](#from_bytes)
  - [serialize_into_buf](#serialize_into_buf)
  - [write_field_into](#write_field_into)
- [Deserialization](#deserialization-spooky_db_moduledeserialization)
  - [Trait: RecordDeserialize](#trait-recorddeserialize)
  - [decode_field](#decode_field)
- [Value Types](#value-types-spooky_db_modulespooky_value)
  - [SpookyValue](#spookyvalue)
  - [SpookyNumber](#spookynumber)
- [Record Types](#record-types-spooky_db_modulespooky_record)
  - [Trait: SpookyReadable](#trait-spookyreadable)
  - [SpookyRecord](#spookyrecord)
  - [SpookyRecordMut](#spookyrecordmut)
  - [FieldSlot](#fieldslot)
  - [FieldRef](#fieldref)
  - [FieldIter](#fielditer)
- [Persistence](#persistence-spooky_db_moduledb)
  - [SpookyDb](#spookydb)
  - [Trait: DbBackend](#trait-dbbackend)
  - [SpookyDbConfig](#spookydbconfig)
  - [Operation](#operation)
  - [DbMutation](#dbmutation)
  - [BulkRecord](#bulkrecord)
  - [BatchMutationResult](#batchmutationresult)
  - [SpookyDbError](#spookydberror)
- [Error Reference](#error-reference)
  - [RecordError](#recorderror)
- [Type Aliases](#type-aliases)
- [Constants](#constants)

---

## Serialization (`spooky_db_module::serialization`)

### Trait: `RecordSerialize`

**Definition**: `pub trait RecordSerialize: serde::Serialize`

Adapter trait for value types that can be serialized into the binary record format. It abstracts over `SpookyValue`, `serde_json::Value`, and `cbor4ii::core::Value`, allowing any of them to be stored without conversion overhead. The serializer dispatches to typed fast paths (native LE bytes for scalars, raw UTF-8 for strings, CBOR for nested types) based on the predicate methods below.

**Implemented by**: `SpookyValue`, `serde_json::Value`, `cbor4ii::core::Value`, and `&T where T: RecordSerialize`.

#### Methods

| Method | Signature | Description |
|--------|-----------|-------------|
| `is_null` | `fn is_null(&self) -> bool` | Returns `true` if this value is null. |
| `as_bool` | `fn as_bool(&self) -> Option<bool>` | Extracts a boolean, or `None` if this is not a boolean. |
| `as_i64` | `fn as_i64(&self) -> Option<i64>` | Extracts a signed 64-bit integer, or `None`. |
| `as_u64` | `fn as_u64(&self) -> Option<u64>` | Extracts an unsigned 64-bit integer, or `None`. |
| `as_f64` | `fn as_f64(&self) -> Option<f64>` | Extracts a 64-bit float, or `None`. |
| `as_str` | `fn as_str(&self) -> Option<&str>` | Extracts a string slice, or `None`. |
| `is_nested` | `fn is_nested(&self) -> bool` | Returns `true` if this value is an array or object (will be CBOR-encoded). |

**Type dispatch order** in `write_field_into`: null → bool → i64 → u64 → f64 → str → nested (CBOR). A value is matched by the first predicate that fires. Because `serde_json::Value` numbers can satisfy both `as_i64` and `as_f64`, the i64 path fires first for integer JSON numbers.

---

### `serialize`

**Signature**:
```rust
pub fn serialize<V: RecordSerialize>(
    map: &BTreeMap<SmolStr, V>,
) -> Result<(Vec<u8>, usize), RecordError>
```

Serialize a map of named fields into a freshly allocated binary record buffer. Fields are hashed with xxh64 (seed 0), sorted by hash, and written in sorted order. The returned buffer is a valid SpookyRecord.

**Returns**: `(bytes, field_count)` — the complete serialized buffer and the number of fields written.

**Errors**:
- `RecordError::TooManyFields` — `map.len() > 32`.
- `RecordError::CborError` — CBOR encoding failed for a nested field.

**Example**:
```rust
use spooky_db_module::serialization::serialize;
use spooky_db_module::spooky_value::{SpookyValue, SpookyNumber};
use smol_str::SmolStr;
use std::collections::BTreeMap;

let mut map: BTreeMap<SmolStr, SpookyValue> = BTreeMap::new();
map.insert(SmolStr::new("name"), SpookyValue::Str(SmolStr::new("Alice")));
map.insert(SmolStr::new("age"), SpookyValue::Number(SpookyNumber::I64(28)));
let (bytes, count) = serialize(&map).unwrap();
assert_eq!(count, 2);
assert!(bytes.len() > 20); // header + index + data
```

---

### `serialize_into`

**Signature**:
```rust
pub fn serialize_into<V: RecordSerialize>(
    map: &BTreeMap<SmolStr, V>,
    buf: &mut Vec<u8>,
) -> Result<usize, RecordError>
```

Serialize a field map into a caller-supplied buffer. The buffer is cleared but retains its existing allocation, eliminating one heap allocation per record in bulk serialization loops. Approximately 17% faster than `serialize` when the same buffer is reused across many calls.

**Returns**: the number of fields written.

**Errors**: same as `serialize`.

**Example**:
```rust
use spooky_db_module::serialization::serialize_into;
use spooky_db_module::spooky_value::{SpookyValue, SpookyNumber};
use smol_str::SmolStr;
use std::collections::BTreeMap;

let mut buf = Vec::with_capacity(256);
let mut map: BTreeMap<SmolStr, SpookyValue> = BTreeMap::new();
map.insert(SmolStr::new("score"), SpookyValue::Number(SpookyNumber::F64(9.5)));

// Reuse buf across 10_000 records without reallocating:
for _ in 0..10_000 {
    let _count = serialize_into(&map, &mut buf).unwrap();
    // persist buf to redb here
}
```

---

### `from_spooky`

**Signature**:
```rust
pub fn from_spooky(data: &SpookyValue) -> Result<(Vec<u8>, usize), RecordError>
```

Convenience wrapper around `serialize` for `SpookyValue::Object` inputs. Extracts the inner map and serializes it.

**Returns**: `(bytes, field_count)`.

**Errors**:
- `RecordError::InvalidBuffer` — `data` is not a `SpookyValue::Object`.
- `RecordError::TooManyFields` — the object has more than 32 fields.

**Example**:
```rust
use spooky_db_module::serialization::from_spooky;
use spooky_db_module::spooky_value::SpookyValue;
use smol_str::SmolStr;
use std::collections::BTreeMap;

let mut map = BTreeMap::new();
map.insert(SmolStr::new("active"), SpookyValue::Bool(true));
let val = SpookyValue::Object(map);
let (bytes, count) = from_spooky(&val).unwrap();
assert_eq!(count, 1);
```

---

### `from_cbor`

**Signature**:
```rust
pub fn from_cbor(data: &cbor4ii::core::Value) -> Result<(Vec<u8>, usize), RecordError>
```

Serialize a `cbor4ii::core::Value::Map` directly into the binary record format without first converting to `SpookyValue`. All CBOR map keys must be text strings; non-text keys return `RecordError::CborError`.

**Returns**: `(bytes, field_count)`.

**Errors**:
- `RecordError::InvalidBuffer` — `data` is not a `cbor4ii::core::Value::Map`.
- `RecordError::CborError` — a map key is not a text value.
- `RecordError::TooManyFields` — the map has more than 32 entries.

**Example**:
```rust
use spooky_db_module::serialization::from_cbor;

// Assume `cbor_bytes` is a CBOR-encoded map
let cbor_val: cbor4ii::core::Value = cbor4ii::serde::from_slice(cbor_bytes).unwrap();
let (bytes, count) = from_cbor(&cbor_val).unwrap();
```

---

### `from_bytes`

**Signature**:
```rust
pub fn from_bytes(buf: &[u8]) -> Result<(&[u8], usize), RecordError>
```

Validate a raw byte slice as a well-formed SpookyRecord buffer and extract the field count from the header. No field data is parsed — the buffer is not copied. In debug builds, additionally verifies that the index is sorted by hash.

**Returns**: `(buf, field_count)` — the same slice and the field count read from the header.

**Errors**:
- `RecordError::InvalidBuffer` — buffer is shorter than `HEADER_SIZE`, or shorter than `HEADER_SIZE + field_count * INDEX_ENTRY_SIZE`.

**Usage pattern** (reading from redb):
```rust
use spooky_db_module::serialization::from_bytes;
use spooky_db_module::spooky_record::{SpookyRecord, SpookyReadable};

let raw: Vec<u8> = /* bytes from redb */ vec![];
let (buf, count) = from_bytes(&raw).unwrap();
let record = SpookyRecord::new(buf, count);
let name = record.get_str("name");
```

---

### `serialize_into_buf`

**Signature**:
```rust
pub fn serialize_into_buf(
    data: &SpookyValue,
    buf: &mut Vec<u8>,
) -> Result<(), RecordError>
```

Serialize a `SpookyValue::Object` into a reusable buffer. Combines the convenience of `from_spooky` with the allocation efficiency of `serialize_into`. The buffer is cleared but retains its capacity.

**Errors**:
- `RecordError::InvalidBuffer` — `data` is not a `SpookyValue::Object`.
- `RecordError::TooManyFields` — more than 32 fields.

**Example**:
```rust
use spooky_db_module::serialization::serialize_into_buf;
use spooky_db_module::spooky_value::SpookyValue;
use smol_str::SmolStr;
use std::collections::BTreeMap;

let mut buf = Vec::new();
let mut map = BTreeMap::new();
map.insert(SmolStr::new("x"), SpookyValue::Bool(false));
let val = SpookyValue::Object(map);
serialize_into_buf(&val, &mut buf).unwrap();
```

---

### `write_field_into`

**Signature**:
```rust
pub fn write_field_into<V: RecordSerialize>(
    buf: &mut Vec<u8>,
    value: &V,
) -> Result<u8, RecordError>
```

Low-level primitive. Appends the binary encoding of a single field value to `buf` and returns the type tag (`TAG_*` constant) that identifies the encoding used. Used internally by all serialization paths. Exposed publicly for custom serialization pipelines that build record buffers manually.

**Encoding rules**:
- `is_null()` → writes nothing, returns `TAG_NULL`.
- `as_bool()` → writes 1 byte (`0` or `1`), returns `TAG_BOOL`.
- `as_i64()` → writes 8 bytes LE, returns `TAG_I64`.
- `as_u64()` → writes 8 bytes LE, returns `TAG_U64`.
- `as_f64()` → writes 8 bytes LE, returns `TAG_F64`.
- `as_str()` → writes raw UTF-8 bytes, returns `TAG_STR`.
- `is_nested()` → CBOR-encodes via `serde::Serialize`, returns `TAG_NESTED_CBOR`.

**Errors**:
- `RecordError::CborError` — CBOR encoding failed for a nested type.
- `RecordError::UnknownTypeTag(0)` — the value matched no predicate (indicates a broken `RecordSerialize` implementation).

---

## Deserialization (`spooky_db_module::deserialization`)

### Trait: `RecordDeserialize`

**Definition**: `pub trait RecordDeserialize: Sized`

Adapter trait for value types that can be constructed from binary record fields. Each method corresponds to one type tag. Implementors construct an instance of `Self` from a primitive value or from raw CBOR bytes.

**Implemented by**: `SpookyValue`, `serde_json::Value`, `cbor4ii::core::Value`.

#### Methods

| Method | Signature | Description |
|--------|-----------|-------------|
| `from_null` | `fn from_null() -> Self` | Construct a null value. |
| `from_bool` | `fn from_bool(b: bool) -> Self` | Construct a boolean value. |
| `from_i64` | `fn from_i64(v: i64) -> Self` | Construct from a signed 64-bit integer. |
| `from_u64` | `fn from_u64(v: u64) -> Self` | Construct from an unsigned 64-bit integer. |
| `from_f64` | `fn from_f64(v: f64) -> Self` | Construct from a 64-bit float. |
| `from_str` | `fn from_str(s: &str) -> Self` | Construct from a UTF-8 string slice. |
| `from_cbor_bytes` | `fn from_cbor_bytes(data: &[u8]) -> Option<Self>` | Deserialize from raw CBOR bytes. Returns `None` on parse failure. |

**Notes on `serde_json::Value` for f64**: `from_f64` returns `serde_json::Value::Null` if the float is NaN or infinite, since JSON cannot represent those values.

**Notes on `cbor4ii::core::Value` for integers**: both `from_i64` and `from_u64` produce `cbor4ii::core::Value::Integer(i128)`. The i128 representation is lossless for the full i64/u64 range.

---

### `decode_field`

**Signature**:
```rust
pub fn decode_field<V: RecordDeserialize>(field: FieldRef) -> Option<V>
```

Decode a raw `FieldRef` into any type that implements `RecordDeserialize`. Dispatches on `field.type_tag` and interprets `field.data` accordingly. Returns `None` if the tag is unrecognised or if data is malformed for that tag (e.g., fewer than 8 bytes for TAG_I64, or invalid UTF-8 for TAG_STR).

This function is called internally by `SpookyReadable::get_field`. It is exposed publicly so custom readers can decode `FieldRef` values obtained from `iter_fields` or `get_raw` without going through a `SpookyReadable` record.

**Example**:
```rust
use spooky_db_module::deserialization::{decode_field, RecordDeserialize};
use spooky_db_module::spooky_value::SpookyValue;
use spooky_db_module::spooky_record::{SpookyReadable, SpookyRecord};
use spooky_db_module::serialization::from_bytes;

let raw: Vec<u8> = /* SpookyRecord bytes */ vec![];
let (buf, count) = from_bytes(&raw).unwrap();
let record = SpookyRecord::new(buf, count);
for field_ref in record.iter_fields() {
    let val: Option<SpookyValue> = decode_field(field_ref);
    // val is None only if the type tag is unknown
}
```

---

## Value Types (`spooky_db_module::spooky_value`)

### `SpookyValue`

**Definition**: `pub enum SpookyValue`

The native dynamic value type for `spooky_db_module`. Implements `Eq`, `Ord`, `Hash`, `Clone`, `Debug`, `Default` (`Null`), and `serde::Serialize`. Convertible to and from `serde_json::Value` and `cbor4ii::core::Value` via `From`/`Into`.

Total ordering: `Null < Bool < Number < Str < Array < Object`.

#### Variants

| Variant | Inner type | Description |
|---------|-----------|-------------|
| `Null` | — | Absent or null value. |
| `Bool(bool)` | `bool` | Boolean. |
| `Number(SpookyNumber)` | `SpookyNumber` | Numeric — i64, u64, or f64. |
| `Str(SmolStr)` | `SmolStr` | UTF-8 string. Stack-inlined for strings ≤ 22 bytes. |
| `Array(Vec<SpookyValue>)` | `Vec<SpookyValue>` | Ordered list. Stored as CBOR in the binary format. |
| `Object(BTreeMap<SmolStr, SpookyValue>)` | `BTreeMap<SmolStr, SpookyValue>` | Named fields. Stored as CBOR in the binary format. |

Note: `FastMap<K, V>` in `spooky_value.rs` is a `BTreeMap` alias, **not** an FxHasher map. The `FastMap` alias in `db::types` is an FxHasher `HashMap`. Use explicit paths if importing both.

#### Accessors

| Method | Signature | Returns |
|--------|-----------|---------|
| `as_str` | `pub fn as_str(&self) -> Option<&str>` | String slice for `Str` variant. |
| `as_f64` | `pub fn as_f64(&self) -> Option<f64>` | Float for any `Number` variant (via `SpookyNumber::as_f64`). |
| `as_i64` | `pub fn as_i64(&self) -> Option<i64>` | `i64` for `Number`, if representable. |
| `as_u64` | `pub fn as_u64(&self) -> Option<u64>` | `u64` for `Number`, if representable. |
| `as_bool` | `pub fn as_bool(&self) -> Option<bool>` | Boolean for `Bool` variant. |
| `as_object` | `pub fn as_object(&self) -> Option<&BTreeMap<SmolStr, SpookyValue>>` | Immutable map for `Object` variant. |
| `as_object_mut` | `pub fn as_object_mut(&mut self) -> Option<&mut BTreeMap<SmolStr, SpookyValue>>` | Mutable map for `Object` variant. |
| `as_array` | `pub fn as_array(&self) -> Option<&Vec<SpookyValue>>` | Immutable slice for `Array` variant. |
| `as_array_mut` | `pub fn as_array_mut(&mut self) -> Option<&mut Vec<SpookyValue>>` | Mutable slice for `Array` variant. |
| `get` | `pub fn get(&self, key: &str) -> Option<&SpookyValue>` | Field access by name on `Object`. Zero-allocation (SmolStr implements `Borrow<str>`). |
| `get_mut` | `pub fn get_mut(&mut self, key: &str) -> Option<&mut SpookyValue>` | Mutable field access by name on `Object`. |
| `is_null` | `pub fn is_null(&self) -> bool` | `true` for `Null`. |
| `is_object` | `pub fn is_object(&self) -> bool` | `true` for `Object`. |
| `is_array` | `pub fn is_array(&self) -> bool` | `true` for `Array`. |
| `is_string` | `pub fn is_string(&self) -> bool` | `true` for `Str`. |
| `is_number` | `pub fn is_number(&self) -> bool` | `true` for `Number`. |

#### `From` conversions

| Source | Produces |
|--------|---------|
| `f64` | `SpookyValue::Number(SpookyNumber::F64(_))` |
| `i64` | `SpookyValue::Number(SpookyNumber::I64(_))` |
| `i32` | `SpookyValue::Number(SpookyNumber::I64(_ as i64))` |
| `u64` | `SpookyValue::Number(SpookyNumber::U64(_))` |
| `u32` | `SpookyValue::Number(SpookyNumber::U64(_ as u64))` |
| `bool` | `SpookyValue::Bool(_)` |
| `&str` | `SpookyValue::Str(SmolStr::from(_))` |
| `String` | `SpookyValue::Str(SmolStr::from(_))` |
| `SmolStr` | `SpookyValue::Str(_)` |
| `cbor4ii::core::Value` | Recursive conversion |
| `serde_json::Value` | Recursive conversion |

---

### `SpookyNumber`

**Definition**: `pub enum SpookyNumber`

Numeric type that covers all three primitive numeric kinds used by the binary format. Implements total ordering (`Ord`), `Hash`, `Eq`, `PartialEq`, `Clone`, `Copy`, `Debug`. Ordering uses f64 promotion with canonical NaN and -0.0/+0.0 handling so `SpookyNumber` can be used as a `BTreeMap` or `ZSet` key.

Cross-variant ordering: all variants are compared via `as_f64` unless both are the same integer variant (integer fast path avoids float imprecision for i64 vs i64 and u64 vs u64 comparisons).

#### Variants

| Variant | Storage | Description |
|---------|---------|-------------|
| `I64(i64)` | 8 bytes LE in record | Signed 64-bit integer. |
| `U64(u64)` | 8 bytes LE in record | Unsigned 64-bit integer. |
| `F64(f64)` | 8 bytes LE in record | IEEE 754 double. |

#### Methods

| Method | Signature | Description |
|--------|-----------|-------------|
| `as_f64` | `pub fn as_f64(self) -> f64` | Promote to f64. Lossless for small integers; large i64/u64 values may lose precision. |
| `as_i64` | `pub fn as_i64(self) -> Option<i64>` | Convert to i64 if representable (whole number within `i64::MIN..=i64::MAX`). |
| `as_u64` | `pub fn as_u64(self) -> Option<u64>` | Convert to u64 if representable (non-negative whole number within `u64::MAX`). |

---

## Record Types (`spooky_db_module::spooky_record`)

### Trait: `SpookyReadable`

**Definition**: `pub trait SpookyReadable`

The primary read interface. Implemented by both `SpookyRecord<'a>` and `SpookyRecordMut`. All field-reading methods are defined here; both record types get them for free. No allocation occurs for flat types (i64, u64, f64, bool, str). Nested CBOR fields allocate once during deserialization.

**Required methods** (implement to satisfy the trait):
- `fn data_buf(&self) -> &[u8]`
- `fn field_count(&self) -> usize`
- `fn iter_fields(&self) -> FieldIter<'_>`

All other methods below are provided default implementations on the trait.

---

#### `find_field`

**Signature**: `fn find_field(&self, name: &str) -> Result<(usize, IndexEntry), RecordError>`

Hash `name` with xxh64 (seed 0) and search the sorted index. Uses linear scan for ≤ 4 fields, binary search for ≥ 5 fields.

**Returns**: `(index_position, IndexEntry)`.

**Errors**: `RecordError::FieldNotFound` if no entry matches the hash.

---

#### `get_str`

**Signature**: `fn get_str(&self, name: &str) -> Option<&str>`

Zero-copy string read. Borrows directly from the record buffer. Returns `None` if the field is absent or is not `TAG_STR`.

---

#### `get_i64`

**Signature**: `fn get_i64(&self, name: &str) -> Option<i64>`

Read an i64 field. Returns `None` if the field is absent, is not `TAG_I64`, or has a length other than 8 bytes.

---

#### `get_u64`

**Signature**: `fn get_u64(&self, name: &str) -> Option<u64>`

Read a u64 field. Returns `None` if the field is absent, is not `TAG_U64`, or has a length other than 8 bytes.

---

#### `get_f64`

**Signature**: `fn get_f64(&self, name: &str) -> Option<f64>`

Read an f64 field. Returns `None` if the field is absent, is not `TAG_F64`, or has a length other than 8 bytes.

---

#### `get_bool`

**Signature**: `fn get_bool(&self, name: &str) -> Option<bool>`

Read a bool field. Returns `None` if the field is absent, is not `TAG_BOOL`, or has a length other than 1 byte.

---

#### `get_raw`

**Signature**: `fn get_raw(&self, name: &str) -> Option<FieldRef<'_>>`

Zero-copy field reference. Returns a `FieldRef` containing the raw bytes, the type tag, and the field's name hash. Use with `decode_field` for generic deserialization, or inspect `type_tag` directly for custom logic.

---

#### `get_field`

**Signature**: `fn get_field<V: RecordDeserialize>(&self, name: &str) -> Option<V>`

Deserialize a field into any type implementing `RecordDeserialize`. Calls `get_raw` then `decode_field`. Allocates for nested CBOR fields; zero allocation for flat types.

**Example**:
```rust
use spooky_db_module::spooky_record::{SpookyReadable, SpookyRecord};
use spooky_db_module::spooky_value::SpookyValue;
use spooky_db_module::serialization::from_bytes;

let raw: Vec<u8> = /* record bytes */ vec![];
let (buf, count) = from_bytes(&raw).unwrap();
let record = SpookyRecord::new(buf, count);

// Typed as SpookyValue
let profile: Option<SpookyValue> = record.get_field::<SpookyValue>("profile");

// Typed as serde_json::Value
let profile_json: Option<serde_json::Value> = record.get_field::<serde_json::Value>("profile");
```

---

#### `get_number_as_f64`

**Signature**: `fn get_number_as_f64(&self, name: &str) -> Option<f64>`

Read any numeric field (TAG_I64, TAG_U64, or TAG_F64) and return it as f64, converting integer types. Returns `None` for non-numeric fields or absent fields.

---

#### `has_field`

**Signature**: `fn has_field(&self, name: &str) -> bool`

Returns `true` if the field exists (regardless of type). Equivalent to `find_field(name).is_ok()`.

---

#### `field_type`

**Signature**: `fn field_type(&self, name: &str) -> Option<u8>`

Returns the `TAG_*` constant for the named field, or `None` if the field is absent.

---

#### `iter_fields`

**Signature**: `fn iter_fields(&self) -> FieldIter<'_>`

Returns an iterator over all raw field references in index order (sorted by name hash). Yields `FieldRef` items. See [`FieldIter`](#fielditer) for iterator details. Zero-copy — no allocations.

---

#### `to_value`

**Signature**: `fn to_value(&self) -> SpookyValue`

**Always returns `SpookyValue::Null`.** Field names are not recoverable from xxh64 hashes stored in the binary format. This method exists as a trait placeholder. Use `get_record_typed(table, id, &["field1", "field2"])` on `SpookyDb` instead, which reconstructs a partial `SpookyValue::Object` by looking up named fields.

---

#### `resolve`

**Signature**: `fn resolve(&self, name: &str) -> Option<FieldSlot>`

Perform one O(log n) hash lookup and cache the result as a `FieldSlot`. The slot records the field's `index_pos`, `data_offset`, `data_len`, `type_tag`, and the record's current `generation`. Use the slot with `get_*_at` and `set_*_at` for O(1) repeat access on the same field.

**Validity**: the slot is valid as long as the record's `generation` has not changed. Any layout-changing mutation (`add_field`, `remove_field`, or a string splice of different length) increments `generation`, invalidating all existing slots. Staleness is caught by `debug_assert` in all `_at` accessors — zero overhead in release builds.

**Example**:
```rust
use spooky_db_module::spooky_record::{SpookyReadable, SpookyRecordMut};

let bytes = /* serialized record bytes */ vec![];
let count = 2;
let mut rec = SpookyRecordMut::new(bytes, count);

let slot = rec.resolve("score").expect("field must exist");
// O(1) reads and writes thereafter:
let score = rec.get_f64_at(&slot);
rec.set_f64_at(&slot, 42.0).unwrap();
```

---

#### `get_i64_at`

**Signature**: `fn get_i64_at(&self, slot: &FieldSlot) -> Option<i64>`

O(1) read using a pre-resolved `FieldSlot`. Approximately 2–3 ns vs ~10 ns for a by-name lookup. Returns `None` if the slot's type tag is not `TAG_I64` or data length is not 8. Panics in debug builds if `slot.generation != self.generation()`.

---

#### `get_u64_at`

**Signature**: `fn get_u64_at(&self, slot: &FieldSlot) -> Option<u64>`

O(1) read for a u64 field. Same validity rules as `get_i64_at`.

---

#### `get_f64_at`

**Signature**: `fn get_f64_at(&self, slot: &FieldSlot) -> Option<f64>`

O(1) read for an f64 field. Same validity rules as `get_i64_at`.

---

#### `get_bool_at`

**Signature**: `fn get_bool_at(&self, slot: &FieldSlot) -> Option<bool>`

O(1) read for a bool field. Returns `None` if data length is not 1.

---

#### `get_str_at`

**Signature**: `fn get_str_at(&self, slot: &FieldSlot) -> Option<&str>`

O(1) zero-copy string read using a cached slot. Returns `None` if the slot's type tag is not `TAG_STR` or the bytes are not valid UTF-8.

---

### `SpookyRecord<'a>`

**Definition**:
```rust
pub struct SpookyRecord<'a> {
    pub data_buf: &'a [u8],
    pub field_count: usize,
}
```

Zero-copy reader that borrows `&'a [u8]`. Implements `Copy` and `Clone` — passing a `SpookyRecord` by value is equivalent to copying a slice pointer and a `usize`. Implements `SpookyReadable`, so all read methods are available. No mutation methods.

Typical lifetime: as short as the surrounding redb read guard or the in-memory cache borrow in `SpookyDb::get_row_record`.

#### Construction

**`SpookyRecord::new`**

**Signature**: `pub fn new(data_buf: &'a [u8], field_count: usize) -> Self`

Construct from a borrowed slice and a known field count. In debug builds, asserts that `field_count` matches the count in the buffer header. Callers normally obtain `field_count` from `from_bytes`.

**Example**:
```rust
use spooky_db_module::serialization::from_bytes;
use spooky_db_module::spooky_record::SpookyRecord;

let raw: Vec<u8> = /* record bytes */ vec![];
let (buf, count) = from_bytes(&raw).unwrap();
let record = SpookyRecord::new(buf, count);
```

---

### `SpookyRecordMut`

**Definition**:
```rust
pub struct SpookyRecordMut {
    pub data_buf: Vec<u8>,
    pub field_count: usize,
    pub generation: usize,
}
```

Owned, mutable record. Wraps `Vec<u8>`. Implements `SpookyReadable` for all read methods plus a separate set of write methods. The `generation` counter starts at 0 and is incremented by any operation that changes the buffer layout (string splice, `add_field`, `remove_field`). In-place overwrites of fixed-width fields do not change `generation`.

---

#### Construction

**`SpookyRecordMut::new`**

**Signature**: `pub fn new(data_buf: Vec<u8>, field_count: usize) -> Self`

Take ownership of a pre-serialized buffer. In debug builds, verifies that `field_count` matches the buffer header. Use after obtaining bytes from `serialize` or from `redb`.

**`SpookyRecordMut::new_empty`**

**Signature**: `pub fn new_empty() -> Self`

Create an empty mutable record with no fields. The buffer contains only a zeroed header. Fields can be added with `add_field`.

**`SpookyRecordMut::as_record`**

**Signature**: `pub fn as_record(&self) -> SpookyRecord<'_>`

Produce a zero-copy `SpookyRecord` view over the mutable buffer without consuming it. Lifetime is tied to `&self`.

**`SpookyRecordMut::find_insert_pos`**

**Signature**: `pub fn find_insert_pos(&self, hash: u64) -> usize`

Binary search for the position at which a field with the given hash should be inserted to maintain sorted order. Used internally by `add_field`. Exposed publicly for custom structural mutation code.

---

#### By-Name Setters

All by-name setters call `find_field` internally (O(log n) or O(n) for n ≤ 4). They fail with `TypeMismatch` if the field exists but has a different type.

**`set_i64`**

**Signature**: `pub fn set_i64(&mut self, name: &str, value: i64) -> Result<(), RecordError>`

In-place overwrite of an i64 field. Zero allocation, ~20 ns. Does not change `generation`.

**Errors**:
- `RecordError::FieldNotFound` — field absent.
- `RecordError::TypeMismatch` — field is not TAG_I64.

**`set_u64`**

**Signature**: `pub fn set_u64(&mut self, name: &str, value: u64) -> Result<(), RecordError>`

In-place overwrite of a u64 field. Zero allocation, ~20 ns. Does not change `generation`.

**`set_f64`**

**Signature**: `pub fn set_f64(&mut self, name: &str, value: f64) -> Result<(), RecordError>`

In-place overwrite of an f64 field. Zero allocation, ~20 ns. Does not change `generation`.

**`set_bool`**

**Signature**: `pub fn set_bool(&mut self, name: &str, value: bool) -> Result<(), RecordError>`

In-place overwrite of a bool field. Zero allocation, ~18 ns. Does not change `generation`.

**`set_str`**

**Signature**: `pub fn set_str(&mut self, name: &str, value: &str) -> Result<(), RecordError>`

Update a string field. Uses the fast path (direct overwrite, ~22 ns, no allocation) if the new value has the exact same byte length. Uses a splice (buffer resize + offset fixup for all subsequent fields, ~150–350 ns) if the length differs. Increments `generation` on the splice path, invalidating any held `FieldSlot` for this or any other field.

**Errors**:
- `RecordError::FieldNotFound` — field absent.
- `RecordError::TypeMismatch` — field is not TAG_STR.

**`set_str_exact`**

**Signature**: `pub fn set_str_exact(&mut self, name: &str, value: &str) -> Result<(), RecordError>`

Write a string field only if the new value has the exact same byte length. Guaranteed zero allocation. Returns `RecordError::LengthMismatch` on length mismatch — caller must fall back to `set_str` if they need to change the length.

**Errors**:
- `RecordError::FieldNotFound` — field absent.
- `RecordError::TypeMismatch` — field is not TAG_STR.
- `RecordError::LengthMismatch { expected, actual }` — byte lengths differ.

**`set_field`**

**Signature**:
```rust
pub fn set_field<V: RecordSerialize>(
    &mut self,
    name: &str,
    value: &V,
) -> Result<(), RecordError>
```

Generic setter for any field, any type. Serializes `value` to a temporary buffer, then:
- Same byte size as current data → in-place overwrite (~25 ns), updates type tag if changed.
- Different byte size → splice + offset fixup (~200–500 ns), increments `generation`.

Prefer the typed setters (`set_i64`, `set_str`, etc.) for hot paths — they skip the temporary allocation.

**`set_null`**

**Signature**: `pub fn set_null(&mut self, name: &str) -> Result<(), RecordError>`

Set a field to null (TAG_NULL, 0 data bytes). Delegates to `set_field`. Increments `generation` if the field previously had non-zero data length.

---

#### FieldSlot Setters (O(1))

All `_at` setters require a valid (non-stale) `FieldSlot` obtained via `resolve`. They perform no hash lookup. Stale slots (from a previous `generation`) trigger a `debug_assert` panic in debug builds; no check in release builds.

**`set_i64_at`**

**Signature**: `pub fn set_i64_at(&mut self, slot: &FieldSlot, value: i64) -> Result<(), RecordError>`

In-place i64 overwrite using a cached slot. ~20 ns. Does not change `generation`.

**Errors**: `RecordError::TypeMismatch` — slot type tag is not TAG_I64.

**`set_u64_at`**

**Signature**: `pub fn set_u64_at(&mut self, slot: &FieldSlot, value: u64) -> Result<(), RecordError>`

In-place u64 overwrite. ~20 ns. Does not change `generation`.

**`set_f64_at`**

**Signature**: `pub fn set_f64_at(&mut self, slot: &FieldSlot, value: f64) -> Result<(), RecordError>`

In-place f64 overwrite. ~20 ns. Does not change `generation`.

**`set_bool_at`**

**Signature**: `pub fn set_bool_at(&mut self, slot: &FieldSlot, value: bool) -> Result<(), RecordError>`

In-place bool overwrite. ~18 ns. Does not change `generation`.

**`set_str_at`**

**Signature**: `pub fn set_str_at(&mut self, slot: &FieldSlot, value: &str) -> Result<(), RecordError>`

In-place string overwrite using a cached slot. Only accepts strings with the **exact same byte length** as the current value. Same-length writes are ~22 ns and do not change `generation`. Returns `RecordError::LengthMismatch` if the new string is a different byte length — caller must fall back to `set_str(name, value)` followed by `re-resolve(name)`.

**Errors**:
- `RecordError::TypeMismatch` — slot type tag is not TAG_STR.
- `RecordError::LengthMismatch { expected, actual }` — byte lengths differ.

---

#### Structural Mutations

Both `add_field` and `remove_field` rebuild the entire record buffer. They always increment `generation`, invalidating all held `FieldSlot` values.

**`add_field`**

**Signature**:
```rust
pub fn add_field<V: RecordSerialize>(
    &mut self,
    name: &str,
    value: &V,
) -> Result<(), RecordError>
```

Insert a new field into the record at the correct sorted position. The buffer is fully rebuilt (new `Vec<u8>`, one heap allocation). Increments `generation`.

**Errors**:
- `RecordError::FieldExists` — a field with this name already exists.
- `RecordError::TooManyFields` — adding this field would exceed 32 fields.
- `RecordError::CborError` — CBOR encoding failed for a nested value.

**Example**:
```rust
use spooky_db_module::spooky_record::SpookyRecordMut;
use spooky_db_module::spooky_value::SpookyValue;

let mut rec = SpookyRecordMut::new_empty();
rec.add_field("name", &SpookyValue::from("Alice")).unwrap();
rec.add_field("age",  &SpookyValue::from(28i64)).unwrap();
// rec.data_buf now holds a valid 2-field SpookyRecord
```

**`remove_field`**

**Signature**: `pub fn remove_field(&mut self, name: &str) -> Result<(), RecordError>`

Remove a field from the record. Rebuilds the buffer without the removed field. If removing the last field, the buffer is reset to an empty header. Increments `generation`.

**Errors**: `RecordError::FieldNotFound` — field is not in the record.

---

#### Accessing the Buffer

`SpookyRecordMut` does not yet expose `into_bytes`, `as_bytes`, or `byte_len` as public methods. To access the serialized bytes for persistence, read `rec.data_buf` directly:

```rust
// Persist to redb after mutations:
let bytes: &[u8] = &rec.data_buf;
// or take ownership:
let owned: Vec<u8> = rec.data_buf;
```

---

### `FieldSlot`

**Definition**:
```rust
pub struct FieldSlot {
    pub(crate) index_pos: usize,
    pub(crate) data_offset: usize,
    pub(crate) data_len: usize,
    pub(crate) type_tag: u8,
    pub(crate) generation: usize,
}
```

Cached field position returned by `SpookyReadable::resolve`. Contains all metadata needed to read or write a field in O(1) without hashing or index traversal. All fields are `pub(crate)` — external code interacts with slots only through `resolve` and the `_at` accessor methods.

**Validity rule**: a slot is valid only while `slot.generation == record.generation()`. Any layout-changing mutation on the record increments `generation`, invalidating all existing slots. Re-resolve with `record.resolve(name)` after any `add_field`, `remove_field`, or variable-length string change.

**Performance**: the `_at` read and write methods are typically 2–5x faster than the by-name equivalents on the hot path, because they skip the xxh64 hash computation and binary search.

**Pattern for hot-path pipelines**:
```rust
use spooky_db_module::spooky_record::{SpookyReadable, SpookyRecordMut};
use spooky_db_module::spooky_value::SpookyValue;

let mut rec = SpookyRecordMut::new_empty();
rec.add_field("count", &SpookyValue::from(0i64)).unwrap();
rec.add_field("score", &SpookyValue::from(0.0f64)).unwrap();

// Resolve once per record schema change:
let count_slot = rec.resolve("count").unwrap();
let score_slot = rec.resolve("score").unwrap();

// O(1) reads and writes in the hot loop:
for _ in 0..1_000_000 {
    let c = rec.get_i64_at(&count_slot).unwrap_or(0);
    rec.set_i64_at(&count_slot, c + 1).unwrap();
    rec.set_f64_at(&score_slot, c as f64 * 1.5).unwrap();
}
```

---

### `FieldRef<'a>`

**Definition**:
```rust
pub struct FieldRef<'a> {
    pub name_hash: u64,
    pub type_tag: u8,
    pub data: &'a [u8],
}
```

A zero-copy reference to a single field's raw bytes within a record buffer. Lifetime `'a` ties the reference to the record's buffer. Produced by `get_raw` and by `FieldIter`.

| Field | Type | Description |
|-------|------|-------------|
| `name_hash` | `u64` | xxh64 hash of the field name (seed 0). Field names are **not recoverable** from this hash. |
| `type_tag` | `u8` | One of the `TAG_*` constants. Determines how `data` should be interpreted. |
| `data` | `&'a [u8]` | Raw field bytes. Length is 0 for null, 1 for bool, 8 for numeric types, variable for strings and CBOR. |

Implements `Debug`, `Clone`, `Copy`.

---

### `FieldIter<'a>`

**Definition**:
```rust
pub struct FieldIter<'a> {
    pub record: SpookyRecord<'a>,
    pub pos: usize,
}
```

Iterator over all fields in a record in index order (sorted by name hash). Implements `Iterator<Item = FieldRef<'a>>` and `ExactSizeIterator`. Zero allocation — yields borrowed `FieldRef` values from the record buffer.

Obtained from `SpookyReadable::iter_fields()`.

**Example**:
```rust
use spooky_db_module::spooky_record::{SpookyReadable, SpookyRecord};
use spooky_db_module::serialization::from_bytes;
use spooky_db_module::types::{TAG_I64, TAG_STR};

let raw: Vec<u8> = /* record bytes */ vec![];
let (buf, count) = from_bytes(&raw).unwrap();
let record = SpookyRecord::new(buf, count);

// ExactSizeIterator: len() is available without consuming the iterator
let n = record.iter_fields().len();

for field in record.iter_fields() {
    match field.type_tag {
        TAG_I64 => { /* interpret field.data as 8 LE bytes */ }
        TAG_STR => { /* interpret field.data as UTF-8 */ }
        _ => {}
    }
}
```

---

## Persistence (`spooky_db_module::db`)

### `SpookyDb`

**Definition**: `pub struct SpookyDb`

Persistent record store backed by [redb](https://github.com/cberner/redb). Owns the database exclusively — no `Arc`, no `Mutex`. All write operations take `&mut self`.

**Internal layout**:
- `RECORDS_TABLE` (`&str → &[u8]`): serialized SpookyRecord bytes. Key format: `"table_name:record_id"`. Table names must not contain `':'`.
- `VERSION_TABLE` (`&str → u64`): optional version number per record. Same key format. Updated only when `version: Some(v)` is passed.
- `zsets` (`FastMap<SmolStr, ZSet>`): in-memory ZSet per table. Rebuilt on open from a full RECORDS_TABLE scan. All ZSet reads are pure memory — zero I/O.
- `row_cache` (`LruCache<(SmolStr, SmolStr), Vec<u8>>`): bounded LRU cache of record bytes. Populated on every Create/Update/bulk_load. Evicts LRU entries at capacity. Starts cold on open.

---

#### Construction

| Method | Signature | Description |
|--------|-----------|-------------|
| `new` | `pub fn new(path: impl AsRef<Path>) -> Result<Self, SpookyDbError>` | Open or create the database with default 10 000-record LRU cache. Rebuilds in-memory ZSets from a full redb scan. |
| `new_with_config` | `pub fn new_with_config(path: impl AsRef<Path>, config: SpookyDbConfig) -> Result<Self, SpookyDbError>` | Open or create with explicit configuration. |

**Startup cost**: `new` scans all records in `RECORDS_TABLE` to rebuild the ZSet — approximately 20–80 ms per million records on an SSD. The row cache starts cold; record bytes are not pre-loaded.

**Example**:
```rust
use spooky_db_module::db::{SpookyDb, SpookyDbConfig};
use std::num::NonZeroUsize;

// Default: 10_000-record cache
let mut db = SpookyDb::new("/tmp/mydb.redb").unwrap();

// Custom cache size
let config = SpookyDbConfig {
    cache_capacity: NonZeroUsize::new(50_000).unwrap(),
};
let mut db2 = SpookyDb::new_with_config("/tmp/mydb2.redb", config).unwrap();
```

---

#### Write Operations (`&mut self`)

**`apply_mutation`**

**Signature**:
```rust
pub fn apply_mutation(
    &mut self,
    table: &str,
    op: Operation,
    id: &str,
    data: Option<&[u8]>,
    version: Option<u64>,
) -> Result<(SmolStr, i64), SpookyDbError>
```

Apply a single mutation in its own write transaction (one fsync). `data` must be pre-serialized SpookyRecord bytes — use `from_cbor` or `serialize_into` before calling this. For `Delete`, pass `data: None`.

Atomicity guarantee: redb is written first. In-memory state (ZSet + row cache) is updated only after a successful `commit()`. A failed commit leaves in-memory state unchanged.

`version: None` leaves the existing version entry unchanged. Pass `version: Some(v)` on every mutation where conflict detection matters.

**Returns**: `(SmolStr::new(id), weight_delta)` — the record ID and the ZSet weight delta for this operation (`+1` for Create, `0` for Update, `-1` for Delete).

**Errors**: `SpookyDbError::InvalidKey` if `table` contains `':'`.

**Example**:
```rust
use spooky_db_module::db::{SpookyDb, Operation};
use spooky_db_module::serialization::from_cbor;

let mut db = SpookyDb::new("/tmp/db.redb").unwrap();
let cbor_val: cbor4ii::core::Value = /* parse CBOR */ todo!();
let (bytes, _) = from_cbor(&cbor_val).unwrap();
db.apply_mutation("users", Operation::Create, "alice", Some(&bytes), Some(1)).unwrap();
db.apply_mutation("users", Operation::Delete, "alice", None, None).unwrap();
```

---

**`apply_batch`**

**Signature**:
```rust
pub fn apply_batch(
    &mut self,
    mutations: Vec<DbMutation>,
) -> Result<BatchMutationResult, SpookyDbError>
```

Apply N mutations in a single write transaction (one fsync). This is the primary write API for high-throughput pipelines. Serialize all record bytes **before** calling this to minimize write-lock hold time.

Internally sorts mutations by table name to improve cache locality. In-memory state is updated only after a successful `commit()`.

**Returns**: `BatchMutationResult` containing per-table ZSet deltas, per-table content update sets, and a deduplicated list of changed table names.

**Errors**: `SpookyDbError::InvalidKey` if any table name contains `':'`. Validation happens before touching redb.

**Example**:
```rust
use spooky_db_module::db::{SpookyDb, DbMutation, Operation, BatchMutationResult};
use spooky_db_module::serialization::from_cbor;
use smol_str::SmolStr;

let mut db = SpookyDb::new("/tmp/db.redb").unwrap();
let cbor_val: cbor4ii::core::Value = /* parse CBOR */ todo!();
let (bytes, _) = from_cbor(&cbor_val).unwrap();

let result = db.apply_batch(vec![
    DbMutation {
        table: SmolStr::new("users"),
        id: SmolStr::new("u1"),
        op: Operation::Create,
        data: Some(bytes.clone()),
        version: Some(1),
    },
    DbMutation {
        table: SmolStr::new("users"),
        id: SmolStr::new("u2"),
        op: Operation::Create,
        data: Some(bytes),
        version: Some(1),
    },
]).unwrap();

assert_eq!(result.changed_tables.len(), 1);
assert_eq!(result.membership_deltas["users"].len(), 2);
```

---

**`bulk_load`**

**Signature**:
```rust
pub fn bulk_load(
    &mut self,
    records: Vec<BulkRecord>,
) -> Result<(), SpookyDbError>
```

Initial bulk load of pre-serialized records in a single write transaction. Sets every record's ZSet weight to 1. Use for startup hydration or snapshot restoration. All `BulkRecord.data` fields must be pre-serialized SpookyRecord bytes.

**Errors**: `SpookyDbError::InvalidKey` if any table name contains `':'`.

---

#### Read Operations (`&self`)

**`get_record_bytes`**

**Signature**: `pub fn get_record_bytes(&self, table: &str, id: &str) -> Option<Vec<u8>>`

Fetch a copy of the raw SpookyRecord bytes.

- **Fast path** (cache hit): `peek()` from the LRU row cache — zero I/O, ~50 ns.
- **Slow path** (cache miss): opens a redb read transaction — ~1–10 µs on warm OS cache.

Returns `None` if the record is absent from the ZSet (the ZSet is checked first to avoid unnecessary redb opens).

Cache misses do **not** populate the cache (requires `&self`). The cache is written only by Create/Update/`bulk_load` paths.

**Example**:
```rust
use spooky_db_module::serialization::from_bytes;
use spooky_db_module::spooky_record::{SpookyRecord, SpookyReadable};

let bytes = db.get_record_bytes("users", "alice").unwrap();
let (buf, count) = from_bytes(&bytes).unwrap();
let record = SpookyRecord::new(buf, count);
let age = record.get_i64("age");
```

---

**`get_row_record`**

**Signature**: `pub fn get_row_record<'a>(&'a self, table: &str, id: &str) -> Option<SpookyRecord<'a>>`

Zero-copy borrowed `SpookyRecord` for the view evaluation hot path. Returns `Some` only if the record is in the LRU row cache; returns `None` if the record does not exist **or** if it exists on disk but has been evicted from the cache.

**Cache miss fallback**: call `get_record_bytes` which reads from redb.

For the streaming pipeline hot path (write then read in the same tick), records are always in the cache — writes populate it immediately. Zero I/O, zero allocation.

**After reopen**: the LRU cache starts cold; `get_row_record` returns `None` for all records until they are written again. Use `get_record_bytes` (with its redb fallback) when reading after a cold start.

---

**`get_record_typed`**

**Signature**:
```rust
pub fn get_record_typed(
    &self,
    table: &str,
    id: &str,
    fields: &[&str],
) -> Result<Option<SpookyValue>, SpookyDbError>
```

Reconstruct a partial `SpookyValue::Object` from a stored record. Only fields whose names appear in `fields` are included. Field names not present in the record are silently skipped. Fields not listed in `fields` are silently omitted (field names are not stored in the binary format and cannot be recovered from hashes).

Returns `Ok(None)` if the record does not exist.

Use `get_record_bytes` + `SpookyReadable` accessors on the hot path. Use this for compatibility layers that need a named `SpookyValue`.

**Example**:
```rust
let val = db.get_record_typed("users", "alice", &["age", "active"]).unwrap().unwrap();
if let spooky_db_module::spooky_value::SpookyValue::Object(map) = val {
    let age = map.get("age"); // Some(SpookyValue::Number(...))
}
```

---

**`get_version`**

**Signature**: `pub fn get_version(&self, table: &str, id: &str) -> Result<Option<u64>, SpookyDbError>`

Retrieve the stored version number for a record from `VERSION_TABLE`. Returns `None` if the record has no version entry (either absent from ZSet, or written with `version: None`).

Fast path: absent from ZSet → returns `None` immediately without opening a redb transaction.

---

#### ZSet Operations (`&self`, pure memory)

**`get_table_zset`**

**Signature**: `pub fn get_table_zset(&self, table: &str) -> Option<&ZSet>`

Borrow the full in-memory ZSet for a table. Zero I/O. Returns `None` if the table has never had any records. The borrow is valid until the next `&mut self` call.

---

**`get_zset_weight`**

**Signature**: `pub fn get_zset_weight(&self, table: &str, id: &str) -> i64`

Weight for a single record. Returns `0` if absent (standard ZSet semantics). Returns `1` if present. Pure memory, zero I/O.

---

#### Table Operations (`&self` and `&mut self`)

**`table_exists`**

**Signature**: `pub fn table_exists(&self, table: &str) -> bool`

Returns `true` if the table has at least one record in the in-memory ZSet. O(1).

Note: `ensure_table` creates an empty ZSet entry; an empty entry causes `table_exists` to return `false` until the first record is inserted.

---

**`table_names`**

**Signature**: `pub fn table_names(&self) -> impl Iterator<Item = &SmolStr>`

Iterator over all known table names derived from in-memory ZSet keys. Pure memory, O(1) per item.

---

**`table_len`**

**Signature**: `pub fn table_len(&self, table: &str) -> usize`

Record count for a table. O(1) — ZSet entry count equals record count.

---

**`ensure_table`**

**Signature**: `pub fn ensure_table(&mut self, table: &str) -> Result<(), SpookyDbError>`

Pre-allocate the in-memory ZSet slot for a table without inserting any records. Ensures that subsequent `get_table_zset` calls return `Some(&ZSet)` rather than `None`. An ensured but empty table still causes `table_exists` to return `false`.

**Errors**: `SpookyDbError::InvalidKey` if `table` contains `':'`.

---

### Trait: `DbBackend`

**Definition**: `pub trait DbBackend`

Thin adapter trait for wiring `SpookyDb` against streaming pipeline code. `SpookyDb` implements `DbBackend`. The trait is object-safe (can be used as `Box<dyn DbBackend>`).

All write operations return `Result` — disk or corruption errors must never become silent no-ops.

| Method | `&self`/`&mut self` | Description |
|--------|---------------------|-------------|
| `get_table_zset` | `&self` | Zero-copy ZSet access. Zero I/O. |
| `get_record_bytes` | `&self` | Raw bytes, cache-first with redb fallback. Returns `None` if absent. |
| `get_row_record_bytes` | `&self` | Cache-only borrowed `&[u8]`. Returns `None` on cache miss. Default impl always returns `None`. |
| `ensure_table` | `&mut self` | Register an empty table. Errors on `':'` in name. |
| `apply_mutation` | `&mut self` | Single mutation: record write + ZSet update. |
| `apply_batch` | `&mut self` | Batch mutations in one transaction. |
| `bulk_load` | `&mut self` | Bulk initial load. |
| `get_zset_weight` | `&self` | Weight for one record. Returns 0 if absent. |
| `get_record_typed` | `&self` | Partial SpookyValue reconstruction from named fields. |

`get_row_record_bytes` has a default implementation returning `None`. Override it in backends that have an in-memory row cache (such as `SpookyDb`).

---

### `SpookyDbConfig`

**Definition**: `pub struct SpookyDbConfig`

Configuration for `SpookyDb::new_with_config`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `cache_capacity` | `NonZeroUsize` | `10_000` | Maximum number of records in the LRU row cache. When this limit is reached, the least-recently-written record is evicted. Evicted records remain on disk and are re-read on the next access. Setting capacity larger than total record count gives full-memory semantics without the startup pre-load cost. |

Implements `Default`.

---

### `Operation`

**Definition**: `pub enum Operation`

Describes the kind of mutation to apply. Used in `DbMutation` and `apply_mutation`.

| Variant | ZSet weight delta | Description |
|---------|-------------------|-------------|
| `Create` | `+1` | Record did not exist before. ZSet entry is created with weight 1. |
| `Update` | `0` | Record existed; bytes are replaced. ZSet weight is unchanged. |
| `Delete` | `-1` | Record removed. ZSet entry is removed. |

**`Operation::weight`**

**Signature**: `pub fn weight(&self) -> i64`

Returns the ZSet weight delta for this operation: `+1`, `0`, or `-1`.

Implements `Debug`, `Clone`, `Copy`, `PartialEq`, `Eq`.

---

### `DbMutation`

**Definition**: `pub struct DbMutation`

A single mutation ready for `apply_batch`. All `data` fields must be pre-serialized SpookyRecord bytes — serialize before calling `apply_batch` to minimize write-lock hold time.

| Field | Type | Description |
|-------|------|-------------|
| `table` | `SmolStr` | Target table name. Must not contain `':'`. |
| `id` | `SmolStr` | Record identifier. May contain `':'`. |
| `op` | `Operation` | Create, Update, or Delete. |
| `data` | `Option<Vec<u8>>` | Pre-serialized SpookyRecord bytes. `None` for `Delete`; `Some(bytes)` for `Create`/`Update`. |
| `version` | `Option<u64>` | Version to write to `VERSION_TABLE`. `None` leaves the existing version entry unchanged. |

---

### `BulkRecord`

**Definition**: `pub struct BulkRecord`

One record for `bulk_load`. `data` must be pre-serialized SpookyRecord bytes.

| Field | Type | Description |
|-------|------|-------------|
| `table` | `SmolStr` | Target table name. Must not contain `':'`. |
| `id` | `SmolStr` | Record identifier. |
| `data` | `Vec<u8>` | Pre-serialized SpookyRecord bytes (owned). |
| `version` | `Option<u64>` | Written to `VERSION_TABLE` when `Some`. Pass `None` to skip version tracking. |

---

### `BatchMutationResult`

**Definition**: `pub struct BatchMutationResult`

Return value of `apply_batch`. Contains all per-table deltas accumulated in a single pass. No extra allocations after the batch commit.

| Field | Type | Description |
|-------|------|-------------|
| `membership_deltas` | `FastMap<SmolStr, ZSet>` | Per-table ZSet weight deltas. Create mutations appear as `+1`; Delete as `-1`. Update mutations do not appear (weight delta is 0). |
| `content_updates` | `FastMap<SmolStr, FastHashSet<SmolStr>>` | Per-table set of record IDs whose bytes were written (Create or Update operations). |
| `changed_tables` | `Vec<SmolStr>` | Deduplicated list of tables with at least one mutation, in the order they first appeared after sort. |

**Usage in pipeline code**:
```rust
let result = db.apply_batch(mutations).unwrap();
for table in &result.changed_tables {
    if let Some(deltas) = result.membership_deltas.get(table) {
        // feed deltas into DBSP change set
    }
    if let Some(updated_ids) = result.content_updates.get(table) {
        // re-evaluate views for these IDs
    }
}
```

---

### `SpookyDbError`

**Definition**: `pub enum SpookyDbError`

Unified error type for all `SpookyDb` operations.

| Variant | When it occurs |
|---------|----------------|
| `Redb(redb::Error)` | Any redb storage, transaction, table, commit, or database error. Individual `From` impls exist for `redb::DatabaseError`, `redb::TransactionError`, `redb::TableError`, `redb::CommitError`, and `redb::StorageError` — all convert via `.into()` to `redb::Error`. |
| `Serialization(String)` | Record serialization or deserialization failure (wraps `RecordError`). |
| `InvalidKey(String)` | Table name contains `':'` or key format is otherwise invalid. |

Implements `Debug` and `Display` (via `thiserror`). Also implements `From<RecordError>` — any `?` on a `Result<_, RecordError>` inside db code converts automatically.

---

## Error Reference

### `RecordError`

**Definition**: `pub enum RecordError`

Error type for the serialization, deserialization, and record mutation layer.

| Variant | When it occurs |
|---------|----------------|
| `SerializationNotObject` | Attempted to serialize a non-Object `SpookyValue` through a path that requires an object. |
| `InvalidBuffer` | Buffer is too short (shorter than `HEADER_SIZE`), shorter than the minimum size implied by the field count in the header, or the field_count in the buffer is corrupt. |
| `TooManyFields` | Attempted to serialize or add a field that would exceed the 32-field hard limit. |
| `FieldNotFound` | A named field is absent from the record's index. |
| `TypeMismatch { expected: u8, actual: u8 }` | A typed getter or setter was called on a field with a different type tag (e.g., `set_i64` on a TAG_STR field). |
| `LengthMismatch { expected: usize, actual: usize }` | `set_str_exact` or `set_str_at` called with a string whose byte length differs from the stored length. `expected` is the stored length; `actual` is the new value's length. |
| `FieldExists` | `add_field` was called for a field name that already exists in the record. |
| `CborError(String)` | CBOR encoding or decoding failure. The string contains the underlying error message. |
| `UnknownTypeTag(u8)` | An unrecognised type tag was encountered in the buffer. |

Implements `Debug` and `Display` (via `thiserror`).

---

## Type Aliases

| Alias | Expands To | Module | Used For |
|-------|-----------|--------|----------|
| `ZSet` | `FastMap<RowKey, Weight>` | `db::types` | Per-table in-memory record membership map. |
| `RowKey` | `SmolStr` | `db::types` | Record identifier. |
| `Weight` | `i64` | `db::types` | ZSet weight. 1 = present, 0 = absent. |
| `TableName` | `SmolStr` | `db::types` | Table name. Must not contain `':'`. |
| `FastMap<K, V>` | `HashMap<K, V, BuildHasherDefault<FxHasher>>` | `db::types` | FxHasher-backed `HashMap`. Used for ZSet and batch result maps. |
| `FastHashSet<T>` | `HashSet<T, BuildHasherDefault<FxHasher>>` | `db::types` | FxHasher-backed `HashSet`. Used in `BatchMutationResult::content_updates`. |
| `FastMap<K, V>` (value layer) | `BTreeMap<K, V>` | `spooky_value` | **Different alias** — used as the inner map type in `SpookyValue::Object`. Not an FxHasher map. Import explicitly to avoid confusion. |

---

## Constants

All constants are defined in `spooky_db_module::types`.

| Constant | Value | Type | Meaning |
|----------|-------|------|---------|
| `TAG_NULL` | `0` | `u8` | Field type tag: null value. Zero data bytes. |
| `TAG_BOOL` | `1` | `u8` | Field type tag: boolean. Exactly 1 data byte (`0` = false, non-zero = true). |
| `TAG_I64` | `2` | `u8` | Field type tag: signed 64-bit integer. Exactly 8 data bytes, little-endian. |
| `TAG_F64` | `3` | `u8` | Field type tag: IEEE 754 64-bit float. Exactly 8 data bytes, little-endian. |
| `TAG_STR` | `4` | `u8` | Field type tag: UTF-8 string. Variable data length (raw bytes, no length prefix). |
| `TAG_NESTED_CBOR` | `5` | `u8` | Field type tag: nested array or object. Variable data length; CBOR-encoded. |
| `TAG_U64` | `6` | `u8` | Field type tag: unsigned 64-bit integer. Exactly 8 data bytes, little-endian. |
| `HEADER_SIZE` | `20` | `usize` | Byte size of the record header (4 bytes field_count + 16 bytes reserved). |
| `INDEX_ENTRY_SIZE` | `20` | `usize` | Byte size of one index entry (8 hash + 4 offset + 4 length + 1 tag + 3 padding). |
