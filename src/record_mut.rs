pub struct SpookyRecordMut {
    dataBuff: Vec<u8>,
    field_count: u32,
    /// Generation counter, bumped on every layout-changing mutation.
    /// Used to detect stale FieldSlots.
    generation: u32,
}
