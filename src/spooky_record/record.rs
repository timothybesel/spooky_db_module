use super::read_op::SpookyReadable;
use crate::types::FieldIter;

// ─── Reader (zero-copy) ────────────────────────────────────────────────────
/// Zero-copy reader over a hybrid record byte slice.
/// No parsing happens until you request a specific field.
#[derive(Debug, Clone, Copy)]
pub struct SpookyRecord<'a> {
    pub data_buf: &'a [u8],
    pub field_count: usize,
}

impl<'a> SpookyRecord<'a> {
    #[inline]
    pub fn new(data_buf: &'a [u8], field_count: usize) -> Self {
        #[cfg(debug_assertions)]
        {
            // Verify caller-provided field_count matches the header.
            let header_count = u32::from_le_bytes(data_buf[0..4].try_into().expect("buf too short")) as usize;
            debug_assert_eq!(
                field_count, header_count,
                "SpookyRecord::new: caller field_count {field_count} != header {header_count}"
            );
        }
        Self {
            data_buf,
            field_count,
        }
    }
}

impl<'a> SpookyReadable for SpookyRecord<'a> {
    #[inline]
    fn data_buf(&self) -> &[u8] {
        self.data_buf
    }

    #[inline]
    fn field_count(&self) -> usize {
        self.field_count
    }

    /// Iterate over all raw fields (zero-copy)
    #[inline]
    fn iter_fields(&self) -> FieldIter<'a> {
        FieldIter {
            record: *self, // Copy, not clone — it's just a slice + usize
            pos: 0,
        }
    }
}
