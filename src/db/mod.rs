#[allow(clippy::module_inception)]
pub mod db;
pub mod types;

pub use db::{DbBackend, SpookyDb};
pub use types::{
    BatchMutationResult, BulkRecord, DbMutation, FastHashSet, FastMap, Operation, RowStore,
    SpookyDbError, TableName, ZSet,
};
