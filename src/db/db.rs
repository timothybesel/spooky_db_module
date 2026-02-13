use std::path::Path;
use redb::{Database, ReadableDatabase, TableDefinition};
use crate::spooky_record::SpookyRecord;


// Table definitions
// Key: String (record ID/key)
// Value: Byte slice (serialized SpookyRecord)
const RECORDS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("records");
const BADGES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("badges");

pub struct SpookyDb {
    db: Database,
}

impl SpookyDb {
    /// Open or create the database at the specified path.
    /// Also ensures that the required tables exist.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        
        // Initialize tables within a write transaction
        let write_txn = db.begin_write()?;
        {
            let _ = write_txn.open_table(RECORDS_TABLE)?;
            let _ = write_txn.open_table(BADGES_TABLE)?;
        }
        write_txn.commit()?;

        Ok(Self { db })
    }

    /// Ingest a record into the database.
    /// This functions as an upsert: if the key exists, it is overwritten.
    pub fn ingest_record(&self, key: &str, record: &SpookyRecord) -> Result<(), redb::Error> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(RECORDS_TABLE)?;
            table.insert(key, record.data_buf)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Update an existing record.
    /// Currently synonymous with ingest_record (upsert).
    pub fn update_record(&self, key: &str, record: &SpookyRecord) -> Result<(), redb::Error> {
        self.ingest_record(key, record)
    }

    /// Remove a record by its key.
    /// Returns true if the record existed and was removed, false otherwise.
    pub fn remove_record(&self, key: &str) -> Result<bool, redb::Error> {
        let write_txn = self.db.begin_write()?;
        let existed = {
            let mut table = write_txn.open_table(RECORDS_TABLE)?;
            table.remove(key)?.is_some()
        };
        write_txn.commit()?;
        Ok(existed)
    }

    /// Retrieve a record's raw bytes by key.
    pub fn get_record(&self, key: &str) -> Result<Option<Vec<u8>>, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(RECORDS_TABLE)?;
        
        if let Some(access) = table.get(key)? {
            // We must copy the data out because the read transaction implementation
            // doesn't allow returning a reference that outlives the transaction easily in this simple API.
            // SpookyRecord is zero-copy but expects a slice. We return Vec<u8> here so caller can wrap it.
            let val = access.value();
            Ok(Some(val.to_vec()))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use crate::spooky_record::record_mut::SpookyRecordMut;

    use std::collections::BTreeMap;
    use crate::serialization::serialize;
    use crate::spooky_value::{SpookyValue, SpookyNumber};
    use smol_str::SmolStr;

    fn create_test_record() -> (Vec<u8>, usize) {
        let mut map = BTreeMap::new();
        map.insert(SmolStr::new("id"), SpookyValue::Number(SpookyNumber::I64(123)));
        // serialize returns Result<(Vec<u8>, usize), RecordError>
        serialize(&map).expect("Should serialize")
    }

    #[test]
    fn test_spooky_db_basics() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_file = NamedTempFile::new()?;
        let db_path = tmp_file.path();
        
        let db = SpookyDb::new(db_path)?;

        let (data, fields) = create_test_record();
        let record = SpookyRecord::new(&data, fields);
        let key = "user:123";

        // 1. Ingest
        db.ingest_record(key, &record)?;

        // 2. Get
        let fetched_data = db.get_record(key)?.expect("Record should exist");
        assert_eq!(fetched_data, data);

        // 3. Update (change value)
        let mut rec_mut = SpookyRecordMut::new(data.clone(), fields);
        rec_mut.set_i64("id", 456)?; // Change ID
        let updated_record = rec_mut.as_record();
        db.update_record(key, &updated_record)?;

        let fetched_updated = db.get_record(key)?.expect("Record should exist");
        assert_eq!(fetched_updated, rec_mut.data_buf);

        // 4. Remove
        let removed = db.remove_record(key)?;
        assert!(removed);
        
        let fetched_after_remove = db.get_record(key)?;
        assert!(fetched_after_remove.is_none());

        Ok(())
    }
    
    #[test]
    fn test_badges_table_exists() -> Result<(), Box<dyn std::error::Error>> {
         let tmp_file = NamedTempFile::new()?;
        let db_path = tmp_file.path();
        
        let db = SpookyDb::new(db_path)?;
        
        // Verify we can write to badges table (even if we don't have specific methods for it yet)
        let write_txn = db.db.begin_write()?;
        {
            let mut table = write_txn.open_table(BADGES_TABLE)?;
            table.insert("badge:1", b"badge_data".as_slice())?;
        }
        write_txn.commit()?;
        
        let read_txn = db.db.begin_read()?;
        let table = read_txn.open_table(BADGES_TABLE)?;
        let val = table.get("badge:1")?;
        assert_eq!(val.unwrap().value(), b"badge_data");
        
        Ok(())
    }
}
