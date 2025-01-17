use crate::blockstore::provider::BlockfileProvider;
use crate::blockstore::{Blockfile, BlockfileKey, HashMapBlockfile, Key, Value};
use crate::errors::{ChromaError, ErrorCodes};
use async_trait::async_trait;
use roaring::RoaringBitmap;
use std::{
    collections::HashMap,
    ops::{BitOrAssign, SubAssign},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum MetadataIndexError {
    #[error("Key not found")]
    NotFoundError,
    #[error("This operation cannot be done in a transaction")]
    InTransaction,
    #[error("This operation can only be done in a transaction")]
    NotInTransaction,
}

impl ChromaError for MetadataIndexError {
    fn code(&self) -> ErrorCodes {
        match self {
            MetadataIndexError::NotFoundError => ErrorCodes::InvalidArgument,
            MetadataIndexError::InTransaction => ErrorCodes::InvalidArgument,
            MetadataIndexError::NotInTransaction => ErrorCodes::InvalidArgument,
        }
    }
}

pub(crate) enum MetadataIndexValue {
    String(String),
    Float(f32),
    Bool(bool),
}

pub(crate) trait MetadataIndex {
    fn begin_transaction(&mut self) -> Result<(), Box<dyn ChromaError>>;
    fn commit_transaction(&mut self) -> Result<(), Box<dyn ChromaError>>;

    // Must be in a transaction to put or delete.
    fn set(
        &mut self,
        key: &str,
        value: MetadataIndexValue,
        offset_id: usize,
    ) -> Result<(), Box<dyn ChromaError>>;
    // Can delete anything -- if it's not in committed state the delete will be silently discarded.
    fn delete(
        &mut self,
        key: &str,
        value: MetadataIndexValue,
        offset_id: usize,
    ) -> Result<(), Box<dyn ChromaError>>;

    // Always reads from committed state.
    fn get(
        &self,
        key: &str,
        value: MetadataIndexValue,
    ) -> Result<RoaringBitmap, Box<dyn ChromaError>>;
}

struct BlockfileMetadataIndex {
    blockfile: Box<dyn Blockfile>,
    in_transaction: bool,
    uncommitted_rbms: HashMap<BlockfileKey, RoaringBitmap>,
}

impl BlockfileMetadataIndex {
    pub fn new(init_blockfile: Box<dyn Blockfile>) -> Self {
        BlockfileMetadataIndex {
            blockfile: init_blockfile,
            in_transaction: false,
            uncommitted_rbms: HashMap::new(),
        }
    }

    fn look_up_key_and_populate_uncommitted_rbms(
        &mut self,
        key: &BlockfileKey,
    ) -> Result<(), Box<dyn ChromaError>> {
        if !self.uncommitted_rbms.contains_key(&key) {
            match self.blockfile.get(key.clone()) {
                Ok(Value::RoaringBitmapValue(rbm)) => {
                    self.uncommitted_rbms.insert(key.clone(), rbm);
                }
                _ => {
                    let rbm = RoaringBitmap::new();
                    self.uncommitted_rbms.insert(key.clone(), rbm);
                }
            };
        }
        Ok(())
    }
}

impl MetadataIndex for BlockfileMetadataIndex {
    fn begin_transaction(&mut self) -> Result<(), Box<dyn ChromaError>> {
        if self.in_transaction {
            return Err(Box::new(MetadataIndexError::InTransaction));
        }
        self.blockfile.begin_transaction()?;
        self.in_transaction = true;
        Ok(())
    }

    fn commit_transaction(&mut self) -> Result<(), Box<dyn ChromaError>> {
        if !self.in_transaction {
            return Err(Box::new(MetadataIndexError::NotInTransaction));
        }
        for (key, rbm) in self.uncommitted_rbms.drain() {
            self.blockfile
                .set(key.clone(), Value::RoaringBitmapValue(rbm.clone()));
        }
        self.blockfile.commit_transaction()?;
        self.in_transaction = false;
        self.uncommitted_rbms.clear();
        Ok(())
    }

    fn set(
        &mut self,
        key: &str,
        value: MetadataIndexValue,
        offset_id: usize,
    ) -> Result<(), Box<dyn ChromaError>> {
        if !self.in_transaction {
            return Err(Box::new(MetadataIndexError::NotInTransaction));
        }
        let blockfilekey = kv_to_blockfile_key(key, value);
        self.look_up_key_and_populate_uncommitted_rbms(&blockfilekey)?;
        let mut rbm = self.uncommitted_rbms.get_mut(&blockfilekey).unwrap();
        rbm.insert(offset_id.try_into().unwrap());
        Ok(())
    }

    fn delete(
        &mut self,
        key: &str,
        value: MetadataIndexValue,
        offset_id: usize,
    ) -> Result<(), Box<dyn ChromaError>> {
        if !self.in_transaction {
            return Err(Box::new(MetadataIndexError::NotInTransaction));
        }
        let blockfilekey = kv_to_blockfile_key(key, value);
        self.look_up_key_and_populate_uncommitted_rbms(&blockfilekey)?;
        let mut rbm = self.uncommitted_rbms.get_mut(&blockfilekey).unwrap();
        rbm.remove(offset_id.try_into().unwrap());
        Ok(())
    }

    fn get(
        &self,
        key: &str,
        value: MetadataIndexValue,
    ) -> Result<RoaringBitmap, Box<dyn ChromaError>> {
        if self.in_transaction {
            return Err(Box::new(MetadataIndexError::InTransaction));
        }
        let blockfilekey = kv_to_blockfile_key(key, value);
        match self.blockfile.get(blockfilekey) {
            Ok(Value::RoaringBitmapValue(rbm)) => Ok(rbm),
            _ => Err(Box::new(MetadataIndexError::NotFoundError)),
        }
    }
}

fn kv_to_blockfile_key(key: &str, value: MetadataIndexValue) -> BlockfileKey {
    let blockfilekey_key = match value {
        MetadataIndexValue::String(s) => Key::String(s),
        MetadataIndexValue::Float(f) => Key::Float(f),
        MetadataIndexValue::Bool(b) => Key::Bool(b),
    };
    BlockfileKey::new(key.to_string(), blockfilekey_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockstore::provider::HashMapBlockfileProvider;
    use crate::blockstore::{KeyType, ValueType};

    #[test]
    fn test_string_value_metadata_index_error_when_not_in_transaction() {
        let mut provider = HashMapBlockfileProvider::new();
        let blockfile = provider
            .create("test", KeyType::String, ValueType::RoaringBitmap)
            .unwrap();
        let mut index = BlockfileMetadataIndex::new(blockfile);
        let result = index.set("key", MetadataIndexValue::String("value".to_string()), 1);
        assert_eq!(result.is_err(), true);
        let result = index.delete("key", MetadataIndexValue::String("value".to_string()), 1);
        assert_eq!(result.is_err(), true);
        let result = index.commit_transaction();
        assert_eq!(result.is_err(), true);
    }

    #[test]
    fn test_string_value_metadata_index_empty_transaction() {
        let mut provider = HashMapBlockfileProvider::new();
        let blockfile = provider
            .create("test", KeyType::String, ValueType::RoaringBitmap)
            .unwrap();
        let mut index = BlockfileMetadataIndex::new(blockfile);
        index.begin_transaction().unwrap();
        index.commit_transaction().unwrap();
    }

    #[test]
    fn test_string_value_metadata_index_set_get() {
        let mut provider = HashMapBlockfileProvider::new();
        let blockfile = provider
            .create("test", KeyType::String, ValueType::RoaringBitmap)
            .unwrap();
        let mut index = BlockfileMetadataIndex::new(blockfile);
        index.begin_transaction().unwrap();
        index
            .set("key", MetadataIndexValue::String("value".to_string()), 1)
            .unwrap();
        index.commit_transaction().unwrap();

        let bitmap = index
            .get("key", MetadataIndexValue::String("value".to_string()))
            .unwrap();
        assert_eq!(bitmap.len(), 1);
        assert_eq!(bitmap.contains(1), true);
    }

    #[test]
    fn test_float_value_metadata_index_set_get() {
        let mut provider = HashMapBlockfileProvider::new();
        let blockfile = provider
            .create("test", KeyType::String, ValueType::RoaringBitmap)
            .unwrap();
        let mut index = BlockfileMetadataIndex::new(blockfile);
        index.begin_transaction().unwrap();
        index.set("key", MetadataIndexValue::Float(1.0), 1).unwrap();
        index.commit_transaction().unwrap();

        let bitmap = index.get("key", MetadataIndexValue::Float(1.0)).unwrap();
        assert_eq!(bitmap.len(), 1);
        assert_eq!(bitmap.contains(1), true);
    }

    #[test]
    fn test_bool_value_metadata_index_set_get() {
        let mut provider = HashMapBlockfileProvider::new();
        let blockfile = provider
            .create("test", KeyType::String, ValueType::RoaringBitmap)
            .unwrap();
        let mut index = BlockfileMetadataIndex::new(blockfile);
        index.begin_transaction().unwrap();
        index.set("key", MetadataIndexValue::Bool(true), 1).unwrap();
        index.commit_transaction().unwrap();

        let bitmap = index.get("key", MetadataIndexValue::Bool(true)).unwrap();
        assert_eq!(bitmap.len(), 1);
        assert_eq!(bitmap.contains(1), true);
    }

    #[test]
    fn test_string_value_metadata_index_set_delete_get() {
        let mut provider = HashMapBlockfileProvider::new();
        let blockfile = provider
            .create("test", KeyType::String, ValueType::RoaringBitmap)
            .unwrap();
        let mut index = BlockfileMetadataIndex::new(blockfile);
        index.begin_transaction().unwrap();
        index
            .set("key", MetadataIndexValue::String("value".to_string()), 1)
            .unwrap();
        index
            .delete("key", MetadataIndexValue::String("value".to_string()), 1)
            .unwrap();
        index.commit_transaction().unwrap();

        let bitmap = index
            .get("key", MetadataIndexValue::String("value".to_string()))
            .unwrap();
        assert_eq!(bitmap.len(), 0);
    }

    #[test]
    fn test_string_value_metadata_index_set_delete_set_get() {
        let mut provider = HashMapBlockfileProvider::new();
        let blockfile = provider
            .create("test", KeyType::String, ValueType::RoaringBitmap)
            .unwrap();
        let mut index = BlockfileMetadataIndex::new(blockfile);
        index.begin_transaction().unwrap();
        index
            .set("key", MetadataIndexValue::String("value".to_string()), 1)
            .unwrap();
        index
            .delete("key", MetadataIndexValue::String("value".to_string()), 1)
            .unwrap();
        index
            .set("key", MetadataIndexValue::String("value".to_string()), 1)
            .unwrap();
        index.commit_transaction().unwrap();

        let bitmap = index
            .get("key", MetadataIndexValue::String("value".to_string()))
            .unwrap();
        assert_eq!(bitmap.len(), 1);
        assert_eq!(bitmap.contains(1), true);
    }

    #[test]
    fn test_string_value_metadata_index_multiple_keys() {
        let mut provider = HashMapBlockfileProvider::new();
        let blockfile = provider
            .create("test", KeyType::String, ValueType::RoaringBitmap)
            .unwrap();
        let mut index = BlockfileMetadataIndex::new(blockfile);
        index.begin_transaction().unwrap();
        index
            .set("key1", MetadataIndexValue::String("value".to_string()), 1)
            .unwrap();
        index
            .set("key2", MetadataIndexValue::String("value".to_string()), 2)
            .unwrap();
        index.commit_transaction().unwrap();

        let bitmap = index
            .get("key1", MetadataIndexValue::String("value".to_string()))
            .unwrap();
        assert_eq!(bitmap.len(), 1);
        assert_eq!(bitmap.contains(1), true);

        let bitmap = index
            .get("key2", MetadataIndexValue::String("value".to_string()))
            .unwrap();
        assert_eq!(bitmap.len(), 1);
        assert_eq!(bitmap.contains(2), true);
    }

    #[test]
    fn test_string_value_metadata_index_multiple_values() {
        let mut provider = HashMapBlockfileProvider::new();
        let blockfile = provider
            .create("test", KeyType::String, ValueType::RoaringBitmap)
            .unwrap();
        let mut index = BlockfileMetadataIndex::new(blockfile);
        index.begin_transaction().unwrap();
        index
            .set("key", MetadataIndexValue::String("value1".to_string()), 1)
            .unwrap();
        index
            .set("key", MetadataIndexValue::String("value2".to_string()), 2)
            .unwrap();
        index.commit_transaction().unwrap();

        let bitmap = index
            .get("key", MetadataIndexValue::String("value1".to_string()))
            .unwrap();
        assert_eq!(bitmap.len(), 1);
        assert_eq!(bitmap.contains(1), true);

        let bitmap = index
            .get("key", MetadataIndexValue::String("value2".to_string()))
            .unwrap();
        assert_eq!(bitmap.len(), 1);
        assert_eq!(bitmap.contains(2), true);
    }

    #[test]
    fn test_string_value_metadata_index_delete_in_standalone_transaction() {
        let mut provider = HashMapBlockfileProvider::new();
        let blockfile = provider
            .create("test", KeyType::String, ValueType::RoaringBitmap)
            .unwrap();
        let mut index = BlockfileMetadataIndex::new(blockfile);
        index.begin_transaction().unwrap();
        index
            .set("key", MetadataIndexValue::String("value".to_string()), 1)
            .unwrap();
        index.commit_transaction().unwrap();

        index.begin_transaction().unwrap();
        index
            .delete("key", MetadataIndexValue::String("value".to_string()), 1)
            .unwrap();
        index.commit_transaction().unwrap();

        let bitmap = index
            .get("key", MetadataIndexValue::String("value".to_string()))
            .unwrap();
        assert_eq!(bitmap.len(), 0);
    }
}
