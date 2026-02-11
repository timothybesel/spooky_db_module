use smol_str::SmolStr;
use xxhash_rust::xxh64::xxh64;

use crate::spooky_value::{FastMap, SpookyNumber, SpookyValue};

// ─── Type Tags ──────────────────────────────────────────────────────────────

pub const TAG_NULL: u8 = 0;
pub const TAG_BOOL: u8 = 1;
pub const TAG_I64: u8 = 2;
pub const TAG_F64: u8 = 3;
pub const TAG_U64: u8 = 4;
pub const TAG_STR: u8 = 5;
pub const TAG_NESTED_CBOR: u8 = 6;

// ─── Error ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum RecordError {
    NotAnObject,
    CborSerializeFailed(String),
    BufferTooSmall,
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::NotAnObject => write!(f, "serialize_record: expected Object"),
            RecordError::CborSerializeFailed(e) => write!(f, "CBOR serialize failed: {}", e),
            RecordError::BufferTooSmall => write!(f, "buffer too small for record"),
        }
    }
}

impl std::error::Error for RecordError {}

// ─── Binary Layout ──────────────────────────────────────────────────────────
//
//  ┌──────────────────────────────────────────────┐
//  │ Header (20 bytes)                            │
//  │   field_count: u32 (LE)                      │
//  │   _reserved: [u8; 16]                        │
//  ├──────────────────────────────────────────────┤
//  │ Index (20 bytes × field_count)               │
//  │   name_hash:   u64 (LE)    ← SORTED by hash │
//  │   data_offset: u32 (LE)                      │
//  │   data_length: u32 (LE)                      │
//  │   type_tag:    u8                            │
//  │   _padding:    [u8; 3]                       │
//  ├──────────────────────────────────────────────┤
//  │ Data (variable)                              │
//  │   field values packed sequentially           │
//  └──────────────────────────────────────────────┘

const HEADER_SIZE: usize = 20; // 4 + 16
const INDEX_ENTRY_SIZE: usize = 20; // 8 + 4 + 4 + 1 + 3

// ─── Writer ─────────────────────────────────────────────────────────────────

/// Pre-computed field metadata for serialization.
/// Stores offset/length/tag before writing, to enable sorted index output.
struct FieldMeta {
    name_hash: u64,
    data_offset: usize,
    data_length: usize,
    type_tag: u8,
}

/// Serialize a SpookyValue::Object into the hybrid binary format.
///
/// **Key improvement over v1**: The index is sorted by name_hash, enabling
/// O(log n) binary search lookups. Fields are serialized directly into the
/// output buffer with no intermediate Vec allocations per field.
pub fn serialize_record(data: &SpookyValue) -> Result<Vec<u8>, RecordError> {
    let map = match data {
        SpookyValue::Object(map) => map,
        _ => return Err(RecordError::NotAnObject),
    };

    let field_count = map.len();
    let index_size = field_count * INDEX_ENTRY_SIZE;
    let data_start = HEADER_SIZE + index_size;

    // ── Pass 1: compute hashes + measure data sizes ──
    // We collect (hash, key, value) so we can sort by hash before writing.
    let mut entries: Vec<(u64, &SpookyValue)> = Vec::with_capacity(field_count);
    let mut total_data_size: usize = 0;

    for (key, value) in map.iter() {
        let hash = xxh64(key.as_bytes(), 0);
        let field_size = measure_field(value);
        total_data_size += field_size;
        entries.push((hash, value));
    }

    // Sort by hash for binary search at read time
    entries.sort_unstable_by_key(|(hash, _)| *hash);

    // ── Pass 2: single allocation, write everything ──
    let total_size = data_start + total_data_size;
    let mut buf = vec![0u8; total_size];

    // Header
    buf[0..4].copy_from_slice(&(field_count as u32).to_le_bytes());

    // Write field data and collect metadata for index
    let mut data_offset = data_start;
    let mut metas: Vec<FieldMeta> = Vec::with_capacity(field_count);

    for (hash, value) in &entries {
        let start = data_offset;
        let tag = write_field(value, &mut buf, &mut data_offset)
            .map_err(|e| RecordError::CborSerializeFailed(e.to_string()))?;
        metas.push(FieldMeta {
            name_hash: *hash,
            data_offset: start,
            data_length: data_offset - start,
            type_tag: tag,
        });
    }

    // Write index entries (already in sorted hash order)
    for (i, meta) in metas.iter().enumerate() {
        let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
        buf[idx..idx + 8].copy_from_slice(&meta.name_hash.to_le_bytes());
        buf[idx + 8..idx + 12].copy_from_slice(&(meta.data_offset as u32).to_le_bytes());
        buf[idx + 12..idx + 16].copy_from_slice(&(meta.data_length as u32).to_le_bytes());
        buf[idx + 16] = meta.type_tag;
        // padding [idx+17..idx+20] already zero
    }

    Ok(buf)
}

/// Measure how many bytes a field will take without allocating.
#[inline]
fn measure_field(value: &SpookyValue) -> usize {
    match value {
        SpookyValue::Null => 0,
        SpookyValue::Bool(_) => 1,
        SpookyValue::Number(n) => match n {
            SpookyNumber::I64(_) | SpookyNumber::F64(_) | SpookyNumber::U64(_) => 8,
        },
        SpookyValue::Str(s) => s.len(),
        SpookyValue::Array(_) | SpookyValue::Object(_) => {
            // For nested CBOR, we must serialize to measure.
            // Use a counting writer to avoid allocation.
            let mut counter = CountingWriter(0);
            ciborium::into_writer(value, &mut counter).unwrap_or(());
            counter.0
        }
    }
}

/// A writer that only counts bytes without storing them.
struct CountingWriter(usize);

impl std::io::Write for CountingWriter {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }
    #[inline]
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Write a field directly into the output buffer at `offset`, advancing it.
/// Returns the type tag.
#[inline]
fn write_field(
    value: &SpookyValue,
    buf: &mut [u8],
    offset: &mut usize,
) -> Result<u8, Box<dyn std::error::Error>> {
    match value {
        SpookyValue::Null => Ok(TAG_NULL),
        SpookyValue::Bool(b) => {
            buf[*offset] = *b as u8;
            *offset += 1;
            Ok(TAG_BOOL)
        }
        SpookyValue::Number(n) => match n {
            SpookyNumber::I64(i) => {
                buf[*offset..*offset + 8].copy_from_slice(&i.to_le_bytes());
                *offset += 8;
                Ok(TAG_I64)
            }
            SpookyNumber::F64(f) => {
                buf[*offset..*offset + 8].copy_from_slice(&f.to_le_bytes());
                *offset += 8;
                Ok(TAG_F64)
            }
            SpookyNumber::U64(u) => {
                buf[*offset..*offset + 8].copy_from_slice(&u.to_le_bytes());
                *offset += 8;
                Ok(TAG_U64)
            }
        },
        SpookyValue::Str(s) => {
            let bytes = s.as_bytes();
            buf[*offset..*offset + bytes.len()].copy_from_slice(bytes);
            *offset += bytes.len();
            Ok(TAG_STR)
        }
        SpookyValue::Array(_) | SpookyValue::Object(_) => {
            // Write CBOR directly into the buffer slice
            let mut cursor = std::io::Cursor::new(&mut buf[*offset..]);
            ciborium::into_writer(value, &mut cursor)
                .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?;
            *offset += cursor.position() as usize;
            Ok(TAG_NESTED_CBOR)
        }
    }
}

// ─── Reader (zero-copy) ────────────────────────────────────────────────────

/// Zero-copy reader over a hybrid record byte slice.
///
/// **Key improvement over v1**: Uses binary search over sorted index for
/// O(log n) field lookups instead of O(n) linear scan.
pub struct SpookyRecord<'a> {
    bytes: &'a [u8],
    field_count: u32,
}

/// A raw field reference — no deserialization yet.
#[derive(Debug, Clone, Copy)]
pub struct FieldRef<'a> {
    pub name_hash: u64,
    pub type_tag: u8,
    pub data: &'a [u8],
}

impl<'a> SpookyRecord<'a> {
    /// Wrap a byte slice as a SpookyRecord. No copies, no parsing.
    #[inline]
    pub fn from_bytes(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < HEADER_SIZE {
            return None;
        }
        let field_count = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
        let min_size = HEADER_SIZE + field_count as usize * INDEX_ENTRY_SIZE;
        if bytes.len() < min_size {
            return None;
        }
        Some(SpookyRecord { bytes, field_count })
    }

    #[inline]
    pub fn field_count(&self) -> u32 {
        self.field_count
    }

    /// Read a raw index entry by position (zero-copy).
    #[inline]
    fn index_entry(&self, i: usize) -> Option<FieldRef<'a>> {
        if i >= self.field_count as usize {
            return None;
        }
        let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
        let name_hash = u64::from_le_bytes(self.bytes[idx..idx + 8].try_into().ok()?);
        let data_offset =
            u32::from_le_bytes(self.bytes[idx + 8..idx + 12].try_into().ok()?) as usize;
        let data_length =
            u32::from_le_bytes(self.bytes[idx + 12..idx + 16].try_into().ok()?) as usize;
        let type_tag = self.bytes[idx + 16];

        let data = self.bytes.get(data_offset..data_offset + data_length)?;
        Some(FieldRef {
            name_hash,
            type_tag,
            data,
        })
    }

    /// Read just the hash from index entry `i` without constructing full FieldRef.
    #[inline]
    fn index_hash(&self, i: usize) -> u64 {
        let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
        u64::from_le_bytes(self.bytes[idx..idx + 8].try_into().unwrap())
    }

    /// Look up a field by name — O(log n) binary search over sorted index.
    ///
    /// Falls back to linear scan for field_count <= 4 where branch prediction
    /// and cache locality make linear faster than binary search overhead.
    pub fn get_raw(&self, name: &str) -> Option<FieldRef<'a>> {
        let hash = xxh64(name.as_bytes(), 0);
        let n = self.field_count as usize;

        if n <= 4 {
            // Linear scan: faster for tiny records due to no branch overhead
            for i in 0..n {
                if self.index_hash(i) == hash {
                    return self.index_entry(i);
                }
            }
            return None;
        }

        // Binary search on sorted hashes
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let mid_hash = self.index_hash(mid);
            match mid_hash.cmp(&hash) {
                std::cmp::Ordering::Equal => return self.index_entry(mid),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }

    /// Look up a field and deserialize it into a SpookyValue.
    #[inline]
    pub fn get_field(&self, name: &str) -> Option<SpookyValue> {
        let field = self.get_raw(name)?;
        decode_field(field)
    }

    /// Get a string field without allocating a SpookyValue (zero-copy).
    #[inline]
    pub fn get_str(&self, name: &str) -> Option<&'a str> {
        let field = self.get_raw(name)?;
        if field.type_tag != TAG_STR {
            return None;
        }
        std::str::from_utf8(field.data).ok()
    }

    /// Get an i64 field without allocating.
    #[inline]
    pub fn get_i64(&self, name: &str) -> Option<i64> {
        let field = self.get_raw(name)?;
        if field.type_tag != TAG_I64 {
            return None;
        }
        Some(i64::from_le_bytes(field.data.try_into().ok()?))
    }

    /// Get a u64 field without allocating.
    #[inline]
    pub fn get_u64(&self, name: &str) -> Option<u64> {
        let field = self.get_raw(name)?;
        if field.type_tag != TAG_U64 {
            return None;
        }
        Some(u64::from_le_bytes(field.data.try_into().ok()?))
    }

    /// Get an f64 field without allocating.
    #[inline]
    pub fn get_f64(&self, name: &str) -> Option<f64> {
        let field = self.get_raw(name)?;
        if field.type_tag != TAG_F64 {
            return None;
        }
        Some(f64::from_le_bytes(field.data.try_into().ok()?))
    }

    /// Get a bool field without allocating.
    #[inline]
    pub fn get_bool(&self, name: &str) -> Option<bool> {
        let field = self.get_raw(name)?;
        if field.type_tag != TAG_BOOL {
            return None;
        }
        Some(field.data.first()? != &0)
    }

    /// Get any numeric field as f64 (works for I64, U64, F64).
    #[inline]
    pub fn get_number_as_f64(&self, name: &str) -> Option<f64> {
        let field = self.get_raw(name)?;
        match field.type_tag {
            TAG_I64 => {
                let bytes: [u8; 8] = field.data.try_into().ok()?;
                Some(i64::from_le_bytes(bytes) as f64)
            }
            TAG_U64 => {
                let bytes: [u8; 8] = field.data.try_into().ok()?;
                Some(u64::from_le_bytes(bytes) as f64)
            }
            TAG_F64 => {
                let bytes: [u8; 8] = field.data.try_into().ok()?;
                Some(f64::from_le_bytes(bytes))
            }
            _ => None,
        }
    }

    /// Check if a field exists without deserializing its value.
    #[inline]
    pub fn has_field(&self, name: &str) -> bool {
        self.get_raw(name).is_some()
    }

    /// Get the type tag for a field without deserializing.
    #[inline]
    pub fn field_type(&self, name: &str) -> Option<u8> {
        self.get_raw(name).map(|f| f.type_tag)
    }

    /// Reconstruct the full SpookyValue::Object from the binary.
    /// This is the slow path — use get_field() for selective access.
    pub fn to_spooky_value(&self, field_names: &[&str]) -> SpookyValue {
        let mut map = FastMap::new();
        for name in field_names {
            if let Some(val) = self.get_field(name) {
                map.insert(SmolStr::from(*name), val);
            }
        }
        SpookyValue::Object(map)
    }

    /// Iterate over all raw fields (zero-copy).
    pub fn iter_fields(&'a self) -> FieldIter<'a> {
        FieldIter {
            record: self,
            pos: 0,
        }
    }
}

/// Decode a raw field reference into a SpookyValue.
#[inline]
fn decode_field(field: FieldRef) -> Option<SpookyValue> {
    Some(match field.type_tag {
        TAG_NULL => SpookyValue::Null,
        TAG_BOOL => SpookyValue::Bool(*field.data.first()? != 0),
        TAG_I64 => {
            let bytes: [u8; 8] = field.data.try_into().ok()?;
            SpookyValue::Number(SpookyNumber::I64(i64::from_le_bytes(bytes)))
        }
        TAG_F64 => {
            let bytes: [u8; 8] = field.data.try_into().ok()?;
            SpookyValue::Number(SpookyNumber::F64(f64::from_le_bytes(bytes)))
        }
        TAG_U64 => {
            let bytes: [u8; 8] = field.data.try_into().ok()?;
            SpookyValue::Number(SpookyNumber::U64(u64::from_le_bytes(bytes)))
        }
        TAG_STR => SpookyValue::Str(SmolStr::from(std::str::from_utf8(field.data).ok()?)),
        TAG_NESTED_CBOR => {
            let cbor_val: ciborium::Value = ciborium::from_reader(field.data).ok()?;
            SpookyValue::from(cbor_val)
        }
        _ => return None,
    })
}

// ─── Iterator ───────────────────────────────────────────────────────────────

pub struct FieldIter<'a> {
    record: &'a SpookyRecord<'a>,
    pos: usize,
}

impl<'a> Iterator for FieldIter<'a> {
    type Item = FieldRef<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.record.field_count as usize {
            return None;
        }
        let entry = self.record.index_entry(self.pos)?;
        self.pos += 1;
        Some(entry)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.record.field_count as usize - self.pos;
        (remaining, Some(remaining))
    }
}

impl<'a> ExactSizeIterator for FieldIter<'a> {}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_record() -> SpookyValue {
        let mut map = FastMap::new();
        map.insert(SmolStr::from("id"), SpookyValue::from("user:123"));
        map.insert(SmolStr::from("name"), SpookyValue::from("Alice"));
        map.insert(SmolStr::from("age"), SpookyValue::from(30i64));
        map.insert(SmolStr::from("score"), SpookyValue::from(99.5f64));
        map.insert(SmolStr::from("active"), SpookyValue::from(true));
        map.insert(SmolStr::from("version"), SpookyValue::from(42u64));
        SpookyValue::Object(map)
    }

    #[test]
    fn test_roundtrip_flat_fields() {
        let original = make_test_record();
        let bytes = serialize_record(&original).unwrap();
        let record = SpookyRecord::from_bytes(&bytes).unwrap();

        assert_eq!(record.field_count(), 6);
        assert_eq!(record.get_str("id"), Some("user:123"));
        assert_eq!(record.get_str("name"), Some("Alice"));
        assert_eq!(record.get_i64("age"), Some(30));
        assert_eq!(record.get_f64("score"), Some(99.5));
        assert_eq!(record.get_bool("active"), Some(true));
        assert_eq!(record.get_u64("version"), Some(42));
    }

    #[test]
    fn test_missing_field() {
        let original = make_test_record();
        let bytes = serialize_record(&original).unwrap();
        let record = SpookyRecord::from_bytes(&bytes).unwrap();

        assert!(record.get_raw("nonexistent").is_none());
        assert!(record.get_str("nonexistent").is_none());
        assert!(!record.has_field("nonexistent"));
    }

    #[test]
    fn test_has_field() {
        let original = make_test_record();
        let bytes = serialize_record(&original).unwrap();
        let record = SpookyRecord::from_bytes(&bytes).unwrap();

        assert!(record.has_field("id"));
        assert!(record.has_field("age"));
        assert!(!record.has_field("missing"));
    }

    #[test]
    fn test_get_number_as_f64() {
        let original = make_test_record();
        let bytes = serialize_record(&original).unwrap();
        let record = SpookyRecord::from_bytes(&bytes).unwrap();

        assert_eq!(record.get_number_as_f64("age"), Some(30.0));
        assert_eq!(record.get_number_as_f64("score"), Some(99.5));
        assert_eq!(record.get_number_as_f64("version"), Some(42.0));
        assert_eq!(record.get_number_as_f64("name"), None);
    }

    #[test]
    fn test_nested_cbor() {
        let mut map = FastMap::new();
        let mut inner = FastMap::new();
        inner.insert(SmolStr::from("city"), SpookyValue::from("Berlin"));
        map.insert(SmolStr::from("address"), SpookyValue::Object(inner));
        map.insert(
            SmolStr::from("tags"),
            SpookyValue::Array(vec![SpookyValue::from("a"), SpookyValue::from("b")]),
        );
        let obj = SpookyValue::Object(map);

        let bytes = serialize_record(&obj).unwrap();
        let record = SpookyRecord::from_bytes(&bytes).unwrap();

        let addr = record.get_field("address").unwrap();
        assert_eq!(addr.get("city").and_then(|v| v.as_str()), Some("Berlin"));

        let tags = record.get_field("tags").unwrap();
        let arr = tags.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn test_not_an_object() {
        let val = SpookyValue::from("not an object");
        assert!(serialize_record(&val).is_err());
    }

    #[test]
    fn test_null_field() {
        let mut map = FastMap::new();
        map.insert(SmolStr::from("nothing"), SpookyValue::Null);
        let obj = SpookyValue::Object(map);

        let bytes = serialize_record(&obj).unwrap();
        let record = SpookyRecord::from_bytes(&bytes).unwrap();
        assert_eq!(record.get_field("nothing"), Some(SpookyValue::Null));
    }

    #[test]
    fn test_iter_fields() {
        let original = make_test_record();
        let bytes = serialize_record(&original).unwrap();
        let record = SpookyRecord::from_bytes(&bytes).unwrap();

        let fields: Vec<_> = record.iter_fields().collect();
        assert_eq!(fields.len(), 6);
    }
}
