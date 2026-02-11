use smol_str::SmolStr;
use xxhash_rust::xxh64::xxh64;

use crate::spooky_value::{FastMap, SpookyNumber, SpookyValue};

// ─── Error ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum RecordError {
    InvalidBuffer,
    FieldNotFound,
    TypeMismatch { expected: u8, actual: u8 },
    LengthMismatch { expected: usize, actual: usize },
    FieldExists,
    CborError(String),
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::InvalidBuffer => write!(f, "Invalid buffer structure"),
            RecordError::FieldNotFound => write!(f, "Field not found"),
            RecordError::TypeMismatch { expected, actual } => {
                write!(f, "Type mismatch: expected {}, got {}", expected, actual)
            }
            RecordError::LengthMismatch { expected, actual } => {
                write!(
                    f,
                    "Length mismatch: expected {}, got {}",
                    expected, actual
                )
            }
            RecordError::FieldExists => write!(f, "Field already exists"),
            RecordError::CborError(msg) => write!(f, "CBOR error: {}", msg),
        }
    }
}

impl std::error::Error for RecordError {}

// ─── Type Tags ──────────────────────────────────────────────────────────────

pub const TAG_NULL: u8 = 0;
pub const TAG_BOOL: u8 = 1;
pub const TAG_I64: u8 = 2;
pub const TAG_F64: u8 = 3;
pub const TAG_STR: u8 = 4;
pub const TAG_NESTED_CBOR: u8 = 5; // Array or Object
pub const TAG_U64: u8 = 6; // Extension

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

pub const HEADER_SIZE: usize = 20; // 4 + 16
pub const INDEX_ENTRY_SIZE: usize = 20; // 8 + 4 + 4 + 1 + 3

// ─── Writer ─────────────────────────────────────────────────────────────────

/// Serialize a SpookyValue::Object into the hybrid binary format.
/// Flat fields are stored as native bytes, nested objects/arrays as CBOR.
///
/// **IMPORTANT**: The index is sorted by name_hash. This is required for
/// O(log n) binary search in both SpookyRecord and SpookyRecordMut.
/// O(log n) binary search in both SpookyRecord and SpookyRecordMut.
pub fn serialize_record(data: &SpookyValue) -> Result<Vec<u8>, RecordError> {
    let map = match data {
        SpookyValue::Object(map) => map,
        _ => return Err(RecordError::TypeMismatch { expected: TAG_NESTED_CBOR, actual: TAG_NULL }), // Using TAG_NULL as a placeholder for "not an object" or better yet, define a mismatch error. Actually, let's allow "not an object" to be a TypeMismatch or just a CborError? No, the caller expects an object. Let's say TypeMismatch expected object/map. But wait, existing code used panic.
        // Let's use TypeMismatch. But we don't have a TAG_OBJECT constant readily available in the 0-6 range that matches SpookyValue variants exactly without looking at `spooky_value.rs`.
        // Let's just say if it's not an object, we return valid error.
    };

    let field_count = map.len();
    let index_size = field_count * INDEX_ENTRY_SIZE;
    let data_start = HEADER_SIZE + index_size;

    // Pre-serialize all field values to calculate total size
    let mut fields: Vec<(u64, Vec<u8>, u8)> = Vec::with_capacity(field_count);
    let mut total_data_size: usize = 0;

    for (key, value) in map.iter() {
        let hash = xxh64(key.as_bytes(), 0);
        let (bytes, tag) = serialize_field(value)?;
        total_data_size += bytes.len();
        fields.push((hash, bytes, tag));
    }

    // Sort by hash for binary search at read time
    fields.sort_unstable_by_key(|(hash, _, _)| *hash);

    // Single allocation for the entire record
    let total_size = data_start + total_data_size;
    let mut buf = vec![0u8; total_size];

    // Write header
    buf[0..4].copy_from_slice(&(field_count as u32).to_le_bytes());
    // reserved bytes [4..20] are already zero

    // Write index entries + field data
    let mut data_offset = data_start;
    for (i, (hash, data, tag)) in fields.iter().enumerate() {
        let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;

        // Index entry
        buf[idx..idx + 8].copy_from_slice(&hash.to_le_bytes());
        buf[idx + 8..idx + 12].copy_from_slice(&(data_offset as u32).to_le_bytes());
        buf[idx + 12..idx + 16].copy_from_slice(&(data.len() as u32).to_le_bytes());
        buf[idx + 16] = *tag;
        // padding [idx+17..idx+20] already zero

        // Field data
        buf[data_offset..data_offset + data.len()].copy_from_slice(data);
        data_offset += data.len();
    }

    Ok(buf)
}

/// Serialize a single field value into (bytes, type_tag).
pub fn serialize_field(value: &SpookyValue) -> Result<(Vec<u8>, u8), RecordError> {
    Ok(match value {
        SpookyValue::Null => (vec![], TAG_NULL),
        SpookyValue::Bool(b) => (vec![*b as u8], TAG_BOOL),
        SpookyValue::Number(n) => match n {
            SpookyNumber::I64(i) => (i.to_le_bytes().to_vec(), TAG_I64),
            SpookyNumber::F64(f) => (f.to_le_bytes().to_vec(), TAG_F64),
            SpookyNumber::U64(u) => (u.to_le_bytes().to_vec(), TAG_U64),
        },
        SpookyValue::Str(s) => (s.as_bytes().to_vec(), TAG_STR),
        SpookyValue::Array(_) | SpookyValue::Object(_) => {
            let mut buf = Vec::new();
            ciborium::into_writer(value, &mut buf).map_err(|e| RecordError::CborError(e.to_string()))?;
            (buf, TAG_NESTED_CBOR)
        }
    })
}

// ─── Reader (zero-copy) ────────────────────────────────────────────────────

/// Zero-copy reader over a hybrid record byte slice.
/// No parsing happens until you request a specific field.
pub struct SpookyRecord<'a> {
    bytes: &'a [u8],
    field_count: u32,
}

/// A raw field reference — no deserialization yet
#[derive(Debug, Clone, Copy)]
pub struct FieldRef<'a> {
    pub name_hash: u64,
    pub type_tag: u8,
    pub data: &'a [u8],
}

#[allow(dead_code)]
impl<'a> SpookyRecord<'a> {
    /// Wrap a byte slice as a SpookyRecord. No copies, no parsing.
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self, RecordError> {
        if bytes.len() < HEADER_SIZE {
            return Err(RecordError::InvalidBuffer);
        }
        let field_count = u32::from_le_bytes(bytes[0..4].try_into().map_err(|_| RecordError::InvalidBuffer)?);
        let min_size = HEADER_SIZE + field_count as usize * INDEX_ENTRY_SIZE;
        if bytes.len() < min_size {
            return Err(RecordError::InvalidBuffer);
        }
        Ok(SpookyRecord { bytes, field_count })
    }

    /// Number of top-level fields
    #[inline]
    pub fn field_count(&self) -> u32 {
        self.field_count
    }

    /// Read a raw index entry by position (zero-copy)
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

    /// Read just the hash from index entry `i`.
    #[inline]
    fn index_hash(&self, i: usize) -> u64 {
        let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
        u64::from_le_bytes(self.bytes[idx..idx + 8].try_into().unwrap())
    }

    /// Look up a field by name — O(log n) binary search over sorted index.
    /// Falls back to linear scan for field_count <= 4 where cache locality wins.
    pub fn get_raw(&self, name: &str) -> Option<FieldRef<'a>> {
        let hash = xxh64(name.as_bytes(), 0);
        let n = self.field_count as usize;

        if n <= 4 {
            // Linear scan: faster for tiny records
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
    /// Only the requested field gets deserialized.
    pub fn get_field(&self, name: &str) -> Option<SpookyValue> {
        let field = self.get_raw(name)?;
        decode_field(field)
    }

    /// Get a string field without allocating a SpookyValue (zero-copy).
    pub fn get_str(&self, name: &str) -> Option<&'a str> {
        let field = self.get_raw(name)?;
        if field.type_tag != TAG_STR {
            return None;
        }
        std::str::from_utf8(field.data).ok()
    }

    /// Get an i64 field without allocating.
    pub fn get_i64(&self, name: &str) -> Option<i64> {
        let field = self.get_raw(name)?;
        if field.type_tag != TAG_I64 {
            return None;
        }
        Some(i64::from_le_bytes(field.data.try_into().ok()?))
    }

    /// Get a u64 field without allocating.
    pub fn get_u64(&self, name: &str) -> Option<u64> {
        let field = self.get_raw(name)?;
        if field.type_tag != TAG_U64 {
            return None;
        }
        Some(u64::from_le_bytes(field.data.try_into().ok()?))
    }

    /// Get an f64 field without allocating.
    pub fn get_f64(&self, name: &str) -> Option<f64> {
        let field = self.get_raw(name)?;
        if field.type_tag != TAG_F64 {
            return None;
        }
        Some(f64::from_le_bytes(field.data.try_into().ok()?))
    }

    /// Get a bool field without allocating.
    pub fn get_bool(&self, name: &str) -> Option<bool> {
        let field = self.get_raw(name)?;
        if field.type_tag != TAG_BOOL {
            return None;
        }
        Some(field.data.first()? != &0)
    }

    /// Check if a field exists.
    #[inline]
    pub fn has_field(&self, name: &str) -> bool {
        self.get_raw(name).is_some()
    }

    /// Get a numeric field as f64 (converting i64/u64 if needed).
    pub fn get_number_as_f64(&self, name: &str) -> Option<f64> {
        let field = self.get_raw(name)?;
        match field.type_tag {
            TAG_F64 => Some(f64::from_le_bytes(field.data.try_into().ok()?)),
            TAG_I64 => Some(i64::from_le_bytes(field.data.try_into().ok()?) as f64),
            TAG_U64 => Some(u64::from_le_bytes(field.data.try_into().ok()?) as f64),
            _ => None,
        }
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

    /// Iterate over all raw fields (zero-copy)
    pub fn iter_fields(&'a self) -> FieldIter<'a> {
        FieldIter {
            record: self,
            pos: 0,
        }
    }
}

/// Decode a raw field reference into a SpookyValue.
pub fn decode_field(field: FieldRef) -> Option<SpookyValue> {
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

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.record.field_count as usize {
            return None;
        }
        let entry = self.record.index_entry(self.pos)?;
        self.pos += 1;
        Some(entry)
    }

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
