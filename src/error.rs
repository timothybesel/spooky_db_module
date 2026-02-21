// ─── Error ──────────────────────────────────────────────────────────────────
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RecordError {
    #[error("Can't Serialize none Object Types")]
    SerializationNotObject,
    #[error("Invalid buffer structure")]
    InvalidBuffer,
    #[error("record exceeds the 32-field limit")]
    TooManyFields,
    #[error("Field not found")]
    FieldNotFound,
    #[error("Type mismatch: expected {expected}, got {actual}")]
    TypeMismatch { expected: u8, actual: u8 },
    #[error("Length mismatch: expected {expected}, got {actual}")]
    LengthMismatch { expected: usize, actual: usize },
    #[error("Field already exists")]
    FieldExists,
    #[error("CBOR error: {0}")]
    CborError(String),
    #[error("Unknown type tag: {0}")]
    UnknownTypeTag(u8),
}
