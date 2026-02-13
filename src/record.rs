use super::deserialization::decode_field;
use super::error::RecordError;
use super::spooky_value::SpookyValue;
use super::types::*;
use xxhash_rust::xxh64::xxh64;

// ─── Reader (zero-copy) ────────────────────────────────────────────────────
/// Zero-copy reader over a hybrid record byte slice.
/// No parsing happens until you request a specific field.
#[derive(Debug, Clone, Copy)]
pub struct SpookyRecord<'a> {
    pub data_buf: &'a [u8],
    pub field_count: usize,
}

impl<'a> SpookyRecord<'a> {
    // ════════════════════════════════════════════════════════════════════════
    // Internal: index access
    // ════════════════════════════════════════════════════════════════════════
    #[inline]
    pub fn new(buf: &'a [u8], field_count: usize) -> Self {
        Self {
            data_buf: buf,
            field_count,
        }
    }
    /// Read the index entry metadata at position `i`.
    #[inline]
    pub fn read_index(&self, i: usize) -> Option<IndexEntry> {
        if i >= self.field_count {
            return None;
        }
        let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
        let name_ptr = self.data_buf[idx..idx + 8].as_ptr() as *const u64;
        let offset_ptr = self.data_buf[idx + 8..idx + 12].as_ptr() as *const u32;
        let length_ptr = self.data_buf[idx + 12..idx + 16].as_ptr() as *const u32;

        Some(IndexEntry {
            name_hash: u64::from_le(unsafe { name_ptr.read_unaligned() }),
            data_offset: u32::from_le(unsafe { offset_ptr.read_unaligned() }) as usize,
            data_len: u32::from_le(unsafe { length_ptr.read_unaligned() }) as usize,
            type_tag: self.data_buf[idx + 16],
        })
    }

    /// Read just the hash at index position `i`.
    #[inline]
    fn read_hash(&self, i: usize) -> u64 {
        let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
        // SAFETY: caller ensures i < field_count, validated at construction
        let ptr = self.data_buf[idx..].as_ptr() as *const u64;
        u64::from_le(unsafe { ptr.read_unaligned() })
    }

    #[inline]
    fn linear_hash_search(&self, n: usize, hash: u64) -> Result<(usize, IndexEntry), RecordError> {
        for i in 0..n {
            if self.read_hash(i) == hash {
                return self
                    .read_index(i)
                    .map(|meta| (i, meta))
                    .ok_or(RecordError::InvalidBuffer);
            }
        }
        return Err(RecordError::FieldNotFound);
    }

    fn binary_hash_search(&self, n: usize, hash: u64) -> Result<(usize, IndexEntry), RecordError> {
        // Binary search on sorted hashes
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let mid_hash = self.read_hash(mid);
            match mid_hash.cmp(&hash) {
                std::cmp::Ordering::Equal => {
                    let meta = self.read_index(mid).ok_or(RecordError::InvalidBuffer)?;
                    return Ok((mid, meta));
                }
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        Err(RecordError::FieldNotFound)
    }

    /// Find a field by name. Returns (index_position, IndexEntry).
    fn find_field(&self, name: &str) -> Result<(usize, IndexEntry), RecordError> {
        let hash = xxh64(name.as_bytes(), 0);
        let n = self.field_count as usize;

        if n == 0 {
            return Err(RecordError::FieldNotFound);
        }
        if n <= 4 {
            return self.linear_hash_search(n, hash);
        }
        return self.binary_hash_search(n, hash);
    }

    // ════════════════════════════════════════════════════════════════════════
    // Read access (zero-copy on the mutable buffer)
    // ════════════════════════════════════════════════════════════════════════

    /// Get a string field (zero-copy).
    #[inline]
    pub fn get_str(&self, name: &str) -> Option<&str> {
        let (_, meta) = self.find_field(name).ok()?;
        if meta.type_tag != TAG_STR {
            return None;
        }
        std::str::from_utf8(&self.data_buf[meta.data_offset..meta.data_offset + meta.data_len]).ok()
    }

    /// Get an i64 field.
    #[inline]
    pub fn get_i64(&self, name: &str) -> Option<i64> {
        let (_, meta) = self.find_field(name).ok()?;
        if meta.type_tag != TAG_I64 || meta.data_len != 8 {
            return None;
        }
        Some(i64::from_le_bytes(
            self.data_buf[meta.data_offset..meta.data_offset + 8]
                .try_into()
                .ok()?,
        ))
    }

    /// Get a u64 field.
    #[inline]
    pub fn get_u64(&self, name: &str) -> Option<u64> {
        let (_, meta) = self.find_field(name).ok()?;
        if meta.type_tag != TAG_U64 || meta.data_len != 8 {
            return None;
        }
        Some(u64::from_le_bytes(
            self.data_buf[meta.data_offset..meta.data_offset + 8]
                .try_into()
                .ok()?,
        ))
    }

    /// Get an f64 field.
    #[inline]
    pub fn get_f64(&self, name: &str) -> Option<f64> {
        let (_, meta) = self.find_field(name).ok()?;
        if meta.type_tag != TAG_F64 || meta.data_len != 8 {
            return None;
        }
        Some(f64::from_le_bytes(
            self.data_buf[meta.data_offset..meta.data_offset + 8]
                .try_into()
                .ok()?,
        ))
    }

    /// Get a bool field.
    #[inline]
    pub fn get_bool(&self, name: &str) -> Option<bool> {
        let (_, meta) = self.find_field(name).ok()?;
        if meta.type_tag != TAG_BOOL || meta.data_len != 1 {
            return None;
        }
        Some(self.data_buf[meta.data_offset] != 0)
    }

    /// Get raw field reference (zero-copy).
    pub fn get_raw(&self, name: &str) -> Option<FieldRef<'_>> {
        let (_, meta) = self.find_field(name).ok()?;
        let data = &self.data_buf[meta.data_offset..meta.data_offset + meta.data_len];
        Some(FieldRef {
            name_hash: meta.name_hash,
            type_tag: meta.type_tag,
            data,
        })
    }

    ///TODO: make it generic
    /// Get any field as a SpookyValue (deserializes nested CBOR if needed).
    pub fn get_field(&self, name: &str) -> Option<SpookyValue> {
        let field = self.get_raw(name)?;
        decode_field(field)
    }

    /// Get a numeric field as f64 (converting i64/u64 if needed).
    pub fn get_number_as_f64(&self, name: &str) -> Option<f64> {
        let (_, meta) = self.find_field(name).ok()?;
        match meta.type_tag {
            TAG_F64 | TAG_I64 | TAG_U64 if meta.data_len == 8 => {}
            _ => return None,
        }
        let bytes: [u8; 8] = self.data_buf[meta.data_offset..meta.data_offset + 8]
            .try_into()
            .ok()?;
        match meta.type_tag {
            TAG_F64 => Some(f64::from_le_bytes(bytes)),
            TAG_I64 => Some(i64::from_le_bytes(bytes) as f64),
            TAG_U64 => Some(u64::from_le_bytes(bytes) as f64),
            _ => unreachable!(),
        }
    }

    /// Convert to SpookyValue (iterator-based full conversion placeholder).
    /// Note: Keys are not recoverable from hashes in the current format.
    pub fn to_value(&self) -> SpookyValue {
        SpookyValue::Null // Placeholder as per parity plan constraint
    }

    /// Check if a field exists.
    #[inline]
    pub fn has_field(&self, name: &str) -> bool {
        self.find_field(name).is_ok()
    }

    /// Get the type tag for a field.
    #[inline]
    pub fn field_type(&self, name: &str) -> Option<u8> {
        self.find_field(name).ok().map(|(_, m)| m.type_tag)
    }

    /// Number of fields.
    #[inline]
    pub fn field_count(&self) -> usize {
        self.field_count
    }

    /// Iterate over all raw fields (zero-copy)
    #[inline]
    pub fn iter_fields(&self) -> FieldIter<'a> {
        FieldIter {
            record: *self, // Copy, not clone — it's just a slice + usize
            pos: 0,
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serialization::{from_bytes, from_spooky, serialize_into};
    use crate::spooky_value::FastMap;
    use smol_str::SmolStr;

    // ═══════════════════════════════════════════════════════════════════════
    // Helpers
    // ═══════════════════════════════════════════════════════════════════════

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

    /// Record with exactly 4 fields — exercises the linear-search path.
    fn make_linear_record() -> SpookyValue {
        let mut map = FastMap::new();
        map.insert(SmolStr::from("a"), SpookyValue::from("alpha"));
        map.insert(SmolStr::from("b"), SpookyValue::from(1i64));
        map.insert(SmolStr::from("c"), SpookyValue::from(2.0f64));
        map.insert(SmolStr::from("d"), SpookyValue::from(true));
        SpookyValue::Object(map)
    }

    /// Build a simple single-field record.
    fn make_single_field(key: &str, val: SpookyValue) -> SpookyValue {
        let mut map = FastMap::new();
        map.insert(SmolStr::from(key), val);
        SpookyValue::Object(map)
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Basic round-trip
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_roundtrip_flat_fields() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.field_count(), 6);
        assert_eq!(record.get_str("id"), Some("user:123"));
        assert_eq!(record.get_str("name"), Some("Alice"));
        assert_eq!(record.get_i64("age"), Some(30));
        assert_eq!(record.get_f64("score"), Some(99.5));
        assert_eq!(record.get_bool("active"), Some(true));
        assert_eq!(record.get_u64("version"), Some(42));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Empty record
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_empty_record() {
        let obj = SpookyValue::Object(FastMap::new());
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.field_count(), 0);
        assert!(!record.has_field("anything"));
        assert!(record.get_str("anything").is_none());
        assert!(record.get_i64("anything").is_none());
        assert!(record.get_f64("anything").is_none());
        assert!(record.get_u64("anything").is_none());
        assert!(record.get_bool("anything").is_none());
        assert!(record.get_raw("anything").is_none());
        assert!(record.get_field("anything").is_none());
        assert!(record.get_number_as_f64("anything").is_none());
        assert!(record.field_type("anything").is_none());
        assert_eq!(record.iter_fields().count(), 0);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Single-field records (one per type tag)
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_single_string_field() {
        let obj = make_single_field("s", SpookyValue::from("hello"));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.field_count(), 1);
        assert_eq!(record.get_str("s"), Some("hello"));
        assert!(record.has_field("s"));
        assert_eq!(record.field_type("s"), Some(TAG_STR));
    }

    #[test]
    fn test_single_i64_field() {
        let obj = make_single_field("n", SpookyValue::from(-999i64));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.get_i64("n"), Some(-999));
        assert_eq!(record.field_type("n"), Some(TAG_I64));
    }

    #[test]
    fn test_single_u64_field() {
        let obj = make_single_field("n", SpookyValue::from(u64::MAX));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.get_u64("n"), Some(u64::MAX));
        assert_eq!(record.field_type("n"), Some(TAG_U64));
    }

    #[test]
    fn test_single_f64_field() {
        let obj = make_single_field("n", SpookyValue::from(std::f64::consts::PI));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.get_f64("n"), Some(std::f64::consts::PI));
        assert_eq!(record.field_type("n"), Some(TAG_F64));
    }

    #[test]
    fn test_single_bool_true() {
        let obj = make_single_field("b", SpookyValue::from(true));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.get_bool("b"), Some(true));
        assert_eq!(record.field_type("b"), Some(TAG_BOOL));
    }

    #[test]
    fn test_single_bool_false() {
        let obj = make_single_field("b", SpookyValue::from(false));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.get_bool("b"), Some(false));
    }

    #[test]
    fn test_single_null_field() {
        let obj = make_single_field("x", SpookyValue::Null);
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.field_type("x"), Some(TAG_NULL));
        assert_eq!(record.get_field("x"), Some(SpookyValue::Null));
        // Null must not be returned by typed getters
        assert!(record.get_str("x").is_none());
        assert!(record.get_i64("x").is_none());
        assert!(record.get_f64("x").is_none());
        assert!(record.get_u64("x").is_none());
        assert!(record.get_bool("x").is_none());
        assert!(record.get_number_as_f64("x").is_none());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Type-mismatch: every getter returns None for wrong type
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_type_mismatch_get_str_on_i64() {
        let obj = make_single_field("n", SpookyValue::from(42i64));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert!(record.get_str("n").is_none());
    }

    #[test]
    fn test_type_mismatch_get_i64_on_str() {
        let obj = make_single_field("s", SpookyValue::from("text"));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert!(record.get_i64("s").is_none());
    }

    #[test]
    fn test_type_mismatch_get_f64_on_bool() {
        let obj = make_single_field("b", SpookyValue::from(true));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert!(record.get_f64("b").is_none());
    }

    #[test]
    fn test_type_mismatch_get_u64_on_f64() {
        let obj = make_single_field("f", SpookyValue::from(1.5f64));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert!(record.get_u64("f").is_none());
    }

    #[test]
    fn test_type_mismatch_get_bool_on_str() {
        let obj = make_single_field("s", SpookyValue::from("yes"));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert!(record.get_bool("s").is_none());
    }

    #[test]
    fn test_type_mismatch_get_i64_on_u64() {
        let obj = make_single_field("n", SpookyValue::from(7u64));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        // i64 and u64 have different type tags
        assert!(record.get_i64("n").is_none());
    }

    #[test]
    fn test_type_mismatch_get_u64_on_i64() {
        let obj = make_single_field("n", SpookyValue::from(7i64));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert!(record.get_u64("n").is_none());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Missing field — every getter
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_missing_field() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert!(record.get_raw("nonexistent").is_none());
        assert!(record.get_str("nonexistent").is_none());
        assert!(record.get_i64("nonexistent").is_none());
        assert!(record.get_u64("nonexistent").is_none());
        assert!(record.get_f64("nonexistent").is_none());
        assert!(record.get_bool("nonexistent").is_none());
        assert!(record.get_field("nonexistent").is_none());
        assert!(record.get_number_as_f64("nonexistent").is_none());
        assert!(!record.has_field("nonexistent"));
        assert!(record.field_type("nonexistent").is_none());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // has_field
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_has_field() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert!(record.has_field("id"));
        assert!(record.has_field("age"));
        assert!(record.has_field("score"));
        assert!(record.has_field("active"));
        assert!(record.has_field("version"));
        assert!(record.has_field("name"));
        assert!(!record.has_field("missing"));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // get_number_as_f64 — cross-type conversion
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_get_number_as_f64() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        // i64 → f64
        assert_eq!(record.get_number_as_f64("age"), Some(30.0));
        // f64 → f64
        assert_eq!(record.get_number_as_f64("score"), Some(99.5));
        // u64 → f64
        assert_eq!(record.get_number_as_f64("version"), Some(42.0));
        // string → None
        assert_eq!(record.get_number_as_f64("name"), None);
        // bool → None
        assert_eq!(record.get_number_as_f64("active"), None);
        // missing → None
        assert_eq!(record.get_number_as_f64("nope"), None);
    }

    #[test]
    fn test_get_number_as_f64_negative_i64() {
        let obj = make_single_field("n", SpookyValue::from(-42i64));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert_eq!(record.get_number_as_f64("n"), Some(-42.0));
    }

    #[test]
    fn test_get_number_as_f64_zero() {
        let obj = make_single_field("n", SpookyValue::from(0i64));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert_eq!(record.get_number_as_f64("n"), Some(0.0));
    }

    #[test]
    fn test_get_number_as_f64_large_u64() {
        let obj = make_single_field("n", SpookyValue::from(u64::MAX));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert_eq!(record.get_number_as_f64("n"), Some(u64::MAX as f64));
    }

    #[test]
    fn test_get_number_as_f64_on_null() {
        let obj = make_single_field("x", SpookyValue::Null);
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert_eq!(record.get_number_as_f64("x"), None);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // field_type — every tag
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_field_type_all_tags() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.field_type("id"), Some(TAG_STR));
        assert_eq!(record.field_type("age"), Some(TAG_I64));
        assert_eq!(record.field_type("score"), Some(TAG_F64));
        assert_eq!(record.field_type("version"), Some(TAG_U64));
        assert_eq!(record.field_type("active"), Some(TAG_BOOL));
        assert_eq!(record.field_type("nope"), None);
    }

    #[test]
    fn test_field_type_null() {
        let obj = make_single_field("x", SpookyValue::Null);
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert_eq!(record.field_type("x"), Some(TAG_NULL));
    }

    #[test]
    fn test_field_type_nested_cbor() {
        let mut map = FastMap::new();
        map.insert(
            SmolStr::from("arr"),
            SpookyValue::Array(vec![SpookyValue::from(1i64)]),
        );
        let obj = SpookyValue::Object(map);
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert_eq!(record.field_type("arr"), Some(TAG_NESTED_CBOR));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // get_raw
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_get_raw_returns_correct_data() {
        let obj = make_single_field("val", SpookyValue::from(42i64));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        let raw = record.get_raw("val").unwrap();
        assert_eq!(raw.type_tag, TAG_I64);
        assert_eq!(raw.data.len(), 8);
        assert_eq!(i64::from_le_bytes(raw.data.try_into().unwrap()), 42);
    }

    #[test]
    fn test_get_raw_string_bytes() {
        let obj = make_single_field("s", SpookyValue::from("abc"));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        let raw = record.get_raw("s").unwrap();
        assert_eq!(raw.type_tag, TAG_STR);
        assert_eq!(raw.data, b"abc");
    }

    #[test]
    fn test_get_raw_null_zero_length() {
        let obj = make_single_field("x", SpookyValue::Null);
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        let raw = record.get_raw("x").unwrap();
        assert_eq!(raw.type_tag, TAG_NULL);
        assert_eq!(raw.data.len(), 0);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // get_field (full decode via CBOR / native)
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_get_field_decodes_all_flat_types() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(
            record.get_field("id"),
            Some(SpookyValue::from("user:123"))
        );
        assert_eq!(record.get_field("age"), Some(SpookyValue::from(30i64)));
        assert_eq!(record.get_field("score"), Some(SpookyValue::from(99.5f64)));
        assert_eq!(record.get_field("active"), Some(SpookyValue::from(true)));
        assert_eq!(
            record.get_field("version"),
            Some(SpookyValue::from(42u64))
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Nested CBOR (objects and arrays)
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_nested_cbor_object() {
        let mut map = FastMap::new();
        let mut inner = FastMap::new();
        inner.insert(SmolStr::from("city"), SpookyValue::from("Berlin"));
        map.insert(SmolStr::from("address"), SpookyValue::Object(inner));
        let obj = SpookyValue::Object(map);

        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        let addr = record.get_field("address").unwrap();
        assert_eq!(addr.get("city").and_then(|v| v.as_str()), Some("Berlin"));
    }

    #[test]
    fn test_nested_cbor_array() {
        let mut map = FastMap::new();
        map.insert(
            SmolStr::from("tags"),
            SpookyValue::Array(vec![
                SpookyValue::from("a"),
                SpookyValue::from("b"),
                SpookyValue::from("c"),
            ]),
        );
        let obj = SpookyValue::Object(map);

        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        let tags = record.get_field("tags").unwrap();
        let arr = tags.as_array().unwrap();
        assert_eq!(arr.len(), 3);
    }

    #[test]
    fn test_nested_cbor_empty_array() {
        let mut map = FastMap::new();
        map.insert(SmolStr::from("empty"), SpookyValue::Array(vec![]));
        let obj = SpookyValue::Object(map);

        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        let val = record.get_field("empty").unwrap();
        assert_eq!(val.as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_nested_cbor_empty_object() {
        let mut map = FastMap::new();
        map.insert(SmolStr::from("obj"), SpookyValue::Object(FastMap::new()));
        let obj = SpookyValue::Object(map);

        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        let val = record.get_field("obj").unwrap();
        assert!(val.as_object().unwrap().is_empty());
    }

    #[test]
    fn test_deeply_nested_cbor() {
        let mut level3 = FastMap::new();
        level3.insert(SmolStr::from("deep"), SpookyValue::from("value"));
        let mut level2 = FastMap::new();
        level2.insert(SmolStr::from("l3"), SpookyValue::Object(level3));
        let mut level1 = FastMap::new();
        level1.insert(SmolStr::from("l2"), SpookyValue::Object(level2));
        let mut root = FastMap::new();
        root.insert(SmolStr::from("l1"), SpookyValue::Object(level1));
        let obj = SpookyValue::Object(root);

        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        let l1 = record.get_field("l1").unwrap();
        let l2 = l1.get("l2").unwrap();
        let l3 = l2.get("l3").unwrap();
        assert_eq!(l3.get("deep").and_then(|v| v.as_str()), Some("value"));
    }

    #[test]
    fn test_mixed_flat_and_nested() {
        let mut map = FastMap::new();
        map.insert(SmolStr::from("flat_str"), SpookyValue::from("hello"));
        map.insert(SmolStr::from("flat_num"), SpookyValue::from(7i64));
        map.insert(
            SmolStr::from("nested"),
            SpookyValue::Array(vec![SpookyValue::from(1i64), SpookyValue::from(2i64)]),
        );
        map.insert(SmolStr::from("flat_bool"), SpookyValue::from(false));
        let obj = SpookyValue::Object(map);

        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.get_str("flat_str"), Some("hello"));
        assert_eq!(record.get_i64("flat_num"), Some(7));
        assert_eq!(record.get_bool("flat_bool"), Some(false));
        let arr = record.get_field("nested").unwrap();
        assert_eq!(arr.as_array().unwrap().len(), 2);
        // Typed getters must not return nested CBOR
        assert!(record.get_str("nested").is_none());
        assert!(record.get_i64("nested").is_none());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Null field
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_null_field() {
        let mut map = FastMap::new();
        map.insert(SmolStr::from("nothing"), SpookyValue::Null);
        let obj = SpookyValue::Object(map);

        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert_eq!(record.get_field("nothing"), Some(SpookyValue::Null));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Serialization: from_spooky rejects non-objects
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_not_an_object_string() {
        assert!(from_spooky(&SpookyValue::from("not an object")).is_err());
    }

    #[test]
    fn test_not_an_object_number() {
        assert!(from_spooky(&SpookyValue::from(42i64)).is_err());
    }

    #[test]
    fn test_not_an_object_array() {
        assert!(from_spooky(&SpookyValue::Array(vec![])).is_err());
    }

    #[test]
    fn test_not_an_object_null() {
        assert!(from_spooky(&SpookyValue::Null).is_err());
    }

    #[test]
    fn test_not_an_object_bool() {
        assert!(from_spooky(&SpookyValue::from(true)).is_err());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Iterator (FieldIter)
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_iter_fields_count() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        let fields: Vec<_> = record.iter_fields().collect();
        assert_eq!(fields.len(), 6);
    }

    #[test]
    fn test_iter_fields_exact_size() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        let iter = record.iter_fields();
        assert_eq!(iter.len(), 6);
    }

    #[test]
    fn test_iter_fields_size_hint_decrements() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        let mut iter = record.iter_fields();
        assert_eq!(iter.size_hint(), (6, Some(6)));
        iter.next();
        assert_eq!(iter.size_hint(), (5, Some(5)));
    }

    #[test]
    fn test_iter_fields_empty_record() {
        let obj = SpookyValue::Object(FastMap::new());
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.iter_fields().count(), 0);
        assert_eq!(record.iter_fields().len(), 0);
    }

    #[test]
    fn test_iter_fields_type_tags_present() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        let tags: Vec<u8> = record.iter_fields().map(|f| f.type_tag).collect();
        // We should see all our type tags (order is sorted by hash, so just check membership)
        assert!(tags.contains(&TAG_STR));
        assert!(tags.contains(&TAG_I64));
        assert!(tags.contains(&TAG_F64));
        assert!(tags.contains(&TAG_BOOL));
        assert!(tags.contains(&TAG_U64));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // read_index — bounds checking
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_read_index_valid() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        for i in 0..fc {
            assert!(record.read_index(i).is_some());
        }
    }

    #[test]
    fn test_read_index_out_of_bounds() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert!(record.read_index(fc).is_none());
        assert!(record.read_index(fc + 1).is_none());
        assert!(record.read_index(usize::MAX).is_none());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Linear search path (≤ 4 fields)
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_linear_search_path() {
        let original = make_linear_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        assert!(fc <= 4, "should use linear search for ≤ 4 fields");
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.get_str("a"), Some("alpha"));
        assert_eq!(record.get_i64("b"), Some(1));
        assert_eq!(record.get_f64("c"), Some(2.0));
        assert_eq!(record.get_bool("d"), Some(true));
        assert!(!record.has_field("e"));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Binary search path (> 4 fields)
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_binary_search_path() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        assert!(fc > 4, "should use binary search for > 4 fields");
        let record = SpookyRecord::new(&buf, fc);

        // Verify every field is still found by binary search
        assert_eq!(record.get_str("id"), Some("user:123"));
        assert_eq!(record.get_str("name"), Some("Alice"));
        assert_eq!(record.get_i64("age"), Some(30));
        assert_eq!(record.get_f64("score"), Some(99.5));
        assert_eq!(record.get_bool("active"), Some(true));
        assert_eq!(record.get_u64("version"), Some(42));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Edge-case numeric values
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_i64_boundaries() {
        for val in [i64::MIN, -1, 0, 1, i64::MAX] {
            let obj = make_single_field("n", SpookyValue::from(val));
            let (buf, fc) = from_spooky(&obj).unwrap();
            let record = SpookyRecord::new(&buf, fc);
            assert_eq!(record.get_i64("n"), Some(val), "failed for i64 = {val}");
        }
    }

    #[test]
    fn test_u64_boundaries() {
        for val in [0u64, 1, u64::MAX] {
            let obj = make_single_field("n", SpookyValue::from(val));
            let (buf, fc) = from_spooky(&obj).unwrap();
            let record = SpookyRecord::new(&buf, fc);
            assert_eq!(record.get_u64("n"), Some(val), "failed for u64 = {val}");
        }
    }

    #[test]
    fn test_f64_special_values() {
        for val in [0.0f64, -0.0, f64::MIN, f64::MAX, f64::INFINITY, f64::NEG_INFINITY] {
            let obj = make_single_field("n", SpookyValue::from(val));
            let (buf, fc) = from_spooky(&obj).unwrap();
            let record = SpookyRecord::new(&buf, fc);
            assert_eq!(record.get_f64("n"), Some(val), "failed for f64 = {val}");
        }
    }

    #[test]
    fn test_f64_nan_roundtrip() {
        let obj = make_single_field("n", SpookyValue::from(f64::NAN));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert!(record.get_f64("n").unwrap().is_nan());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Edge-case strings
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_empty_string() {
        let obj = make_single_field("s", SpookyValue::from(""));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert_eq!(record.get_str("s"), Some(""));
    }

    #[test]
    fn test_unicode_string() {
        let obj = make_single_field("s", SpookyValue::from("Héllo 🌍 日本語"));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert_eq!(record.get_str("s"), Some("Héllo 🌍 日本語"));
    }

    #[test]
    fn test_long_string() {
        let long = "x".repeat(10_000);
        let obj = make_single_field("s", SpookyValue::from(long.as_str()));
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);
        assert_eq!(record.get_str("s"), Some(long.as_str()));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // from_bytes validation
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_from_bytes_valid() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        let (parsed_buf, parsed_fc) = from_bytes(&buf).unwrap();
        assert_eq!(parsed_fc, fc);
        assert_eq!(parsed_buf.len(), buf.len());
    }

    #[test]
    fn test_from_bytes_too_short() {
        let short = vec![0u8; 10]; // less than HEADER_SIZE (20)
        assert!(from_bytes(&short).is_err());
    }

    #[test]
    fn test_from_bytes_empty() {
        assert!(from_bytes(&[]).is_err());
    }

    #[test]
    fn test_from_bytes_header_only_zero_fields() {
        // 20 bytes, field_count = 0 → valid
        let mut buf = vec![0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&0u32.to_le_bytes());
        let (_, fc) = from_bytes(&buf).unwrap();
        assert_eq!(fc, 0);
    }

    #[test]
    fn test_from_bytes_claims_fields_but_too_short() {
        // Header says 5 fields but buffer is only header-sized
        let mut buf = vec![0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&5u32.to_le_bytes());
        assert!(from_bytes(&buf).is_err());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // serialize_into (reusable buffer path)
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_serialize_into_roundtrip() {
        let mut map = FastMap::new();
        map.insert(SmolStr::from("x"), SpookyValue::from(10i64));
        map.insert(SmolStr::from("y"), SpookyValue::from("hi"));
        let mut buf = Vec::new();
        let fc = serialize_into(&map, &mut buf).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.get_i64("x"), Some(10));
        assert_eq!(record.get_str("y"), Some("hi"));
    }

    #[test]
    fn test_serialize_into_reuses_buffer() {
        let mut map = FastMap::new();
        map.insert(SmolStr::from("a"), SpookyValue::from(1i64));
        let mut buf = Vec::new();

        // First serialization
        let fc1 = serialize_into(&map, &mut buf).unwrap();
        let r1 = SpookyRecord::new(&buf, fc1);
        assert_eq!(r1.get_i64("a"), Some(1));

        // Second serialization into the *same* buffer
        map.clear();
        map.insert(SmolStr::from("b"), SpookyValue::from(2i64));
        let fc2 = serialize_into(&map, &mut buf).unwrap();
        let r2 = SpookyRecord::new(&buf, fc2);
        assert_eq!(r2.get_i64("b"), Some(2));
        assert!(r2.get_i64("a").is_none()); // old field gone
    }

    // ═══════════════════════════════════════════════════════════════════════
    // to_value (current placeholder behaviour)
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_to_value_returns_null_placeholder() {
        let original = make_test_record();
        let (buf, fc) = from_spooky(&original).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        // Current implementation is a placeholder
        assert_eq!(record.to_value(), SpookyValue::Null);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Multiple records from the same original check independence
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_two_records_independent() {
        let obj1 = make_single_field("a", SpookyValue::from(1i64));
        let obj2 = make_single_field("b", SpookyValue::from(2i64));
        let (buf1, fc1) = from_spooky(&obj1).unwrap();
        let (buf2, fc2) = from_spooky(&obj2).unwrap();
        let r1 = SpookyRecord::new(&buf1, fc1);
        let r2 = SpookyRecord::new(&buf2, fc2);

        assert_eq!(r1.get_i64("a"), Some(1));
        assert!(r1.get_i64("b").is_none());
        assert_eq!(r2.get_i64("b"), Some(2));
        assert!(r2.get_i64("a").is_none());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Many fields (stress binary search)
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_many_fields() {
        let mut map = FastMap::new();
        for i in 0..30 {
            map.insert(
                SmolStr::from(format!("field_{i}")),
                SpookyValue::from(i as i64),
            );
        }
        let obj = SpookyValue::Object(map);
        let (buf, fc) = from_spooky(&obj).unwrap();
        let record = SpookyRecord::new(&buf, fc);

        assert_eq!(record.field_count(), 30);
        for i in 0..30 {
            assert_eq!(
                record.get_i64(&format!("field_{i}")),
                Some(i as i64),
                "field_{i} not found"
            );
        }
    }
}
