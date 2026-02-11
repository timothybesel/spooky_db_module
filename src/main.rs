//use redb::{Database, Error, ReadableDatabase, ReadableTable, TableDefinition};
mod spooky_value;

use ciborium;
use serde_json::{Result, Value};
use spooky_value::{SpookyNumber, SpookyValue};

const TEST_JSON: &str = r#"{
  "age": 28,
  "id": "user:abc123",
  "name": "Alice",
  "email": "alice@example.com",
  "profile": { "bio": "Developer", "avatar": "https://..." },
  "created_at": "2024-01-15T10:30:00Z"
}"#;

/// Header at the start of every hybrid record
#[repr(C, packed)]
struct RecordHeader {
    field_count: u32,
    _reserved: u32,
}

/// Field index entry (fixed size for easy offset calculation)
#[repr(C, packed)]
struct FieldIndexEntry {
    name_hash: u64,
    data_offset: u32,
    data_length: u32,
    type_tag: u8,
    _padding: [u8; 3], // Align to 16 bytes
}

/// Zero-copy record reader
pub struct HybridRecord<'a> {
    bytes: &'a [u8],
    header: &'a RecordHeader,
    index: &'a [FieldIndexEntry],
}

fn serielize_data(data: &SpookyValue) {
    let fields: Vec<(&str, &SpookyValue)> = match data {
        SpookyValue::Object(map) => map.iter().map(|(k, v)| (k.as_str(), v)).collect(),
        _ => panic!("Can only serialize objects as records"),
    };

    let field_count = fields.len() as u32;
    let header_size = std::mem::size_of::<RecordHeader>();
    let index_size = field_count as usize * std::mem::size_of::<FieldIndexEntry>();
    let _data_start = header_size + index_size;
}

fn serialize_field_value(value: &SpookyValue) -> (Vec<u8>, u8) {
    match value {
        SpookyValue::Null => (vec![], 0),
        SpookyValue::Bool(b) => (vec![if *b { 1 } else { 0 }], 1),
        SpookyValue::Number(n) => match n {
            SpookyNumber::I64(i) => (i.to_le_bytes().to_vec(), 2),
            SpookyNumber::F64(f) => (f.to_le_bytes().to_vec(), 3),
            // ACHTUNG: Du hattest hier Tag 4, aber Tag 4 ist schon Bool!
            // Ich habe es auf 6 geändert.
            SpookyNumber::U64(u) => (u.to_le_bytes().to_vec(), 4),
        },
        SpookyValue::Str(s) => (s.as_bytes().to_vec(), 5),

        // Der neue Teil:
        SpookyValue::Array(_) | SpookyValue::Object(_) => {
            let mut buf = Vec::new();
            ciborium::into_writer(value, &mut buf).expect("Failed to serialize complex type");
            (buf, 6)
        }
    }
}

fn main() -> Result<()> {
    let res: Value = serde_json::from_str(TEST_JSON)?;
    let spooky_res = SpookyValue::from(res);
    serielize_data(&spooky_res);
    Ok(())
}
