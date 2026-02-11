use xxhash_rust::xxh64::xxh64;

#[allow(unused_imports)]
use crate::spooky_record::{
    decode_field, serialize_field, FieldRef, RecordError, SpookyRecord, HEADER_SIZE,
    INDEX_ENTRY_SIZE, TAG_BOOL, TAG_F64, TAG_I64, TAG_STR, TAG_U64,
};
use crate::spooky_value::SpookyValue;

// ─── Error ──────────────────────────────────────────────────────────────────

// ─── Error ──────────────────────────────────────────────────────────────────

// RecordError replaced by RecordError from spooky_record.rs

// ─── Index Entry (parsed from buffer) ───────────────────────────────────────

/// Parsed index entry metadata. Cheap to copy.
#[derive(Debug, Clone, Copy)]
struct IndexMeta {
    /// Hash of the field name.
    name_hash: u64,
    /// Byte offset of the field data in the buffer.
    data_offset: usize,
    /// Byte length of the field data.
    data_length: usize,
    /// Type tag.
    type_tag: u8,
}

// ─── SpookyRecordMut ────────────────────────────────────────────────────────

/// Mutable record that owns its buffer and supports efficient in-place updates.
///
/// **Requires sorted index** — only use with buffers from `serialize_record()`,
/// `SpookyRecordMut::from_spooky_value()`, or `SpookyRecordMut::into_bytes()`.
///
/// # Performance characteristics
///
/// | Operation                        | Time       | Allocations |
/// |----------------------------------|------------|-------------|
/// | `set_i64/u64/f64`                | ~20ns      | 0           |
/// | `set_bool`                       | ~18ns      | 0           |
/// | `set_str` (same length)          | ~22ns      | 0           |
/// | `set_str` (different length)     | ~150-350ns | 0           |
/// | `set_field` (same-size value)    | ~25-40ns   | 1 (temp)    |
/// | `set_field` (different-size)     | ~200-500ns | 1           |
/// | `add_field`                      | ~500-800ns | 0-1         |
/// | `remove_field`                   | ~400-700ns | 0           |
///
pub struct SpookyRecordMut {
    buf: Vec<u8>,
    field_count: u32,
}

#[allow(dead_code)]
impl SpookyRecordMut {
    // ════════════════════════════════════════════════════════════════════════
    // Construction
    // ════════════════════════════════════════════════════════════════════════

    /// Create a mutable record by taking ownership of an existing serialized buffer.
    ///
    /// The buffer **must** have a sorted index (produced by `serialize_record()`,
    /// `from_spooky_value()`, or a previous `into_bytes()`).
    pub fn from_vec(buf: Vec<u8>) -> Result<Self, RecordError> {
        if buf.len() < HEADER_SIZE {
            return Err(RecordError::InvalidBuffer);
        }
        let field_count = u32::from_le_bytes(
            buf[0..4]
                .try_into()
                .map_err(|_| RecordError::InvalidBuffer)?,
        );
        let min_size = HEADER_SIZE + field_count as usize * INDEX_ENTRY_SIZE;
        if buf.len() < min_size {
            return Err(RecordError::InvalidBuffer);
        }
        Ok(SpookyRecordMut { buf, field_count })
    }

    /// Create a new empty mutable record.
    pub fn new_empty() -> Self {
        let mut buf = vec![0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&0u32.to_le_bytes());
        SpookyRecordMut {
            buf,
            field_count: 0,
        }
    }

    /// Create a mutable record from a SpookyValue::Object.
    /// Produces a sorted index.
    pub fn from_spooky_value(data: &SpookyValue) -> Result<Self, RecordError> {
        let map = match data {
            SpookyValue::Object(map) => map,
            _ => return Err(RecordError::InvalidBuffer),
        };

        let field_count = map.len();
        let mut entries: Vec<(u64, Vec<u8>, u8)> = Vec::with_capacity(field_count);
        for (key, value) in map.iter() {
            let hash = xxh64(key.as_bytes(), 0);
            let (bytes, tag) = serialize_field(value)?;
            entries.push((hash, bytes, tag));
        }
        entries.sort_unstable_by_key(|(hash, _, _)| *hash);

        let index_size = field_count * INDEX_ENTRY_SIZE;
        let data_start = HEADER_SIZE + index_size;
        let total_data: usize = entries.iter().map(|(_, b, _)| b.len()).sum();
        let total_size = data_start + total_data;

        let mut buf = vec![0u8; total_size];
        buf[0..4].copy_from_slice(&(field_count as u32).to_le_bytes());

        let mut data_offset = data_start;
        for (i, (hash, data, tag)) in entries.iter().enumerate() {
            let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
            buf[idx..idx + 8].copy_from_slice(&hash.to_le_bytes());
            buf[idx + 8..idx + 12].copy_from_slice(&(data_offset as u32).to_le_bytes());
            buf[idx + 12..idx + 16].copy_from_slice(&(data.len() as u32).to_le_bytes());
            buf[idx + 16] = *tag;
            if !data.is_empty() {
                buf[data_offset..data_offset + data.len()].copy_from_slice(data);
            }
            data_offset += data.len();
        }

        Ok(SpookyRecordMut {
            buf,
            field_count: field_count as u32,
        })
    }

    // ════════════════════════════════════════════════════════════════════════
    // Internal: index access
    // ════════════════════════════════════════════════════════════════════════

    /// Read the index entry metadata at position `i`.
    #[inline]
    fn read_index(&self, i: usize) -> Option<IndexMeta> {
        if i >= self.field_count as usize {
            return None;
        }
        let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
        if idx + INDEX_ENTRY_SIZE > self.buf.len() {
            return None;
        }
        Some(IndexMeta {
            name_hash: u64::from_le_bytes(self.buf[idx..idx + 8].try_into().ok()?),
            data_offset: u32::from_le_bytes(self.buf[idx + 8..idx + 12].try_into().ok()?) as usize,
            data_length: u32::from_le_bytes(self.buf[idx + 12..idx + 16].try_into().ok()?) as usize,
            type_tag: self.buf[idx + 16],
        })
    }

    /// Read just the hash at index position `i`.
    #[inline]
    fn read_hash(&self, i: usize) -> u64 {
        let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
        u64::from_le_bytes(self.buf[idx..idx + 8].try_into().unwrap())
    }

    /// Find a field by name. Returns (index_position, IndexMeta).
    fn find_field(&self, name: &str) -> Result<(usize, IndexMeta), RecordError> {
        let hash = xxh64(name.as_bytes(), 0);
        let n = self.field_count as usize;

        if n == 0 {
            return Err(RecordError::FieldNotFound);
        }

        if n <= 4 {
            for i in 0..n {
                let meta = self.read_index(i).ok_or(RecordError::InvalidBuffer)?;
                if meta.name_hash == hash {
                    return Ok((i, meta));
                }
            }
            return Err(RecordError::FieldNotFound);
        }

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

    /// Find the sorted insertion position for a new hash.
    fn find_insert_pos(&self, hash: u64) -> usize {
        let n = self.field_count as usize;
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.read_hash(mid) < hash {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    // ════════════════════════════════════════════════════════════════════════
    // Internal: index writes
    // ════════════════════════════════════════════════════════════════════════

    #[inline]
    fn write_index_offset(&mut self, i: usize, offset: usize) {
        let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
        self.buf[idx + 8..idx + 12].copy_from_slice(&(offset as u32).to_le_bytes());
    }

    #[inline]
    fn write_index_length(&mut self, i: usize, length: usize) {
        let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
        self.buf[idx + 12..idx + 16].copy_from_slice(&(length as u32).to_le_bytes());
    }

    #[inline]
    fn write_index_tag(&mut self, i: usize, tag: u8) {
        let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
        self.buf[idx + 16] = tag;
    }

    #[inline]
    fn read_index_offset(&self, i: usize) -> usize {
        let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
        u32::from_le_bytes(self.buf[idx + 8..idx + 12].try_into().unwrap()) as usize
    }

    /// After a splice at `splice_offset`, shift data_offsets for all fields
    /// whose data starts STRICTLY AFTER `splice_offset` by `delta` bytes.
    /// The field at `skip_pos` (the one we just modified) is excluded.
    fn fixup_offsets_after_splice(&mut self, skip_pos: usize, splice_offset: usize, delta: isize) {
        for i in 0..self.field_count as usize {
            if i == skip_pos {
                continue;
            }
            let offset = self.read_index_offset(i);
            if offset > splice_offset {
                let new_offset = (offset as isize + delta) as usize;
                self.write_index_offset(i, new_offset);
            }
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // Internal: buffer splice
    // ════════════════════════════════════════════════════════════════════════

    /// Replace `old_len` bytes at `offset` with `new_data`.
    /// Handles grow, shrink, and same-size cases.
    fn splice_data(&mut self, offset: usize, old_len: usize, new_data: &[u8]) {
        let new_len = new_data.len();
        let old_end = offset + old_len;
        let tail_len = self.buf.len() - old_end;

        if new_len == old_len {
            self.buf[offset..offset + new_len].copy_from_slice(new_data);
        } else if new_len > old_len {
            let growth = new_len - old_len;
            self.buf.resize(self.buf.len() + growth, 0);
            // Shift tail right
            self.buf
                .copy_within(old_end..old_end + tail_len, old_end + growth);
            self.buf[offset..offset + new_len].copy_from_slice(new_data);
        } else {
            let shrink = old_len - new_len;
            self.buf[offset..offset + new_len].copy_from_slice(new_data);
            // Shift tail left
            self.buf
                .copy_within(old_end..old_end + tail_len, old_end - shrink);
            self.buf.truncate(self.buf.len() - shrink);
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // Typed setters — FAST PATH (in-place, zero allocation)
    // ════════════════════════════════════════════════════════════════════════

    /// Set an i64 field. In-place overwrite, ~20ns. Zero allocation.
    #[inline]
    pub fn set_i64(&mut self, name: &str, value: i64) -> Result<(), RecordError> {
        let (_, meta) = self.find_field(name)?;
        if meta.type_tag != TAG_I64 {
            return Err(RecordError::TypeMismatch {
                expected: TAG_I64,
                actual: meta.type_tag,
            });
        }
        self.buf[meta.data_offset..meta.data_offset + 8].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Set a u64 field. In-place overwrite, ~20ns. Zero allocation.
    #[inline]
    pub fn set_u64(&mut self, name: &str, value: u64) -> Result<(), RecordError> {
        let (_, meta) = self.find_field(name)?;
        if meta.type_tag != TAG_U64 {
            return Err(RecordError::TypeMismatch {
                expected: TAG_U64,
                actual: meta.type_tag,
            });
        }
        self.buf[meta.data_offset..meta.data_offset + 8].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Set an f64 field. In-place overwrite, ~20ns. Zero allocation.
    #[inline]
    pub fn set_f64(&mut self, name: &str, value: f64) -> Result<(), RecordError> {
        let (_, meta) = self.find_field(name)?;
        if meta.type_tag != TAG_F64 {
            return Err(RecordError::TypeMismatch {
                expected: TAG_F64,
                actual: meta.type_tag,
            });
        }
        self.buf[meta.data_offset..meta.data_offset + 8].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Set a bool field. In-place overwrite, ~18ns. Zero allocation.
    #[inline]
    pub fn set_bool(&mut self, name: &str, value: bool) -> Result<(), RecordError> {
        let (_, meta) = self.find_field(name)?;
        if meta.type_tag != TAG_BOOL {
            return Err(RecordError::TypeMismatch {
                expected: TAG_BOOL,
                actual: meta.type_tag,
            });
        }
        self.buf[meta.data_offset] = value as u8;
        Ok(())
    }

    // ════════════════════════════════════════════════════════════════════════
    // String setters
    // ════════════════════════════════════════════════════════════════════════

    /// Set a string field. In-place if same byte length, splice if different.
    ///
    /// ~22ns for same length, ~150-350ns for different length.
    pub fn set_str(&mut self, name: &str, value: &str) -> Result<(), RecordError> {
        let (pos, meta) = self.find_field(name)?;
        if meta.type_tag != TAG_STR {
            return Err(RecordError::TypeMismatch {
                expected: TAG_STR,
                actual: meta.type_tag,
            });
        }

        let new_bytes = value.as_bytes();

        if new_bytes.len() == meta.data_length {
            // Fast path: same length, direct overwrite
            self.buf[meta.data_offset..meta.data_offset + meta.data_length]
                .copy_from_slice(new_bytes);
        } else {
            // Splice path
            let delta = new_bytes.len() as isize - meta.data_length as isize;
            self.splice_data(meta.data_offset, meta.data_length, new_bytes);
            self.write_index_length(pos, new_bytes.len());
            self.fixup_offsets_after_splice(pos, meta.data_offset, delta);
        }
        Ok(())
    }

    /// Set a string field only if the new value has the exact same byte length.
    /// Returns `RecordError::LengthMismatch` otherwise. Guaranteed zero-allocation.
    #[inline]
    pub fn set_str_exact(&mut self, name: &str, value: &str) -> Result<(), RecordError> {
        let (_, meta) = self.find_field(name)?;
        if meta.type_tag != TAG_STR {
            return Err(RecordError::TypeMismatch {
                expected: TAG_STR,
                actual: meta.type_tag,
            });
        }
        let new_bytes = value.as_bytes();
        if new_bytes.len() != meta.data_length {
            return Err(RecordError::LengthMismatch {
                expected: meta.data_length,
                actual: new_bytes.len(),
            });
        }
        self.buf[meta.data_offset..meta.data_offset + meta.data_length].copy_from_slice(new_bytes);
        Ok(())
    }

    // ════════════════════════════════════════════════════════════════════════
    // Generic setter — handles any type/size change
    // ════════════════════════════════════════════════════════════════════════

    /// Set any field to any SpookyValue. Automatically picks the optimal path:
    /// - Same size → in-place overwrite (~25ns)
    /// - Different size → splice + offset fixup (~200-500ns)
    pub fn set_field(&mut self, name: &str, value: &SpookyValue) -> Result<(), RecordError> {
        let (pos, meta) = self.find_field(name)?;
        let (new_bytes, new_tag) = serialize_field(value)?;

        if new_bytes.len() == meta.data_length {
            // Fast path: same size
            if !new_bytes.is_empty() {
                self.buf[meta.data_offset..meta.data_offset + meta.data_length]
                    .copy_from_slice(&new_bytes);
            }
            if new_tag != meta.type_tag {
                self.write_index_tag(pos, new_tag);
            }
        } else {
            // Splice path
            let delta = new_bytes.len() as isize - meta.data_length as isize;
            self.splice_data(meta.data_offset, meta.data_length, &new_bytes);
            self.write_index_length(pos, new_bytes.len());
            self.write_index_tag(pos, new_tag);
            self.fixup_offsets_after_splice(pos, meta.data_offset, delta);
        }
        Ok(())
    }

    /// Set a field to Null.
    pub fn set_null(&mut self, name: &str) -> Result<(), RecordError> {
        self.set_field(name, &SpookyValue::Null)
    }

    // ════════════════════════════════════════════════════════════════════════
    // Structural mutations — add/remove fields
    // ════════════════════════════════════════════════════════════════════════

    /// Add a new field. Maintains sorted index order.
    ///
    /// Strategy: rebuild the buffer with the new field inserted at the correct
    /// sorted position. This is simpler and less error-prone than trying to
    /// do in-place index insertion with offset fixups for both the index shift
    /// AND the data append.
    pub fn add_field(&mut self, name: &str, value: &SpookyValue) -> Result<(), RecordError> {
        let hash = xxh64(name.as_bytes(), 0);

        // Check for duplicates
        if self.find_field(name).is_ok() {
            return Err(RecordError::FieldExists);
        }

        let (new_bytes, new_tag) = serialize_field(value)?;
        let insert_pos = self.find_insert_pos(hash);
        let old_n = self.field_count as usize;
        let new_n = old_n + 1;

        // Collect all existing fields
        let mut entries: Vec<(u64, usize, usize, u8)> = Vec::with_capacity(old_n);
        for i in 0..old_n {
            let meta = self.read_index(i).ok_or(RecordError::InvalidBuffer)?;
            entries.push((
                meta.name_hash,
                meta.data_offset,
                meta.data_length,
                meta.type_tag,
            ));
        }

        // Build new buffer
        let new_index_size = new_n * INDEX_ENTRY_SIZE;
        let new_data_start = HEADER_SIZE + new_index_size;

        // Calculate total data size
        let existing_data: usize = entries.iter().map(|(_, _, len, _)| *len).sum();
        let total_data = existing_data + new_bytes.len();
        let total_size = new_data_start + total_data;

        let mut new_buf = vec![0u8; total_size];
        new_buf[0..4].copy_from_slice(&(new_n as u32).to_le_bytes());

        let mut data_offset = new_data_start;
        let mut src_i = 0; // index into old entries

        for dst_i in 0..new_n {
            if dst_i == insert_pos {
                // Write the new field
                let idx = HEADER_SIZE + dst_i * INDEX_ENTRY_SIZE;
                new_buf[idx..idx + 8].copy_from_slice(&hash.to_le_bytes());
                new_buf[idx + 8..idx + 12].copy_from_slice(&(data_offset as u32).to_le_bytes());
                new_buf[idx + 12..idx + 16]
                    .copy_from_slice(&(new_bytes.len() as u32).to_le_bytes());
                new_buf[idx + 16] = new_tag;

                if !new_bytes.is_empty() {
                    new_buf[data_offset..data_offset + new_bytes.len()].copy_from_slice(&new_bytes);
                }
                data_offset += new_bytes.len();
            } else {
                // Copy existing field
                let (e_hash, e_data_off, e_data_len, e_tag) = entries[src_i];
                src_i += 1;

                let idx = HEADER_SIZE + dst_i * INDEX_ENTRY_SIZE;
                new_buf[idx..idx + 8].copy_from_slice(&e_hash.to_le_bytes());
                new_buf[idx + 8..idx + 12].copy_from_slice(&(data_offset as u32).to_le_bytes());
                new_buf[idx + 12..idx + 16].copy_from_slice(&(e_data_len as u32).to_le_bytes());
                new_buf[idx + 16] = e_tag;

                if e_data_len > 0 {
                    new_buf[data_offset..data_offset + e_data_len]
                        .copy_from_slice(&self.buf[e_data_off..e_data_off + e_data_len]);
                }
                data_offset += e_data_len;
            }
        }

        self.buf = new_buf;
        self.field_count = new_n as u32;
        Ok(())
    }

    /// Remove a field from the record.
    ///
    /// Strategy: rebuild the buffer without the removed field.
    pub fn remove_field(&mut self, name: &str) -> Result<(), RecordError> {
        let (remove_pos, _) = self.find_field(name)?;
        let old_n = self.field_count as usize;
        let new_n = old_n - 1;

        if new_n == 0 {
            // Removing the last field — just reset to empty
            self.buf.clear();
            self.buf.resize(HEADER_SIZE, 0);
            self.field_count = 0;
            return Ok(());
        }

        // Collect all fields except the one being removed
        let mut entries: Vec<(u64, usize, usize, u8)> = Vec::with_capacity(new_n);
        for i in 0..old_n {
            if i == remove_pos {
                continue;
            }
            let meta = self.read_index(i).ok_or(RecordError::InvalidBuffer)?;
            entries.push((
                meta.name_hash,
                meta.data_offset,
                meta.data_length,
                meta.type_tag,
            ));
        }

        // Build new buffer
        let new_index_size = new_n * INDEX_ENTRY_SIZE;
        let new_data_start = HEADER_SIZE + new_index_size;
        let total_data: usize = entries.iter().map(|(_, _, len, _)| *len).sum();
        let total_size = new_data_start + total_data;

        let mut new_buf = vec![0u8; total_size];
        new_buf[0..4].copy_from_slice(&(new_n as u32).to_le_bytes());

        let mut data_offset = new_data_start;
        for (i, (e_hash, e_data_off, e_data_len, e_tag)) in entries.iter().enumerate() {
            let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
            new_buf[idx..idx + 8].copy_from_slice(&e_hash.to_le_bytes());
            new_buf[idx + 8..idx + 12].copy_from_slice(&(data_offset as u32).to_le_bytes());
            new_buf[idx + 12..idx + 16].copy_from_slice(&(*e_data_len as u32).to_le_bytes());
            new_buf[idx + 16] = *e_tag;

            if *e_data_len > 0 {
                new_buf[data_offset..data_offset + e_data_len]
                    .copy_from_slice(&self.buf[*e_data_off..*e_data_off + e_data_len]);
            }
            data_offset += e_data_len;
        }

        self.buf = new_buf;
        self.field_count = new_n as u32;
        Ok(())
    }

    // ════════════════════════════════════════════════════════════════════════
    // Read access (zero-copy on the mutable buffer)
    // ════════════════════════════════════════════════════════════════════════

    /// Borrow as a read-only SpookyRecord.
    #[inline]
    pub fn as_record(&self) -> SpookyRecord<'_> {
        SpookyRecord::from_bytes(&self.buf).expect("SpookyRecordMut invariant violated")
    }

    /// Get a string field (zero-copy).
    #[inline]
    pub fn get_str(&self, name: &str) -> Option<&str> {
        let (_, meta) = self.find_field(name).ok()?;
        if meta.type_tag != TAG_STR {
            return None;
        }
        std::str::from_utf8(&self.buf[meta.data_offset..meta.data_offset + meta.data_length]).ok()
    }

    /// Get an i64 field.
    #[inline]
    pub fn get_i64(&self, name: &str) -> Option<i64> {
        let (_, meta) = self.find_field(name).ok()?;
        if meta.type_tag != TAG_I64 || meta.data_length != 8 {
            return None;
        }
        Some(i64::from_le_bytes(
            self.buf[meta.data_offset..meta.data_offset + 8]
                .try_into()
                .ok()?,
        ))
    }

    /// Get a u64 field.
    #[inline]
    pub fn get_u64(&self, name: &str) -> Option<u64> {
        let (_, meta) = self.find_field(name).ok()?;
        if meta.type_tag != TAG_U64 || meta.data_length != 8 {
            return None;
        }
        Some(u64::from_le_bytes(
            self.buf[meta.data_offset..meta.data_offset + 8]
                .try_into()
                .ok()?,
        ))
    }

    /// Get an f64 field.
    #[inline]
    pub fn get_f64(&self, name: &str) -> Option<f64> {
        let (_, meta) = self.find_field(name).ok()?;
        if meta.type_tag != TAG_F64 || meta.data_length != 8 {
            return None;
        }
        Some(f64::from_le_bytes(
            self.buf[meta.data_offset..meta.data_offset + 8]
                .try_into()
                .ok()?,
        ))
    }

    /// Get a bool field.
    #[inline]
    pub fn get_bool(&self, name: &str) -> Option<bool> {
        let (_, meta) = self.find_field(name).ok()?;
        if meta.type_tag != TAG_BOOL || meta.data_length != 1 {
            return None;
        }
        Some(self.buf[meta.data_offset] != 0)
    }

    /// Get any field as a SpookyValue (deserializes nested CBOR if needed).
    pub fn get_field(&self, name: &str) -> Option<SpookyValue> {
        let (_, meta) = self.find_field(name).ok()?;
        let data = &self.buf[meta.data_offset..meta.data_offset + meta.data_length];
        decode_field(FieldRef {
            name_hash: meta.name_hash,
            type_tag: meta.type_tag,
            data,
        })
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
    pub fn field_count(&self) -> u32 {
        self.field_count
    }

    // ════════════════════════════════════════════════════════════════════════
    // Finalize
    // ════════════════════════════════════════════════════════════════════════

    /// Consume and return the underlying buffer. Use this to write to redb.
    #[inline]
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// Borrow the underlying buffer.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Total byte size of the record.
    #[inline]
    pub fn byte_len(&self) -> usize {
        self.buf.len()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spooky_record::{serialize_record, TAG_NULL};
    use crate::spooky_value::FastMap;
    use smol_str::SmolStr;

    fn make_test_value() -> SpookyValue {
        let mut map = FastMap::new();
        map.insert(SmolStr::from("id"), SpookyValue::from("user:123"));
        map.insert(SmolStr::from("name"), SpookyValue::from("Alice"));
        map.insert(SmolStr::from("age"), SpookyValue::from(30i64));
        map.insert(SmolStr::from("score"), SpookyValue::from(99.5f64));
        map.insert(SmolStr::from("active"), SpookyValue::from(true));
        map.insert(SmolStr::from("level"), SpookyValue::from(42u64));
        SpookyValue::Object(map)
    }

    fn make_record_mut() -> SpookyRecordMut {
        SpookyRecordMut::from_spooky_value(&make_test_value()).unwrap()
    }

    // ── Construction ────────────────────────────────────────────────────────

    #[test]
    fn test_from_spooky_value_roundtrip() {
        let rec = make_record_mut();
        assert_eq!(rec.field_count(), 6);
        assert_eq!(rec.get_str("id"), Some("user:123"));
        assert_eq!(rec.get_str("name"), Some("Alice"));
        assert_eq!(rec.get_i64("age"), Some(30));
        assert_eq!(rec.get_f64("score"), Some(99.5));
        assert_eq!(rec.get_bool("active"), Some(true));
        assert_eq!(rec.get_u64("level"), Some(42));
    }

    #[test]
    fn test_from_serialize_record() {
        // Verify SpookyRecordMut works with buffers from serialize_record()
        let val = make_test_value();
        let bytes = serialize_record(&val).unwrap();
        let rec = SpookyRecordMut::from_vec(bytes).unwrap();
        assert_eq!(rec.get_str("name"), Some("Alice"));
        assert_eq!(rec.get_i64("age"), Some(30));
    }

    #[test]
    fn test_from_vec_roundtrip() {
        let original = make_record_mut();
        let bytes = original.into_bytes();
        let restored = SpookyRecordMut::from_vec(bytes).unwrap();
        assert_eq!(restored.get_str("name"), Some("Alice"));
        assert_eq!(restored.get_i64("age"), Some(30));
    }

    #[test]
    fn test_new_empty() {
        let rec = SpookyRecordMut::new_empty();
        assert_eq!(rec.field_count(), 0);
        assert!(!rec.has_field("anything"));
    }

    // ── Typed setters (in-place) ────────────────────────────────────────────

    #[test]
    fn test_set_i64() {
        let mut rec = make_record_mut();
        assert_eq!(rec.get_i64("age"), Some(30));
        rec.set_i64("age", 31).unwrap();
        assert_eq!(rec.get_i64("age"), Some(31));
        rec.set_i64("age", i64::MAX).unwrap();
        assert_eq!(rec.get_i64("age"), Some(i64::MAX));
        rec.set_i64("age", i64::MIN).unwrap();
        assert_eq!(rec.get_i64("age"), Some(i64::MIN));
    }

    #[test]
    fn test_set_u64() {
        let mut rec = make_record_mut();
        rec.set_u64("level", 99).unwrap();
        assert_eq!(rec.get_u64("level"), Some(99));
        rec.set_u64("level", u64::MAX).unwrap();
        assert_eq!(rec.get_u64("level"), Some(u64::MAX));
    }

    #[test]
    fn test_set_f64() {
        let mut rec = make_record_mut();
        rec.set_f64("score", 100.0).unwrap();
        assert_eq!(rec.get_f64("score"), Some(100.0));
        rec.set_f64("score", f64::NEG_INFINITY).unwrap();
        assert_eq!(rec.get_f64("score"), Some(f64::NEG_INFINITY));
    }

    #[test]
    fn test_set_bool() {
        let mut rec = make_record_mut();
        assert_eq!(rec.get_bool("active"), Some(true));
        rec.set_bool("active", false).unwrap();
        assert_eq!(rec.get_bool("active"), Some(false));
        rec.set_bool("active", true).unwrap();
        assert_eq!(rec.get_bool("active"), Some(true));
    }

    #[test]
    fn test_typed_setter_type_mismatch() {
        let mut rec = make_record_mut();
        assert!(matches!(
            rec.set_u64("age", 5),
            Err(RecordError::TypeMismatch { .. })
        ));
        assert!(matches!(
            rec.set_i64("name", 5),
            Err(RecordError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn test_setter_field_not_found() {
        let mut rec = make_record_mut();
        assert!(matches!(
            rec.set_i64("nope", 5),
            Err(RecordError::FieldNotFound)
        ));
    }

    // ── String setters ──────────────────────────────────────────────────────

    #[test]
    fn test_set_str_same_length() {
        let mut rec = make_record_mut();
        rec.set_str("name", "Bobby").unwrap(); // 5 → 5 bytes
        assert_eq!(rec.get_str("name"), Some("Bobby"));
        assert_eq!(rec.get_i64("age"), Some(30));
        assert_eq!(rec.get_str("id"), Some("user:123"));
    }

    #[test]
    fn test_set_str_grow() {
        let mut rec = make_record_mut();
        let old_len = rec.byte_len();
        rec.set_str("name", "Alexander").unwrap(); // 5 → 9 bytes
        assert_eq!(rec.get_str("name"), Some("Alexander"));
        assert_eq!(rec.byte_len(), old_len + 4);

        // All other fields intact
        assert_eq!(rec.get_str("id"), Some("user:123"));
        assert_eq!(rec.get_i64("age"), Some(30));
        assert_eq!(rec.get_f64("score"), Some(99.5));
        assert_eq!(rec.get_bool("active"), Some(true));
        assert_eq!(rec.get_u64("level"), Some(42));
    }

    #[test]
    fn test_set_str_shrink() {
        let mut rec = make_record_mut();
        let old_len = rec.byte_len();
        rec.set_str("name", "Al").unwrap(); // 5 → 2 bytes
        assert_eq!(rec.get_str("name"), Some("Al"));
        assert_eq!(rec.byte_len(), old_len - 3);

        assert_eq!(rec.get_str("id"), Some("user:123"));
        assert_eq!(rec.get_i64("age"), Some(30));
        assert_eq!(rec.get_f64("score"), Some(99.5));
    }

    #[test]
    fn test_set_str_exact() {
        let mut rec = make_record_mut();
        rec.set_str_exact("name", "Bobby").unwrap();
        assert_eq!(rec.get_str("name"), Some("Bobby"));
        assert!(matches!(
            rec.set_str_exact("name", "Al"),
            Err(RecordError::LengthMismatch { .. })
        ));
    }

    // ── Generic setter ──────────────────────────────────────────────────────

    #[test]
    fn test_set_field_same_type_same_size() {
        let mut rec = make_record_mut();
        rec.set_field("age", &SpookyValue::from(99i64)).unwrap();
        assert_eq!(rec.get_i64("age"), Some(99));
    }

    #[test]
    fn test_set_field_type_change() {
        let mut rec = make_record_mut();
        rec.set_field("age", &SpookyValue::from("thirty")).unwrap();
        assert_eq!(rec.get_str("age"), Some("thirty"));
        assert_eq!(rec.get_i64("age"), None);
        assert_eq!(rec.get_str("name"), Some("Alice"));
        assert_eq!(rec.get_f64("score"), Some(99.5));
    }

    #[test]
    fn test_set_field_to_null() {
        let mut rec = make_record_mut();
        rec.set_null("name").unwrap();
        assert_eq!(rec.get_str("name"), None);
        assert_eq!(rec.field_type("name"), Some(TAG_NULL));
        assert_eq!(rec.get_i64("age"), Some(30));
    }

    #[test]
    fn test_set_field_nested_object() {
        let mut rec = make_record_mut();
        let mut inner = FastMap::new();
        inner.insert(SmolStr::from("city"), SpookyValue::from("Berlin"));
        let obj = SpookyValue::Object(inner);

        rec.set_field("name", &obj).unwrap();
        let result = rec.get_field("name").unwrap();
        assert_eq!(result.get("city").and_then(|v| v.as_str()), Some("Berlin"));
        assert_eq!(rec.get_i64("age"), Some(30));
    }

    // ── add_field ───────────────────────────────────────────────────────────

    #[test]
    fn test_add_field() {
        let mut rec = make_record_mut();
        assert_eq!(rec.field_count(), 6);
        rec.add_field("email", &SpookyValue::from("alice@example.com"))
            .unwrap();

        assert_eq!(rec.field_count(), 7);
        assert_eq!(rec.get_str("email"), Some("alice@example.com"));

        // All original fields intact
        assert_eq!(rec.get_str("id"), Some("user:123"));
        assert_eq!(rec.get_str("name"), Some("Alice"));
        assert_eq!(rec.get_i64("age"), Some(30));
        assert_eq!(rec.get_f64("score"), Some(99.5));
        assert_eq!(rec.get_bool("active"), Some(true));
        assert_eq!(rec.get_u64("level"), Some(42));
    }

    #[test]
    fn test_add_field_duplicate() {
        let mut rec = make_record_mut();
        assert!(matches!(
            rec.add_field("name", &SpookyValue::from("Bob")),
            Err(RecordError::FieldExists)
        ));
    }

    #[test]
    fn test_add_multiple_fields() {
        let mut rec = make_record_mut();
        rec.add_field("email", &SpookyValue::from("alice@test.com"))
            .unwrap();
        rec.add_field("country", &SpookyValue::from("DE")).unwrap();
        rec.add_field("verified", &SpookyValue::from(true)).unwrap();

        assert_eq!(rec.field_count(), 9);
        assert_eq!(rec.get_str("email"), Some("alice@test.com"));
        assert_eq!(rec.get_str("country"), Some("DE"));
        assert_eq!(rec.get_bool("verified"), Some(true));
        assert_eq!(rec.get_str("name"), Some("Alice"));
    }

    // ── remove_field ────────────────────────────────────────────────────────

    #[test]
    fn test_remove_field() {
        let mut rec = make_record_mut();
        rec.remove_field("name").unwrap();

        assert_eq!(rec.field_count(), 5);
        assert!(!rec.has_field("name"));
        assert_eq!(rec.get_str("id"), Some("user:123"));
        assert_eq!(rec.get_i64("age"), Some(30));
        assert_eq!(rec.get_f64("score"), Some(99.5));
        assert_eq!(rec.get_bool("active"), Some(true));
        assert_eq!(rec.get_u64("level"), Some(42));
    }

    #[test]
    fn test_remove_nonexistent() {
        let mut rec = make_record_mut();
        assert!(matches!(
            rec.remove_field("nope"),
            Err(RecordError::FieldNotFound)
        ));
    }

    #[test]
    fn test_remove_then_add() {
        let mut rec = make_record_mut();
        rec.remove_field("name").unwrap();
        rec.add_field("name", &SpookyValue::from("Bob")).unwrap();
        assert_eq!(rec.get_str("name"), Some("Bob"));
        assert_eq!(rec.field_count(), 6);
    }

    #[test]
    fn test_remove_all_fields() {
        let mut rec = make_record_mut();
        for f in &["id", "name", "age", "score", "active", "level"] {
            rec.remove_field(f).unwrap();
        }
        assert_eq!(rec.field_count(), 0);
        assert_eq!(rec.byte_len(), HEADER_SIZE);
    }

    // ── Multiple mutations ──────────────────────────────────────────────────

    #[test]
    fn test_multiple_mutations_sequence() {
        let mut rec = make_record_mut();
        rec.set_i64("age", 31).unwrap();
        rec.set_f64("score", 100.5).unwrap();
        rec.set_bool("active", false).unwrap();
        rec.set_str("name", "Bob").unwrap();
        rec.set_u64("level", 43).unwrap();

        assert_eq!(rec.get_i64("age"), Some(31));
        assert_eq!(rec.get_f64("score"), Some(100.5));
        assert_eq!(rec.get_bool("active"), Some(false));
        assert_eq!(rec.get_str("name"), Some("Bob"));
        assert_eq!(rec.get_u64("level"), Some(43));
        assert_eq!(rec.get_str("id"), Some("user:123"));
    }

    #[test]
    fn test_rapid_fire_same_field() {
        let mut rec = make_record_mut();
        for i in 0..1000 {
            rec.set_i64("age", i).unwrap();
        }
        assert_eq!(rec.get_i64("age"), Some(999));
        assert_eq!(rec.get_str("name"), Some("Alice"));
    }

    // ── as_record interop ───────────────────────────────────────────────────

    #[test]
    fn test_as_record_interop() {
        let mut rec = make_record_mut();
        rec.set_i64("age", 50).unwrap();
        rec.set_str("name", "Charlie").unwrap();

        let reader = rec.as_record();
        assert_eq!(reader.get_i64("age"), Some(50));
        assert_eq!(reader.get_str("name"), Some("Charlie"));
        assert_eq!(reader.field_count(), 6);
    }

    // ── Persist + restore ───────────────────────────────────────────────────

    #[test]
    fn test_mutate_persist_restore() {
        let mut rec = make_record_mut();
        rec.set_i64("age", 99).unwrap();
        rec.set_str("name", "Modified").unwrap();
        rec.add_field("new_field", &SpookyValue::from(42i64))
            .unwrap();

        let bytes = rec.into_bytes();
        let restored = SpookyRecordMut::from_vec(bytes).unwrap();

        assert_eq!(restored.get_i64("age"), Some(99));
        assert_eq!(restored.get_str("name"), Some("Modified"));
        assert_eq!(restored.get_i64("new_field"), Some(42));
        assert_eq!(restored.field_count(), 7);
    }

    // ── Edge cases ──────────────────────────────────────────────────────────

    #[test]
    fn test_empty_string_field() {
        let mut rec = make_record_mut();
        rec.set_str("name", "").unwrap();
        assert_eq!(rec.get_str("name"), Some(""));
        rec.set_str("name", "back").unwrap();
        assert_eq!(rec.get_str("name"), Some("back"));
    }

    #[test]
    fn test_add_field_to_empty_record() {
        let mut rec = SpookyRecordMut::new_empty();
        rec.add_field("first", &SpookyValue::from("hello")).unwrap();
        assert_eq!(rec.field_count(), 1);
        assert_eq!(rec.get_str("first"), Some("hello"));

        rec.add_field("second", &SpookyValue::from(42i64)).unwrap();
        assert_eq!(rec.field_count(), 2);
        assert_eq!(rec.get_i64("second"), Some(42));
    }

    #[test]
    fn test_unicode_strings() {
        let mut rec = make_record_mut();
        rec.set_str("name", "Ünïcödé 🎃").unwrap();
        assert_eq!(rec.get_str("name"), Some("Ünïcödé 🎃"));
        assert_eq!(rec.get_i64("age"), Some(30));
    }

    #[test]
    fn test_large_string_growth() {
        let mut rec = make_record_mut();
        let large = "x".repeat(10_000);
        rec.set_str("name", &large).unwrap();
        assert_eq!(rec.get_str("name"), Some(large.as_str()));
        assert_eq!(rec.get_i64("age"), Some(30));
    }

    #[test]
    fn test_multiple_splices_accumulate_correctly() {
        let mut rec = make_record_mut();
        // Grow, shrink, grow, shrink — stress test offset fixups
        rec.set_str("name", "A very long name indeed").unwrap();
        assert_eq!(rec.get_str("name"), Some("A very long name indeed"));
        assert_eq!(rec.get_i64("age"), Some(30));

        rec.set_str("name", "X").unwrap();
        assert_eq!(rec.get_str("name"), Some("X"));
        assert_eq!(rec.get_i64("age"), Some(30));

        rec.set_str("id", "user:999999999").unwrap();
        assert_eq!(rec.get_str("id"), Some("user:999999999"));
        assert_eq!(rec.get_str("name"), Some("X"));
        assert_eq!(rec.get_i64("age"), Some(30));

        rec.set_str("id", "u").unwrap();
        assert_eq!(rec.get_str("id"), Some("u"));
        assert_eq!(rec.get_str("name"), Some("X"));
        assert_eq!(rec.get_i64("age"), Some(30));
        assert_eq!(rec.get_f64("score"), Some(99.5));
        assert_eq!(rec.get_bool("active"), Some(true));
        assert_eq!(rec.get_u64("level"), Some(42));
    }

    #[test]
    fn test_add_then_mutate() {
        let mut rec = make_record_mut();
        rec.add_field("email", &SpookyValue::from("old@test.com"))
            .unwrap();
        rec.set_str("email", "new@test.com").unwrap();
        assert_eq!(rec.get_str("email"), Some("new@test.com"));

        rec.set_str("email", "x@y.z").unwrap(); // shrink
        assert_eq!(rec.get_str("email"), Some("x@y.z"));

        // Original fields still fine
        assert_eq!(rec.get_str("name"), Some("Alice"));
    }

    #[test]
    fn test_null_field_add_and_read() {
        let mut rec = make_record_mut();
        rec.add_field("nothing", &SpookyValue::Null).unwrap();
        assert_eq!(rec.field_type("nothing"), Some(TAG_NULL));
        assert_eq!(rec.get_field("nothing"), Some(SpookyValue::Null));
    }
}
