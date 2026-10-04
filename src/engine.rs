use std::{
    fs::read_dir,
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use lru::LruCache;
use tempfile::TempDir;

use crate::{
    datafile::{DATAFILE_EXTENSION, DATAFILE_HEADER_SIZE_BYTES, Datafile, DatafileLog},
    error::BitcaskError,
    keydir::KeyDir,
    options::{BitcaskOption, BitcaskOptions},
};

pub const LRU_CACHED_READERS_COUNT: usize = 10;
pub struct BitcaskInstance {
    pub(crate) keydir: KeyDir,            // In-memory keydir map
    pub writer: Datafile,                 // Writer file handle
    dir_path: PathBuf,                    // For opening files through LRU
    pub readers: LruCache<u32, Datafile>, // Use LRU cache to track recently used files
    options: BitcaskOptions,              // User-customisable options
}

pub struct BitcaskStartupHandler {}

impl BitcaskStartupHandler {
    pub fn parse_db_datafile_ids_at_path(
        dir: &Path,
        options: &BitcaskOptions,
    ) -> Result<Vec<u32>, BitcaskError> {
        if !dir.exists() {
            std::fs::create_dir_all(dir)?;
        }

        if !dir.is_dir() {
            return Err(BitcaskError::PathIsFile);
        }

        let mut ids: Vec<u32> = Vec::new();
        for file in read_dir(dir)? {
            let filename = file?
                .file_name()
                .into_string()
                .map_err(|_| BitcaskError::FilenameConversion)?;

            if let Some((name, ext)) = filename.rsplit_once('.') {
                if ext == DATAFILE_EXTENSION {
                    ids.push(name.parse::<u32>()?);
                }
            } else {
                continue;
            }
        }
        ids.sort();

        Ok(ids)
    }

    // TODO: Return last reader insted
    pub fn replay_and_init_readers(
        dir: &Path,
        ids: Vec<u32>,
    ) -> Result<(KeyDir, LruCache<u32, Datafile>, u32), BitcaskError> {
        let mut keydir = KeyDir::new();
        let mut readers: LruCache<u32, Datafile> =
            LruCache::new(NonZeroUsize::new(LRU_CACHED_READERS_COUNT).unwrap());

        let mut highest_id = 0u32;
        for id in ids {
            let mut df = Datafile::open_reader(dir, id)?;
            df.replay(&mut keydir)?;

            // TODO: Reuse old writer option

            readers.put(id, df);
            highest_id = highest_id.max(id);
        }

        Ok((keydir, readers, highest_id))
    }

    pub fn create_writer_with_options(
        dir: &Path,
        highest_reader_id: u32,
        options: &BitcaskOptions,
    ) -> Result<Datafile, BitcaskError> {
        if options.is_enabled(BitcaskOption::ReuseExistingNonFullWriterOnStartup) {
            todo!()
        } else {
            Ok(Datafile::new_writer(dir, highest_reader_id + 1)?)
        }
    }
}

impl BitcaskInstance {
    pub fn open(dir: PathBuf, options: BitcaskOptions) -> Result<Self, BitcaskError> {
        let (keydir, readers, highest_reader_id) = BitcaskStartupHandler::replay_and_init_readers(
            &dir,
            BitcaskStartupHandler::parse_db_datafile_ids_at_path(&dir, &options)?,
        )?;

        Ok(BitcaskInstance {
            keydir,
            writer: BitcaskStartupHandler::create_writer_with_options(
                &dir,
                highest_reader_id,
                &options,
            )?,
            dir_path: dir,
            readers,
            options,
        })
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), BitcaskError> {
        let entry = DatafileLog::new(key, value)?;

        if self
            .writer
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
            let reader = if self.writer.id == keydir_entry.file_id {
                &self.writer
            } else if let Some(reader) = self.readers.get(&keydir_entry.file_id) {
                reader
            } else if let Ok(reader) = Datafile::open_reader(&self.dir_path, keydir_entry.file_id) {
                let reader_id = reader.id;
                self.readers.put(reader_id, reader);
                self.readers
                    .get(&reader_id)
                    .ok_or(BitcaskError::DatafileMissing(reader_id as usize))?
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
            self.writer.id,
            DATAFILE_HEADER_SIZE_BYTES as u64 + ksz as u64 + offset,
        );

        Ok(())
    }

    fn commit_datafile_write(&mut self, log: &DatafileLog) -> Result<u64, BitcaskError> {
        self.writer
            .append_log(log)
            .map_err(BitcaskError::DatafileError)
    }

    fn switch_writer_datafile(&mut self) -> Result<(), BitcaskError> {
        self.writer.demote_to_reader();
        let old_writer = &mut self.writer;
        let new_writer = Datafile::new_writer(&self.dir_path, old_writer.id + 1)?;

        let old_writer = std::mem::replace(old_writer, new_writer);

        self.readers.put(old_writer.id, old_writer);

        Ok(())
    }

    pub fn build_mock_bitcask_instance() -> (BitcaskInstance, TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let dir_path = temp_dir.path();

        let writer = Datafile::new_writer(dir_path, 2).unwrap();
        let mut reader = Datafile::new_writer(dir_path, 1).unwrap();
        reader.is_writer = false;

        let mut readers: LruCache<u32, Datafile> =
            LruCache::new(NonZeroUsize::new(LRU_CACHED_READERS_COUNT).unwrap());

        readers.put(1, reader);

        (
            BitcaskInstance {
                keydir: KeyDir::new(),
                writer,
                dir_path: dir_path.to_path_buf(),
                readers,
                options: BitcaskOptions::default(),
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
        let expected = keydir_entry_for(&datafile_entry, db.writer.id, 0);

        assert!(db.commit_keydir_write(datafile_entry.clone(), 0).is_ok());
        assert_eq!(db.keydir.size(), 1);
        assert_eq!(db.keydir.get(&datafile_entry.key).unwrap(), &expected);

        let datafile_entry = datafile_entry_for_string_u32("byeworld", 2);
        let expected = keydir_entry_for(&datafile_entry, db.writer.id, 35);

        assert!(db.commit_keydir_write(datafile_entry.clone(), 35).is_ok());
        assert_eq!(db.keydir.size(), 2);
        assert_eq!(db.keydir.get(&datafile_entry.key).unwrap(), &expected);

        let datafile_entry = datafile_entry_for_string_u32("byeworld", 3);
        let expected = keydir_entry_for(&datafile_entry, db.writer.id, 70);

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
        let expected = keydir_entry_for(&datafile_entry, db.writer.id, 0);

        assert!(db.put(key.as_bytes(), &value.to_be_bytes()).is_ok());

        let mut expected_bytes = Vec::new();
        Datafile::write_log_to(&mut expected_bytes, &datafile_entry).unwrap();
        let read_bytes = db
            .writer
            .read_bytes(0, datafile_entry.size().unwrap())
            .unwrap();
        assert_eq!(&*read_bytes, &expected_bytes[..]);

        let keydir_written = db.keydir.get(key.as_bytes()).unwrap();
        assert_eq!(db.keydir.size(), 1);
        assert_eq!(keydir_written.file_id, expected.file_id);
        assert_eq!(keydir_written.value_size, expected.value_size);
        assert_eq!(keydir_written.value_offset, expected.value_offset);

        assert_eq!(db.writer.offset, datafile_entry.size().unwrap() as u64);
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

        assert_eq!(db.writer.id, 2);
        assert!(db.writer.is_writer);
        assert_eq!(db.writer.offset, 0);
        assert_eq!(db.readers.len(), 1);

        assert!(db.switch_writer_datafile().is_ok());

        // Make sure old writer is demoted and added to LRU cache of readers
        assert_eq!(db.readers.len(), 2);
        let old = db.readers.get(&2u32).unwrap();
        assert_eq!(old.id, 2);
        assert!(!old.is_writer);

        // Make sure new writer is created with proper id, role and offset
        assert_eq!(db.writer.id, 3);
        assert!(db.writer.is_writer);
        assert_eq!(db.writer.offset, 0);

        // Make sure the new datafile accepts writes
        let key = "hello world";
        let value: u32 = 2;

        let datafile_entry = datafile_entry_for_string_u32(key, value);
        assert!(db.writer.append_log(&datafile_entry).is_ok());
        assert_eq!(db.writer.offset, datafile_entry.size().unwrap() as u64);
    }

    #[test]
    fn put_switch_writer() {
        let (mut db, _dir) = BitcaskInstance::build_mock_bitcask_instance();
        db.writer.offset = DATAFILE_MAX_SIZE_BYTES as u64;

        assert_eq!(db.writer.id, 2);
        assert!(db.writer.is_writer);
        assert_eq!(db.writer.offset, DATAFILE_MAX_SIZE_BYTES as u64);

        let key = "hello world";
        let value: u32 = 2;

        assert!(db.put(key.as_bytes(), &value.to_be_bytes()).is_ok());

        // Make sure new writer is created with proper id, role and offset
        assert_eq!(db.writer.id, 3);
        assert!(db.writer.is_writer);
        assert_eq!(
            db.writer.offset,
            datafile_entry_for_string_u32(key, value).size().unwrap() as u64
        );

        // Verify the new writer is properly handled on get syncpath
        assert_eq!(
            db.get(key.as_bytes()).unwrap().unwrap(),
            Box::from(value.to_be_bytes())
        )
    }

    #[test]
    fn get_ok_lru_cache_miss() {
        let (mut db, _dir) = BitcaskInstance::build_mock_bitcask_instance();

        let key = "hello world";
        let value = 128u32;

        let dfl = datafile_entry_for_string_u32(key, value);

        // Prepare reader datafile with the requested entry
        let mut missed_reader = Datafile::new_writer(&db.dir_path, 3).unwrap();
        assert!(missed_reader.append_log(&dfl).is_ok());
        missed_reader.is_writer = false;

        // Prepare KeyDir to store a mapping of the key to the file that isn't present in LRU
        db.keydir.put(dfl, 3, 31);

        // Make sure the datafile is not known
        assert_eq!(db.readers.len(), 1);

        assert_eq!(
            db.get(key.as_bytes()).unwrap().unwrap(),
            Box::from(value.to_be_bytes())
        );
        // Verify the missed reader has been discovered
        assert_eq!(db.readers.len(), 2);
    }
}
