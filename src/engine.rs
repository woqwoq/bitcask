use std::{num::NonZeroUsize, path::PathBuf};

use lru::LruCache;
use tempfile::TempDir;

use crate::{
    datafile::{DATAFILE_HEADER_SIZE_BYTES, Datafile, DatafileError, DatafileLog},
    keydir::KeyDir,
};

pub const LRU_CACHED_READERS_COUNT: usize = 10;

#[derive(Debug)]
pub enum BitcaskError {
    DatafileError(DatafileError),

    DatafileMissing(usize),
}

impl From<DatafileError> for BitcaskError {
    fn from(value: DatafileError) -> Self {
        Self::DatafileError(value)
    }
}

pub struct BitcaskInstance {
    pub(crate) keydir: KeyDir,             // In-memory keydir map
    active_file: Datafile,                 // Writer file handle
    dir_path: PathBuf,                     // For opening files through LRU
    reader_files: LruCache<u32, Datafile>, // Use LRU cache to track recently used files
}

impl BitcaskInstance {
    pub fn open(dir_path: PathBuf) -> Result<Self, BitcaskError> {
        todo!()
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), BitcaskError> {
        let entry = DatafileLog::new(key, value)?;

        if self
            .active_file
            .should_switch_writer_before_write_commit(&entry)?
        {
            self.switch_writer_datafile()?;
        }

        let offset = self.commit_datafile_write(&entry)?; // Write log first, so that it can be
        // replayed in case of a crash.

        self.commit_keydir_write(entry, offset)?; // Append entry to the keydir

        Ok(())
    }

    pub fn get(&mut self, key: &[u8]) -> Result<Option<Box<[u8]>>, BitcaskError> {
        if let Some(keydir_entry) = self.keydir.get(key) {
            let reader = if self.active_file.id == keydir_entry.file_id {
                &self.active_file
            } else if let Some(reader) = self.reader_files.get(&keydir_entry.file_id) {
                reader
            } else {
                return Err(BitcaskError::DatafileMissing(keydir_entry.file_id as usize));
            };

            Ok(Some(
                reader
                    .read_bytes(keydir_entry.value_offset, keydir_entry.value_size as usize)
                    .map_err(BitcaskError::DatafileError)?,
            ))
        } else {
            Ok(None)
        }
    }

    fn commit_keydir_write(&mut self, log: DatafileLog, offset: u64) -> Result<(), BitcaskError> {
        let ksz = log.header.key_size;

        self.keydir.put(
            log,
            self.active_file.id,
            DATAFILE_HEADER_SIZE_BYTES as u64 + ksz as u64 + offset,
        );

        Ok(())
    }

    fn commit_datafile_write(&mut self, log: &DatafileLog) -> Result<u64, BitcaskError> {
        self.active_file
            .write_log(log)
            .map_err(BitcaskError::DatafileError)
    }

    fn switch_writer_datafile(&mut self) -> Result<(), BitcaskError> {
        Ok(())
    }

    pub fn build_mock_bitcask_instance() -> (BitcaskInstance, TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let dir_path = PathBuf::from(temp_dir.path());

        let writer = Datafile::open_from_file(
            dir_path.clone(),
            tempfile::tempfile_in(&dir_path).unwrap(),
            2,
        )
        .unwrap();

        let reader = Datafile::open_from_file(
            dir_path.clone(),
            tempfile::tempfile_in(&dir_path).unwrap(),
            1,
        )
        .unwrap();

        let mut readers: LruCache<u32, Datafile> =
            LruCache::new(NonZeroUsize::new(LRU_CACHED_READERS_COUNT).unwrap());

        readers.put(1, reader);

        (
            BitcaskInstance {
                keydir: KeyDir::new(),
                active_file: writer,
                dir_path: dir_path.clone(),
                reader_files: readers,
            },
            temp_dir,
        )
    }
}

#[cfg(test)]
mod test {
    use crate::{
        datafile::{DATAFILE_HEADER_SIZE_BYTES, Datafile, DatafileLog},
        engine::BitcaskInstance,
        keydir::KeyDirEntry,
    };

    pub fn keydir_entry_for(
        datafile_entry: &DatafileLog,
        file_id: u32,
        offset: u64,
    ) -> KeyDirEntry {
        KeyDirEntry::new(
            file_id,
            datafile_entry.header.value_size,
            DATAFILE_HEADER_SIZE_BYTES as u64 + datafile_entry.header.key_size as u64 + offset,
            datafile_entry.header.timestamp,
        )
    }

    pub fn datafile_entry_for_string_u32(key: &str, value: u32) -> DatafileLog {
        DatafileLog::new(key.as_bytes(), &value.to_be_bytes()).unwrap()
    }

    #[test]
    fn commit_keydir_write() {
        let (mut db, _dir) = BitcaskInstance::build_mock_bitcask_instance();

        let datafile_entry = datafile_entry_for_string_u32("hello world", 2);
        let expected = keydir_entry_for(&datafile_entry, db.active_file.id, 0);

        assert!(db.commit_keydir_write(datafile_entry.clone(), 0).is_ok());
        assert_eq!(db.keydir.size(), 1);
        assert_eq!(db.keydir.get(&datafile_entry.key).unwrap(), &expected);

        let datafile_entry = datafile_entry_for_string_u32("byeworld", 2);
        let expected = keydir_entry_for(&datafile_entry, db.active_file.id, 35);

        assert!(db.commit_keydir_write(datafile_entry.clone(), 35).is_ok());
        assert_eq!(db.keydir.size(), 2);
        assert_eq!(db.keydir.get(&datafile_entry.key).unwrap(), &expected);

        let datafile_entry = datafile_entry_for_string_u32("byeworld", 3);
        let expected = keydir_entry_for(&datafile_entry, db.active_file.id, 70);

        assert!(db.commit_keydir_write(datafile_entry.clone(), 70).is_ok());
        assert_ne!(db.keydir.size(), 3);
        assert_eq!(db.keydir.get(&datafile_entry.key).unwrap(), &expected);
    }

    #[test]
    fn put_ok() {
        let (mut db, _dir) = BitcaskInstance::build_mock_bitcask_instance();

        let key = "hello world";
        let value: u32 = 2;

        let datafile_entry = datafile_entry_for_string_u32(key, value);
        let expected = keydir_entry_for(&datafile_entry, db.active_file.id, 0);

        assert!(db.put(key.as_bytes(), &value.to_be_bytes()).is_ok());

        let mut expected_bytes = Vec::new();
        Datafile::write_log_to(&mut expected_bytes, &datafile_entry).unwrap();
        let read_bytes = db
            .active_file
            .read_bytes(0, datafile_entry.size().unwrap())
            .unwrap();
        assert_eq!(&*read_bytes, &expected_bytes[..]);

        let keydir_written = db.keydir.get(key.as_bytes()).unwrap();
        assert_eq!(db.keydir.size(), 1);
        assert_eq!(keydir_written.file_id, expected.file_id);
        assert_eq!(keydir_written.value_size, expected.value_size);
        assert_eq!(keydir_written.value_offset, expected.value_offset);

        assert_eq!(db.active_file.offset, datafile_entry.size().unwrap() as u64);
    }

    #[test]
    fn get_ok() {
        let (mut db, _dir) = BitcaskInstance::build_mock_bitcask_instance();

        let key = "hello world";
        let value: u32 = 2;

        assert!(db.put(key.as_bytes(), &value.to_be_bytes()).is_ok());

        assert_eq!(
            db.get(key.as_bytes()).unwrap().unwrap(),
            Box::from(value.to_be_bytes())
        );

        let value2 = "bye world";
        assert!(db.put(key.as_bytes(), value2.as_bytes()).is_ok());
        assert_eq!(
            db.get(key.as_bytes()).unwrap().unwrap(),
            Box::from(value2.as_bytes())
        );
    }
}
