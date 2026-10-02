use std::{
    fs::File,
    io::{BufWriter, Error},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};

use lru::LruCache;

use crate::{
    datafile::{DATAFILE_HEADER_SIZE_BYTES, Datafile, DatafileError, DatafileLog},
    keydir::KeyDir,
};

const LRU_CACHED_READERS_COUNT: usize = 10;

#[derive(Debug)]
pub enum BitcaskError {
    DatafileError(DatafileError),

    DatafileMissing(usize),
    IO(Error),
}

impl From<Error> for BitcaskError {
    fn from(value: Error) -> Self {
        BitcaskError::IO(value)
    }
}

impl From<DatafileError> for BitcaskError {
    fn from(value: DatafileError) -> Self {
        Self::DatafileError(value)
    }
}

pub struct BitcaskInstance {
    keydir: KeyDir,                        // In-memory keydir map
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

        self.commit_datafile_write(&entry)?; // Write log first, so that it can be
        // replayed in case of a crash.

        self.commit_keydir_write(entry)?; // Append entry to the keydir

        Ok(())
    }

    pub fn get(&mut self, key: &[u8]) -> Result<Option<Box<[u8]>>, BitcaskError> {
        if let Some(keydir_entry) = self.keydir.get(key) {
            if let Some(reader) = self.reader_files.get(&keydir_entry.file_id) {
                Ok(Some(
                    reader
                        .read_bytes(keydir_entry.value_offset, keydir_entry.value_size as usize)
                        .map_err(BitcaskError::DatafileError)?,
                ))
            } else {
                Err(BitcaskError::DatafileMissing(keydir_entry.file_id as usize))
            }
        } else {
            Ok(None)
        }
    }

    fn commit_keydir_write(&mut self, log: DatafileLog) -> Result<(), BitcaskError> {
        let ksz = log.header.key_size;

        self.keydir.put(
            log,
            self.active_file.id,
            DATAFILE_HEADER_SIZE_BYTES as u64 + ksz as u64 + self.active_file.offset,
        );

        Ok(())
    }

    fn commit_datafile_write(&mut self, log: &DatafileLog) -> Result<(), BitcaskError> {
        self.active_file
            .write_log(log)
            .map_err(BitcaskError::DatafileError)
    }

    fn switch_writer_datafile(&mut self) -> Result<(), BitcaskError> {
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::{fs::File, io::BufWriter, num::NonZeroUsize, path::PathBuf};

    use lru::LruCache;
    use tempfile::TempDir;

    use crate::{
        datafile::{DATAFILE_HEADER_SIZE_BYTES, DatafileLog},
        engine::{BitcaskInstance, LRU_CACHED_READERS_COUNT, test},
        keydir::{KeyDir, KeyDirEntry},
    };

    pub fn build_mock_bitcask_instance() -> (BitcaskInstance, TempDir) {
        let dir_path = tempfile::tempdir().unwrap();
        let writer = tempfile::tempfile_in(&dir_path).unwrap();
        let reader = tempfile::tempfile_in(&dir_path).unwrap();

        let mut readers: LruCache<u32, File> =
            LruCache::new(NonZeroUsize::new(LRU_CACHED_READERS_COUNT).unwrap());

        readers.put(1, reader);

        (
            BitcaskInstance {
                keydir: KeyDir::new(),
                active_file: BufWriter::new(writer),
                active_file_id: 2,
                active_file_offset: 0,
                dir_path: PathBuf::from(dir_path.path()),
                reader_files: readers,
            },
            dir_path,
        )
    }

    pub fn keydir_entry_for(datafile_entry: &DatafileLog, db: &BitcaskInstance) -> KeyDirEntry {
        KeyDirEntry::new(
            db.active_file_id,
            datafile_entry.header.value_size,
            DATAFILE_HEADER_SIZE_BYTES as u64
                + datafile_entry.header.key_size as u64
                + db.active_file_offset,
            datafile_entry.header.timestamp,
        )
    }

    pub fn datafile_entry_for_string_string(key: &str, value: &str) -> DatafileLog {
        DatafileLog::new(key.as_bytes(), value.as_bytes()).unwrap()
    }

    pub fn datafile_entry_for_u32_string(key: u32, value: &str) -> DatafileLog {
        DatafileLog::new(&key.to_be_bytes(), value.as_bytes()).unwrap()
    }

    pub fn datafile_entry_for_string_u32(key: &str, value: u32) -> DatafileLog {
        DatafileLog::new(key.as_bytes(), &value.to_be_bytes()).unwrap()
    }

    #[test]
    fn commit_datafile_write() {
        let (mut db, _dir) = build_mock_bitcask_instance();

        let datafile_entry = datafile_entry_for_string_u32("hello world", 2);

        let mut writer: Vec<u8> = Vec::new();
        let _ = datafile_entry.write_to(&mut writer);

        assert!(db.commit_datafile_write(&datafile_entry).is_ok());
        assert_eq!(db.active_file.buffer(), &writer);

        let _ = datafile_entry.write_to(&mut writer);
        assert!(db.commit_datafile_write(&datafile_entry).is_ok());
        assert_eq!(db.active_file.buffer(), &writer);
    }

    #[test]
    fn commit_keydir_write() {
        let (mut db, _dir) = build_mock_bitcask_instance();

        let datafile_entry = datafile_entry_for_string_u32("hello world", 2);
        let expected = keydir_entry_for(&datafile_entry, &db);

        assert!(db.commit_keydir_write(datafile_entry.clone()).is_ok());
        assert_eq!(db.keydir.size(), 1);
        assert_eq!(db.keydir.get(&datafile_entry.key).unwrap(), &expected);

        let datafile_entry = datafile_entry_for_string_u32("byeworld", 2);
        let expected = keydir_entry_for(&datafile_entry, &db);

        assert!(db.commit_keydir_write(datafile_entry.clone()).is_ok());
        assert_eq!(db.keydir.size(), 2);
        assert_eq!(db.keydir.get(&datafile_entry.key).unwrap(), &expected);

        let datafile_entry = datafile_entry_for_string_u32("byeworld", 3);
        let expected = keydir_entry_for(&datafile_entry, &db);

        assert!(db.commit_keydir_write(datafile_entry.clone()).is_ok());
        assert_ne!(db.keydir.size(), 3);
        assert_eq!(db.keydir.get(&datafile_entry.key).unwrap(), &expected);
    }

    #[test]
    fn move_writer_offset() {
        let (mut db, _dir) = build_mock_bitcask_instance();

        let datafile_entry = datafile_entry_for_string_u32("hello world", 2);

        assert_eq!(db.active_file_offset, 0);
        assert!(
            db.move_writer_offset(datafile_entry.size().unwrap())
                .is_ok()
        );
        assert_eq!(db.active_file_offset, datafile_entry.size().unwrap() as u64);
    }

    #[test]
    fn put_ok() {
        let (mut db, _dir) = build_mock_bitcask_instance();

        let key = "hello world";
        let value: u32 = 2;

        let datafile_entry = datafile_entry_for_string_u32(key, value);
        let expected = keydir_entry_for(&datafile_entry, &db);

        assert!(db.put(key.as_bytes(), &value.to_be_bytes()).is_ok());

        // Persistent Write
        let mut writer: Vec<u8> = Vec::new();
        let _ = datafile_entry.write_to(&mut writer);
        assert_eq!(db.active_file.buffer(), &writer);

        // KeyDir Write
        let keydir_written = db.keydir.get(key.as_bytes()).unwrap();
        assert_eq!(db.keydir.size(), 1);
        assert_eq!(keydir_written.file_id, expected.file_id);
        assert_eq!(keydir_written.value_size, expected.value_size);
        assert_eq!(keydir_written.value_offset, expected.value_offset);

        // Offset Move
        assert_eq!(db.active_file_offset, datafile_entry.size().unwrap() as u64);
    }

    #[test]
    fn get_ok() {
        let (mut db, _dir) = build_mock_bitcask_instance();

        let key = "hello world";
        let value: u32 = 2;

        assert!(db.put(key.as_bytes(), &value.to_be_bytes()).is_ok());

        assert_eq!(
            db.get(key.as_bytes()).unwrap().unwrap(),
            Box::from(value.to_be_bytes())
        );
    }
}
