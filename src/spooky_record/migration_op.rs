use super::read_op::SpookyReadable;
use super::record_mut::SpookyRecordMut;
use crate::error::RecordError;
use crate::serialization::write_field_into;
use crate::spooky_value::SpookyValue;
use crate::types::*;
use xxhash_rust::xxh64::xxh64;

impl SpookyRecordMut {
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

        let mut new_bytes = Vec::new();
        let new_tag = write_field_into(&mut new_bytes, value)?;
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
                meta.data_len,
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
                        .copy_from_slice(&self.data_buf[e_data_off..e_data_off + e_data_len]);
                }
                data_offset += e_data_len;
            }
        }

        self.data_buf = new_buf;
        self.field_count = new_n;
        self.generation += 1; // Added field, layout changed
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
            self.data_buf.clear();
            self.data_buf.resize(HEADER_SIZE, 0);
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
                meta.data_len,
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
                    .copy_from_slice(&self.data_buf[*e_data_off..*e_data_off + e_data_len]);
            }
            data_offset += e_data_len;
        }

        self.data_buf = new_buf;
        self.field_count = new_n;
        self.generation += 1; // Removed field, layout changed
        Ok(())
    }
}
