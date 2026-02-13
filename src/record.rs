use super::deserialization::decode_field;
use super::error::RecordError;
use super::spooky_value::SpookyValue;
use super::types::*;
use xxhash_rust::xxh64::xxh64;

// ─── Reader (zero-copy) ────────────────────────────────────────────────────
/// Zero-copy reader over a hybrid record byte slice.
/// No parsing happens until you request a specific field.
#[derive(Debug, Clone)]
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
        let bytes: [u8; 8] = self.data_buf[meta.data_offset..meta.data_offset + 8]
            .try_into()
            .ok()?;
        match meta.type_tag {
            TAG_F64 => Some(f64::from_le_bytes(bytes)),
            TAG_I64 => Some(i64::from_le_bytes(bytes) as f64),
            TAG_U64 => Some(u64::from_le_bytes(bytes) as f64),
            _ => None,
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
        let bytes = SpookyRecord::serialize(&original).unwrap();
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
        let bytes = SpookyRecord::serialize(&original).unwrap();
        let record = SpookyRecord::from_bytes(&bytes).unwrap();

        assert!(record.get_raw("nonexistent").is_none());
        assert!(record.get_str("nonexistent").is_none());
        assert!(!record.has_field("nonexistent"));
    }

    #[test]
    fn test_has_field() {
        let original = make_test_record();
        let bytes = SpookyRecord::serialize(&original).unwrap();
        let record = SpookyRecord::from_bytes(&bytes).unwrap();

        assert!(record.has_field("id"));
        assert!(record.has_field("age"));
        assert!(!record.has_field("missing"));
    }

    #[test]
    fn test_get_number_as_f64() {
        let original = make_test_record();
        let bytes = SpookyRecord::serialize(&original).unwrap();
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

        let bytes = SpookyRecord::serialize(&obj).unwrap();
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
        assert!(SpookyRecord::serialize(&val).is_err());
    }

    #[test]
    fn test_null_field() {
        let mut map = FastMap::new();
        map.insert(SmolStr::from("nothing"), SpookyValue::Null);
        let obj = SpookyValue::Object(map);

        let bytes = SpookyRecord::serialize(&obj).unwrap();
        let record = SpookyRecord::from_bytes(&bytes).unwrap();
        assert_eq!(record.get_field("nothing"), Some(SpookyValue::Null));
    }

    #[test]
    fn test_iter_fields() {
        let original = make_test_record();
        let bytes = SpookyRecord::serialize(&original).unwrap();
        let record = SpookyRecord::from_bytes(&bytes).unwrap();

        let fields: Vec<_> = record.iter_fields().collect();
        assert_eq!(fields.len(), 6);
    }
}
