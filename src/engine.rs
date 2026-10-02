use std::{
    fs::File,
    io::BufWriter,
    path::{Path, PathBuf},
};

use lru::LruCache;

use crate::{
    datafile::{DATAFILE_HEADER_SIZE_BYTES, DatafileEntry, DatafileError},
    keydir::KeyDir,
};

const LRU_CACHED_READERS_COUNT: usize = 10;
const DATAFILE_MAX_SIZE_BYTES: usize = 10_000_000;

pub enum BitcaskError {
    DatafileError(DatafileError),

    WriterFileOffsetOverflow,
}

impl From<DatafileError> for BitcaskError {
    fn from(value: DatafileError) -> Self {
        Self::DatafileError(value)
    }
}

pub struct BitcaskInstance {
    keydir: KeyDir,                    // In-memory keydir map
    active_file: BufWriter<File>,      // Writer file handle
    active_file_id: u32,               // Writer file id
    active_file_offset: u64,           // Writer file offset
    dir_path: PathBuf,                 // For opening files through LRU
    reader_files: LruCache<u32, File>, // Use LRU cache to track recently used files
}

impl BitcaskInstance {
    pub fn open(dir_path: PathBuf) -> Result<Self, BitcaskError> {
        todo!()
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), BitcaskError> {
        let entry = DatafileEntry::new(key, value)?;
        let entry_size = entry.size()?;

        if self.active_file_offset as usize + entry_size >= DATAFILE_HEADER_SIZE_BYTES {
            self.switch_writer_datafile()?;
            return Err(BitcaskError::DatafileError(DatafileError::DatafileFull));
        }

        self.commit_datafile_write(&entry)?; // Write log first, so that it can be
        // replayed in case of a crash.

        self.commit_keydir_write(entry)?; // Append entry to the keydir
        self.move_writer_offset(entry_size)?; // Move the writer offset to suit the new entry

        Ok(())
    }

    fn commit_keydir_write(&mut self, datafile: DatafileEntry) -> Result<(), BitcaskError> {
        let ksz = datafile.header.key_size;

        self.keydir.put(
            datafile,
            self.active_file_id,
            DATAFILE_HEADER_SIZE_BYTES as u64 + ksz as u64 + self.active_file_offset,
        );

        Ok(())
    }

    fn move_writer_offset(&mut self, entry_size: usize) -> Result<(), BitcaskError> {
        self.active_file_offset = self
            .active_file_offset
            .checked_add(entry_size as u64)
            .ok_or(BitcaskError::WriterFileOffsetOverflow)?;

        Ok(())
    }

    fn commit_datafile_write(&mut self, datafile: &DatafileEntry) -> Result<(), BitcaskError> {
        datafile
            .write_to(&mut self.active_file)
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
        datafile::{DATAFILE_HEADER_SIZE_BYTES, DatafileEntry},
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

    pub fn keydir_entry_for(datafile_entry: &DatafileEntry, db: &BitcaskInstance) -> KeyDirEntry {
        KeyDirEntry::new(
            db.active_file_id,
            datafile_entry.header.value_size,
            DATAFILE_HEADER_SIZE_BYTES as u64
                + datafile_entry.header.key_size as u64
                + db.active_file_offset,
            datafile_entry.header.timestamp,
        )
    }

    pub fn datafile_entry_for_string_string(key: &str, value: &str) -> DatafileEntry {
        DatafileEntry::new(key.as_bytes(), value.as_bytes()).unwrap()
    }

    pub fn datafile_entry_for_u32_string(key: u32, value: &str) -> DatafileEntry {
        DatafileEntry::new(&key.to_be_bytes(), value.as_bytes()).unwrap()
    }

    pub fn datafile_entry_for_string_u32(key: &str, value: u32) -> DatafileEntry {
        DatafileEntry::new(key.as_bytes(), &value.to_be_bytes()).unwrap()
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
}
