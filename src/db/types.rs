use rustc_hash::FxHasher;
use smol_str::SmolStr;
use std::collections::HashSet;
use std::hash::BuildHasherDefault;
use std::num::NonZeroUsize;
use thiserror::Error;

pub type Weight = i64;
pub type RowKey = SmolStr;
pub type FastMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FxHasher>>;
pub type FastHashSet<T> = HashSet<T, BuildHasherDefault<FxHasher>>;
pub type ZSet = FastMap<RowKey, Weight>;

/// Alias for table names — documents that this string must not contain ':'.
pub type TableName = SmolStr;

/// Configuration for [`SpookyDb::new_with_config`].
pub struct SpookyDbConfig {
    /// Maximum number of records to keep in the LRU row cache.
    ///
    /// When this limit is reached, the least-recently-written record is evicted.
    /// Evicted records remain on disk in redb and are re-read on the next access.
    ///
    /// Default: 10 000 records (~10–500 MB depending on average record size).
    pub cache_capacity: NonZeroUsize,
}

impl Default for SpookyDbConfig {
    fn default() -> Self {
        Self {
            cache_capacity: NonZeroUsize::new(10_000).unwrap(),
        }
    }
}

#[derive(Debug, Error)]
pub enum SpookyDbError {
    #[error("redb error: {0}")]
    Redb(#[from] redb::Error),
    #[error("serialization error: {0}")]
    Serialization(String),
    /// Table name contains ':' or key format is otherwise invalid.
    #[error("invalid key: {0}")]
    InvalidKey(String),
}

impl From<redb::DatabaseError> for SpookyDbError {
    fn from(e: redb::DatabaseError) -> Self {
        SpookyDbError::Redb(e.into())
    }
}

impl From<redb::TransactionError> for SpookyDbError {
    fn from(e: redb::TransactionError) -> Self {
        SpookyDbError::Redb(e.into())
    }
}

impl From<redb::TableError> for SpookyDbError {
    fn from(e: redb::TableError) -> Self {
        SpookyDbError::Redb(e.into())
    }
}

impl From<redb::CommitError> for SpookyDbError {
    fn from(e: redb::CommitError) -> Self {
        SpookyDbError::Redb(e.into())
    }
}

impl From<redb::StorageError> for SpookyDbError {
    fn from(e: redb::StorageError) -> Self {
        SpookyDbError::Redb(e.into())
    }
}

impl From<crate::error::RecordError> for SpookyDbError {
    fn from(e: crate::error::RecordError) -> Self {
        SpookyDbError::Serialization(e.to_string())
    }
}

/// A single mutation ready for `apply_batch`.
///
/// `data` MUST be pre-serialized SpookyRecord bytes (from `from_cbor` /
/// `serialize_into`). Serialization happens BEFORE `begin_write()` to
/// minimize write lock hold time.
///
/// # Limits
///
/// Records are capped at 32 fields. Attempting to serialize a record with more
/// than 32 fields returns [`SpookyDbError`] wrapping `RecordError::TooManyFields`.
pub struct DbMutation {
    pub table: SmolStr,
    pub id: SmolStr,
    pub op: Operation,
    /// `None` for `Delete`; `Some(bytes)` for `Create` / `Update`.
    pub data: Option<Vec<u8>>,
    /// Explicit version. If `None`, VERSION_TABLE entry is left unchanged.
    ///
    /// # Version tracking
    ///
    /// `version: None` means "do not update the version entry". The previous version
    /// entry (if any) is left unchanged. Callers must provide `version: Some(v)` on
    /// every mutation where version tracking matters, or accept that `get_version` may
    /// return a stale value after an update with `version: None`.
    pub version: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Record did not exist before. ZSet weight += 1.
    Create,
    /// Record existed, bytes replaced. ZSet weight unchanged.
    Update,
    /// Record removed. ZSet weight -= 1 (entry removed at 0).
    Delete,
}

impl Operation {
    /// Weight delta this operation contributes to the ZSet.
    pub fn weight(&self) -> i64 {
        match self {
            Operation::Create => 1,
            Operation::Delete => -1,
            Operation::Update => 0,
        }
    }
}

/// Return value of `apply_batch`. Contains all per-table deltas accumulated
/// in a single pass — no extra allocations after the batch commit.
pub struct BatchMutationResult {
    /// Per-table ZSet weight deltas (Create = +1, Delete = -1, Update = 0).
    /// Key: table name → ZSet<record_id, weight_delta>.
    pub membership_deltas: FastMap<SmolStr, ZSet>,
    /// Per-table set of record IDs whose content was written (Create or Update).
    pub content_updates: FastMap<SmolStr, FastHashSet<SmolStr>>,
    /// Tables that had at least one mutation (deduplicated).
    pub changed_tables: Vec<SmolStr>,
}

/// One record for `bulk_load`. `data` must be pre-serialized SpookyRecord bytes.
pub struct BulkRecord {
    pub table: SmolStr,
    pub id: SmolStr,
    pub data: Vec<u8>,
    /// Written to VERSION_TABLE when `Some`. Pass `None` to skip version tracking.
    pub version: Option<u64>,
}
