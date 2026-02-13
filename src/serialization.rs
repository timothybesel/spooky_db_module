use super::error::RecordError;
use super::spooky_value::{SpookyNumber, SpookyValue};
use super::types::*;
use arrayvec::ArrayVec;
use smol_str::SmolStr;
use std::collections::BTreeMap;
use xxhash_rust::const_xxh64::xxh64;

// ─── Writer ─────────────────────────────────────────────────────────────────

/// Serialize a SpookyValue::Object into the hybrid binary format.
/// Flat fields are stored as native bytes, nested objects/arrays as CBOR.
///
/// **IMPORTANT**: The index is sorted by name_hash. This is required for
/// O(log n) binary search in both SpookyRecord and SpookyRecordMut.

/// Serialize a single field value into (bytes, type_tag).
#[inline]
pub fn write_field_into(buf: &mut Vec<u8>, value: &SpookyValue) -> Result<u8, RecordError> {
    Ok(match value {
        SpookyValue::Null => TAG_NULL,
        SpookyValue::Bool(b) => {
            buf.push(*b as u8);
            TAG_BOOL
        }
        SpookyValue::Number(n) => {
            // All numeric types are exactly 8 bytes — reserve once, write directly
            buf.reserve(8);
            let len = buf.len();
            let bytes = match n {
                SpookyNumber::I64(i) => {
                    let b = i.to_le_bytes();
                    (b, TAG_I64)
                }
                SpookyNumber::F64(f) => {
                    let b = f.to_le_bytes();
                    (b, TAG_F64)
                }
                SpookyNumber::U64(u) => {
                    let b = u.to_le_bytes();
                    (b, TAG_U64)
                }
            };
            // SAFETY: we just reserved 8 bytes
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.0.as_ptr(), buf.as_mut_ptr().add(len), 8);
                buf.set_len(len + 8);
            }
            bytes.1
        }
        SpookyValue::Str(s) => {
            buf.extend_from_slice(s.as_bytes());
            TAG_STR
        }
        SpookyValue::Array(_) | SpookyValue::Object(_) => {
            cbor4ii::serde::to_writer(&mut *buf, value)
                .map_err(|e| RecordError::CborError(e.to_string()))?;
            TAG_NESTED_CBOR
        }
    })
}

pub fn prepare_buf(
    map: &BTreeMap<SmolStr, SpookyValue>,
    buf: &mut Vec<u8>,
    field_count: usize,
) -> Result<(), RecordError> {
    // 3. Sort
    // Collect references & hashes to avoid unnecessary data copies.
    // // Stack-allocated sort buffer — no heap allocation for ≤32 fields
    // //TODO: has to be check if this could be panic in normal sitations
    let mut entries: ArrayVec<(&SpookyValue, u64), 32> = ArrayVec::new();

    for (key, value) in map.iter() {
        // Compute the hash for the key
        let hash = xxh64(key.as_bytes(), 0);
        entries
            .try_push((value, hash))
            .map_err(|_| RecordError::TooManyFields)?;
    }

    // Sort for O(log n) lookup in the reader
    entries.sort_unstable_by_key(|(_, hash)| *hash);

    // Write header (field count)
    buf[0..4].copy_from_slice(&(field_count as u32).to_le_bytes());

    // 4. Loop & Write
    for (i, (value, hash)) in entries.iter().enumerate() {
        // A. Append data to value area
        let data_offset = buf.len();
        let tag = write_field_into(buf, value)?;
        let data_length = buf.len() - data_offset;

        // B. Fill in the index entry
        // All arithmetic must use usize
        let idx = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
        let entry = &mut buf[idx..idx + INDEX_ENTRY_SIZE];
        entry[0..8].copy_from_slice(&hash.to_le_bytes());
        entry[8..12].copy_from_slice(&(data_offset as u32).to_le_bytes());
        entry[12..16].copy_from_slice(&(data_length as u32).to_le_bytes());
        entry[16] = tag;
    }
    Ok(())
}

// ════════════════════════════════════════════════════════════════════════
// Serializations patterns
// ════════════════════════════════════════════════════════════════════════

pub fn serialize(map: &BTreeMap<SmolStr, SpookyValue>) -> Result<(Vec<u8>, usize), RecordError> {
    let field_count = map.len();

    // NOTE: All arithmetic must use usize
    let index_size = field_count * INDEX_ENTRY_SIZE;
    let data_start = HEADER_SIZE + index_size;

    // 2. Prepare buffer (rough capacity estimate)
    let mut buf: Vec<u8> = Vec::with_capacity(data_start + field_count * 32);
    // CRITICAL: resize fills the header/index area with 0,
    // so we can write by index (buf[idx] = ...) immediately.
    buf.resize(data_start, 0);

    prepare_buf(&map, &mut buf, field_count)?;

    // 5. Return
    Ok((buf, field_count))
}

/// Serialize a SpookyValue::Object into the hybrid binary format.
/// Flat fields are stored as native bytes, nested objects/arrays as CBOR.
///
/// **IMPORTANT**: The index is sorted by name_hash. This is required for
/// O(log n) binary search in both SpookyRecord and SpookyRecordMut.
pub fn from_spooky(data: &SpookyValue) -> Result<(Vec<u8>, usize), RecordError> {
    let map = match data {
        SpookyValue::Object(map) => map,
        _ => return Err(RecordError::InvalidBuffer),
    };

    let (buf, field_count) = serialize(map)?;
    Ok((buf, field_count))
}

/// Create a mutable record by taking ownership of an existing serialized buffer.
///
/// The buffer **must** have a sorted index (produced by `serialize_record()`,
/// `from_spooky_value()`, or a previous `into_bytes()`).
/// Validate a byte slice and extract field_count.
pub fn from_bytes(buf: &[u8]) -> Result<(&[u8], usize), RecordError> {
    if buf.len() < HEADER_SIZE {
        return Err(RecordError::InvalidBuffer);
    }
    let field_count = u32::from_le_bytes(
        buf[0..4]
            .try_into()
            .map_err(|_| RecordError::InvalidBuffer)?,
    ) as usize;
    let min_size = HEADER_SIZE + field_count * INDEX_ENTRY_SIZE;
    if buf.len() < min_size {
        return Err(RecordError::InvalidBuffer);
    }
    Ok((buf, field_count))
}

// Serialize a SpookyValue::Object into a reusable buffer.
///
/// Identical to `serialize`, but reuses the caller's Vec to eliminate
/// allocations when serializing many records in sequence (sync ingestion,
/// snapshot rebuild). The buffer is cleared but retains its capacity.
///
/// **IMPORTANT**: The index is sorted by name_hash.
pub fn serialize_into(
    map: &BTreeMap<SmolStr, SpookyValue>,
    buf: &mut Vec<u8>,
) -> Result<usize, RecordError> {
    let field_count = map.len();
    // NOTE: All arithmetic must use usize
    let index_size = field_count * INDEX_ENTRY_SIZE;
    let data_start = HEADER_SIZE + index_size;

    // Reuse buffer — clears but retains capacity
    buf.clear();
    buf.reserve(data_start + field_count * 16);
    buf.resize(data_start, 0);

    prepare_buf(&map, buf, field_count)?;
    // 5. Return
    Ok(field_count)
}

pub fn serialize_into_buf(data: &SpookyValue, buf: &mut Vec<u8>) -> Result<(), RecordError> {
    let map = match data {
        SpookyValue::Object(map) => map,
        _ => return Err(RecordError::InvalidBuffer),
    };

    let _ = serialize_into(map, buf)?;

    Ok(())
}
