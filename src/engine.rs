use std::{num::NonZeroUsize, path::PathBuf};

use lru::LruCache;
use tempfile::TempDir;

use crate::{
    datafile::{DATAFILE_HEADER_SIZE_BYTES, Datafile, DatafileLog},
    error::BitcaskError,
    keydir::KeyDir,
};

pub const LRU_CACHED_READERS_COUNT: usize = 10;

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
        self.active_file.demote_to_reader();
        let old_writer = &mut self.active_file;
        let new_writer = Datafile::new(self.dir_path.clone(), old_writer.id + 1)?;

        let old_writer = std::mem::replace(old_writer, new_writer);

        self.reader_files.put(old_writer.id, old_writer);

        Ok(())
    }

    pub fn build_mock_bitcask_instance() -> (BitcaskInstance, TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let dir_path = PathBuf::from(temp_dir.path());

        let writer = Datafile::new(dir_path.clone(), 2).unwrap();
        let mut reader = Datafile::new(dir_path.clone(), 1).unwrap();
        reader.is_writer = false;

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
        datafile::{DATAFILE_HEADER_SIZE_BYTES, DATAFILE_MAX_SIZE_BYTES, Datafile, DatafileLog},
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

    #[test]
    fn switch_writer_datafile() {
        let (mut db, _dir) = BitcaskInstance::build_mock_bitcask_instance();

        assert_eq!(db.active_file.id, 2);
        assert!(db.active_file.is_writer);
        assert_eq!(db.active_file.offset, 0);
        assert_eq!(db.reader_files.len(), 1);

        assert!(db.switch_writer_datafile().is_ok());

        // Make sure old writer is demoted and added to LRU cache of readers
        assert_eq!(db.reader_files.len(), 2);
        let old = db.reader_files.get(&2u32).unwrap();
        assert_eq!(old.id, 2);
        assert!(!old.is_writer);

        // Make sure new writer is created with proper id, role and offset
        assert_eq!(db.active_file.id, 3);
        assert!(db.active_file.is_writer);
        assert_eq!(db.active_file.offset, 0);

        // Make sure the new datafile accepts writes
        let key = "hello world";
        let value: u32 = 2;

        let datafile_entry = datafile_entry_for_string_u32(key, value);
        assert!(db.active_file.write_log(&datafile_entry).is_ok());
        assert_eq!(db.active_file.offset, datafile_entry.size().unwrap() as u64);
    }

    #[test]
    fn put_switch_writer() {
        let (mut db, _dir) = BitcaskInstance::build_mock_bitcask_instance();
        db.active_file.offset = DATAFILE_MAX_SIZE_BYTES as u64;

        assert_eq!(db.active_file.id, 2);
        assert!(db.active_file.is_writer);
        assert_eq!(db.active_file.offset, DATAFILE_MAX_SIZE_BYTES as u64);

        let key = "hello world";
        let value: u32 = 2;

        assert!(db.put(key.as_bytes(), &value.to_be_bytes()).is_ok());

        // Make sure new writer is created with proper id, role and offset
        assert_eq!(db.active_file.id, 3);
        assert!(db.active_file.is_writer);
        assert_eq!(
            db.active_file.offset,
            datafile_entry_for_string_u32(key, value).size().unwrap() as u64
        );
    }
}
