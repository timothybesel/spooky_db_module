use super::spooky_value::{SpookyNumber, SpookyValue};
use super::types::*;
use smol_str::SmolStr;

/// Decode a raw field reference into a SpookyValue.
pub fn decode_field(field: FieldRef) -> Option<SpookyValue> {
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
            let cbor_val: cbor4ii::core::Value = cbor4ii::serde::from_slice(field.data).ok()?;
            SpookyValue::from(cbor_val)
        }
        _ => return None,
    })
}
