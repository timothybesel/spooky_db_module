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

// ─── Binary Layout ──────────────────────────────────────────────────────────
//
//  ┌──────────────────────────────────────────────┐
//  │ Header (20 bytes)                            │
//  │   field_count: u32 (LE)                      │
//  │   _reserved: [u8; 16]                        │
//  ├──────────────────────────────────────────────┤
//  │ Index (20 bytes × field_count)               │
//  │   name_hash:   u64 (LE)                      │
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

/// Serialize a SpookyValue::Object into the hybrid binary format.
/// Flat fields are stored as native bytes, nested objects/arrays as CBOR.
pub fn serialize_record(data: &SpookyValue) -> Vec<u8> {
    let map = match data {
        SpookyValue::Object(map) => map,
        _ => panic!("serialize_record: expected Object"),
    };

    let field_count = map.len();
    let index_size = field_count * INDEX_ENTRY_SIZE;
    let data_start = HEADER_SIZE + index_size;

    // Pre-serialize all field values to calculate total size
    let mut fields: Vec<(u64, Vec<u8>, u8)> = Vec::with_capacity(field_count);
    let mut total_data_size: usize = 0;

    for (key, value) in map.iter() {
        let hash = xxh64(key.as_bytes(), 0);
        let (bytes, tag) = serialize_field(value);
        total_data_size += bytes.len();
        fields.push((hash, bytes, tag));
    }

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

        // Index entry (no unsafe needed)
        buf[idx..idx + 8].copy_from_slice(&hash.to_le_bytes());
        buf[idx + 8..idx + 12].copy_from_slice(&(data_offset as u32).to_le_bytes());
        buf[idx + 12..idx + 16].copy_from_slice(&(data.len() as u32).to_le_bytes());
        buf[idx + 16] = *tag;
        // padding [idx+17..idx+20] already zero

        // Field data
        buf[data_offset..data_offset + data.len()].copy_from_slice(data);
        data_offset += data.len();
    }

    buf
}

fn serialize_field(value: &SpookyValue) -> (Vec<u8>, u8) {
    match value {
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
            ciborium::into_writer(value, &mut buf).expect("CBOR serialize failed");
            (buf, TAG_NESTED_CBOR)
        }
    }
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

impl<'a> SpookyRecord<'a> {
    /// Wrap a byte slice as a SpookyRecord. No copies, no parsing.
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

    /// Look up a field by name — O(n) scan over index entries.
    /// For small field counts (typical records <50 fields) this is faster than a HashMap.
    pub fn get_raw(&self, name: &str) -> Option<FieldRef<'a>> {
        let hash = xxh64(name.as_bytes(), 0);
        for i in 0..self.field_count as usize {
            let entry = self.index_entry(i)?;
            if entry.name_hash == hash {
                return Some(entry);
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

/// Decode a raw field reference into a SpookyValue
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
