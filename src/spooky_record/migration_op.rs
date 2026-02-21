use arrayvec::ArrayVec;

use super::read_op::SpookyReadable;
use super::record_mut::SpookyRecordMut;
use crate::error::RecordError;
use crate::serialization::write_field_into;
use crate::types::*;
use xxhash_rust::xxh64::xxh64;

impl SpookyRecordMut {
    // ════════════════════════════════════════════════════════════════════════
    // Structural mutations — add/remove fields
    // ════════════════════════════════════════════════════════════════════════

    /// Add a new field. Maintains sorted index order.
    ///
    /// Rebuilds the buffer with the new field inserted at the correct
    /// sorted position. This is simpler and less error-prone than in-place
    /// index insertion with offset fixups.
    pub fn add_field<V: crate::serialization::RecordSerialize>(&mut self, name: &str, value: &V) -> Result<(), RecordError> {
        let hash = xxh64(name.as_bytes(), 0);

        if self.find_field(name).is_ok() {
            return Err(RecordError::FieldExists);
        }

        let mut new_bytes = Vec::new();
        let new_tag = write_field_into(&mut new_bytes, value)?;
        let insert_pos = self.find_insert_pos(hash);
        let old_n = self.field_count;
        let new_n = old_n + 1;

        let mut scratch = Vec::new();
        self.rebuild_buffer_with(&mut scratch, old_n, new_n, |i| {
            if i == insert_pos {
                FieldSource::New {
                    hash,
                    data: &new_bytes,
                    tag: new_tag,
                }
            } else {
                let src_i = if i < insert_pos { i } else { i - 1 };
                FieldSource::Existing(src_i)
            }
        })?;

        self.data_buf = scratch;
        self.field_count = new_n;
        self.generation += 1;
        Ok(())
    }

    /// Remove a field from the record.
    ///
    /// Rebuilds the buffer without the removed field.
    pub fn remove_field(&mut self, name: &str) -> Result<(), RecordError> {
        let (remove_pos, _) = self.find_field(name)?;
        let old_n = self.field_count;
        let new_n = old_n - 1;

        if new_n == 0 {
            self.data_buf.clear();
            self.data_buf.resize(HEADER_SIZE, 0);
            self.field_count = 0;
            self.generation += 1;
            return Ok(());
        }

        let mut scratch = Vec::new();
        self.rebuild_buffer_with(&mut scratch, old_n, new_n, |i| {
            let src_i = if i < remove_pos { i } else { i + 1 };
            FieldSource::Existing(src_i)
        })?;

        self.data_buf = scratch;
        self.field_count = new_n;
        self.generation += 1;
        Ok(())
    }

    // ════════════════════════════════════════════════════════════════════════
    // Internal: buffer rebuild helpers
    // ════════════════════════════════════════════════════════════════════════

    /// Rebuild the record buffer into `scratch`, reusing its existing allocation.
    ///
    /// `scratch` is cleared and filled with the rebuilt record. On success,
    /// callers should swap `scratch` into `self.data_buf`. Accepting a caller-
    /// supplied scratch buffer allows callers that perform multiple structural
    /// mutations in sequence (e.g. a migration loop) to reuse the same buffer
    /// across iterations, saving one heap allocation per mutation.
    ///
    /// This avoids the duplicated rebuild logic between add_field and
    /// remove_field (and any future structural mutations).
    fn rebuild_buffer_with<'a, F>(
        &self,
        scratch: &mut Vec<u8>,
        old_n: usize,
        new_n: usize,
        field_source: F,
    ) -> Result<(), RecordError>
    where
        F: Fn(usize) -> FieldSource<'a>,
    {
        // Pre-read all existing field metadata in one pass
        let old_entries = self.read_all_index_entries(old_n)?;

        // Calculate sizes
        let new_data_start = HEADER_SIZE + new_n * INDEX_ENTRY_SIZE;
        let total_data: usize = (0..new_n)
            .map(|i| match field_source(i) {
                FieldSource::New { data, .. } => data.len(),
                FieldSource::Existing(src) => old_entries[src].data_len,
            })
            .sum();

        // Reuse the existing allocation: clear and resize instead of a fresh Vec.
        scratch.clear();
        scratch.resize(new_data_start + total_data, 0u8);
        scratch[0..4].copy_from_slice(&(new_n as u32).to_le_bytes());

        let mut data_cursor = new_data_start;

        for dst_i in 0..new_n {
            let (hash, len, tag) = match field_source(dst_i) {
                FieldSource::New { hash, data, tag } => {
                    scratch[data_cursor..data_cursor + data.len()].copy_from_slice(data);
                    (hash, data.len(), tag)
                }
                FieldSource::Existing(src_i) => {
                    let e = &old_entries[src_i];
                    if e.data_len > 0 {
                        scratch[data_cursor..data_cursor + e.data_len].copy_from_slice(
                            &self.data_buf[e.data_offset..e.data_offset + e.data_len],
                        );
                    }
                    (e.name_hash, e.data_len, e.type_tag)
                }
            };

            // Write index entry — single slice bounds check, then relative writes
            let idx = HEADER_SIZE + dst_i * INDEX_ENTRY_SIZE;
            let entry = &mut scratch[idx..idx + INDEX_ENTRY_SIZE];
            entry[0..8].copy_from_slice(&hash.to_le_bytes());
            entry[8..12].copy_from_slice(&(data_cursor as u32).to_le_bytes());
            entry[12..16].copy_from_slice(&(len as u32).to_le_bytes());
            entry[16] = tag;

            data_cursor += len;
        }

        Ok(())
    }

    /// Read all index entries in one pass into a stack-allocated ArrayVec.
    ///
    /// The 32-field hard limit (enforced by serialization) ensures the
    /// ArrayVec capacity of 32 is never exceeded in valid records.
    /// One `try_push` failure signals a corrupt buffer (field_count > 32).
    #[inline]
    fn read_all_index_entries(&self, n: usize) -> Result<ArrayVec<IndexEntry, 32>, RecordError> {
        let mut entries = ArrayVec::<IndexEntry, 32>::new();
        for i in 0..n {
            let e = self.read_index(i).ok_or(RecordError::InvalidBuffer)?;
            entries.try_push(e).map_err(|_| RecordError::InvalidBuffer)?;
        }
        Ok(entries)
    }
}

/// Describes where a field in the rebuilt buffer comes from.
enum FieldSource<'a> {
    /// A newly inserted field with its serialized data.
    New { hash: u64, data: &'a [u8], tag: u8 },
    /// An existing field, referenced by its position in the old index.
    Existing(usize),
}
