use std::{
    fs::read_dir,
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use lru::LruCache;
use tempfile::TempDir;

use crate::{
    datafile::{
        DATAFILE_EXTENSION, DATAFILE_HEADER_SIZE_BYTES, DATAFILE_RECORD_TYPE_NORMAL,
        DATAFILE_RECORD_TYPE_TOMBSTONE, Datafile, DatafileRecord,
    },
    error::{BitcaskError, DatafileError},
    keydir::KeyDir,
    options::{BitcaskOptions, DatafileMaxSize},
};

pub const LRU_CACHED_READERS_COUNT: usize = 10;
pub struct BitcaskInstance {
    dir_path: PathBuf,                    // For opening files through LRU
    pub(crate) keydir: KeyDir,            // In-memory keydir map
    pub writer: Datafile,                 // Writer file handle
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
        if options.reuse_old_writer_on_startup {
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
        self.write_record(DatafileRecord::new(key, value)?)
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

    fn write_record(&mut self, record: DatafileRecord) -> Result<(), BitcaskError> {
        let rsz = record.size()?;

        // If our record is larger than the writer max size, we either throw an
        // error or let it happen and continue
        if rsz > self.options.get_datafile_max_size()
            && matches!(
                self.options.datafile_max_size,
                DatafileMaxSize::HardCappedBytes(_)
            )
        {
            return Err(BitcaskError::RecordLargerThanMaxDatafileSize(rsz));
        }

        if self
            .writer
            .should_switch_writer_before_write_commit(rsz as u64, &self.options)?
        {
            self.switch_writer_datafile()?;
        }

        let offset = self.commit_datafile_write(&record)?; // Write record first, so that it can be
        // replayed in case of a crash.

        self.commit_keydir_write(record, offset)?; // Append entry to the keydir

        Ok(())
    }

    pub fn remove(&mut self, key: &[u8]) -> Result<(), BitcaskError> {
        if self.keydir.contains(key) {
            self.write_record(DatafileRecord::new_tombstone(key)?)?;
        }

        Ok(())
    }

    fn commit_keydir_write(
        &mut self,
        record: DatafileRecord,
        offset: u64,
    ) -> Result<(), BitcaskError> {
        let ksz = record.header.key_size;

        match record.header.record_type {
            DATAFILE_RECORD_TYPE_NORMAL => {
                self.keydir.put(
                    record,
                    self.writer.id,
                    offset + DATAFILE_HEADER_SIZE_BYTES as u64 + ksz as u64,
                );
            }
            DATAFILE_RECORD_TYPE_TOMBSTONE => {
                self.keydir.remove(&record.key);
            }
            _ => return Err(DatafileError::UnexpectedDatafileRecordType)?,
        }

        Ok(())
    }

    fn commit_datafile_write(&mut self, record: &DatafileRecord) -> Result<u64, BitcaskError> {
        self.writer
            .append_record(record)
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
        datafile::{DATAFILE_HEADER_SIZE_BYTES, Datafile, DatafileRecord},
        engine::BitcaskInstance,
        error::BitcaskError,
        keydir::KeyDirEntry,
        options::BitcaskOptions,
    };

    pub fn keydir_entry_for(
        datafile_record: &DatafileRecord,
        file_id: u32,
        offset: u64,
    ) -> KeyDirEntry {
        KeyDirEntry::new(
            file_id,
            datafile_record.header.value_size,
            DATAFILE_HEADER_SIZE_BYTES as u64 + datafile_record.header.key_size as u64 + offset,
            datafile_record.header.timestamp,
        )
    }

    pub fn datafile_record_for_string_u32(key: &str, value: u32) -> DatafileRecord {
        DatafileRecord::new(key.as_bytes(), &value.to_be_bytes()).unwrap()
    }

    #[test]
    fn commit_keydir_write() {
        let (mut db, _dir) = BitcaskInstance::build_mock_bitcask_instance();

        let datafile_record = datafile_record_for_string_u32("hello world", 2);
        let expected = keydir_entry_for(&datafile_record, db.writer.id, 0);

        assert!(db.commit_keydir_write(datafile_record.clone(), 0).is_ok());
        assert_eq!(db.keydir.len(), 1);
        assert_eq!(db.keydir.get(&datafile_record.key).unwrap(), &expected);

        let datafile_record = datafile_record_for_string_u32("byeworld", 2);
        let expected = keydir_entry_for(&datafile_record, db.writer.id, 35);

        assert!(db.commit_keydir_write(datafile_record.clone(), 35).is_ok());
        assert_eq!(db.keydir.len(), 2);
        assert_eq!(db.keydir.get(&datafile_record.key).unwrap(), &expected);

        let datafile_record = datafile_record_for_string_u32("byeworld", 3);
        let expected = keydir_entry_for(&datafile_record, db.writer.id, 70);

        assert!(db.commit_keydir_write(datafile_record.clone(), 70).is_ok());
        assert_ne!(db.keydir.len(), 3);
        assert_eq!(db.keydir.get(&datafile_record.key).unwrap(), &expected);
    }

    #[test]
    fn put_ok() {
        let (mut db, _dir) = BitcaskInstance::build_mock_bitcask_instance();

        let key = "hello world";
        let value: u32 = 2;

        let datafile_record = datafile_record_for_string_u32(key, value);
        let expected = keydir_entry_for(&datafile_record, db.writer.id, 0);

        assert!(db.put(key.as_bytes(), &value.to_be_bytes()).is_ok());

        let mut expected_bytes = Vec::new();
        Datafile::write_record_to(&mut expected_bytes, &datafile_record).unwrap();
        let read_bytes = db
            .writer
            .read_bytes(0, datafile_record.size().unwrap())
            .unwrap();
        assert_eq!(&read_bytes[0..1], &expected_bytes[0..1]);
        assert_eq!(&read_bytes[13..], &expected_bytes[13..]);

        let keydir_written = db.keydir.get(key.as_bytes()).unwrap();
        assert_eq!(db.keydir.len(), 1);
        assert_eq!(keydir_written.file_id, expected.file_id);
        assert_eq!(keydir_written.value_size, expected.value_size);
        assert_eq!(keydir_written.value_offset, expected.value_offset);

        assert_eq!(db.writer.offset, datafile_record.size().unwrap() as u64);
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

        let datafile_record = datafile_record_for_string_u32(key, value);
        assert!(db.writer.append_record(&datafile_record).is_ok());
        assert_eq!(db.writer.offset, datafile_record.size().unwrap() as u64);
    }

    #[test]
    fn put_switch_writer() {
        let (mut db, _dir) = BitcaskInstance::build_mock_bitcask_instance();
        let options = BitcaskOptions::default();
        db.writer.offset = options.get_datafile_max_size() as u64;

        assert_eq!(db.writer.id, 2);
        assert!(db.writer.is_writer);
        assert_eq!(db.writer.offset, options.get_datafile_max_size() as u64);

        let key = "hello world";
        let value: u32 = 2;

        assert!(db.put(key.as_bytes(), &value.to_be_bytes()).is_ok());

        // Make sure new writer is created with proper id, role and offset
        assert_eq!(db.writer.id, 3);
        assert!(db.writer.is_writer);
        assert_eq!(
            db.writer.offset,
            datafile_record_for_string_u32(key, value).size().unwrap() as u64
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

        let record = datafile_record_for_string_u32(key, value);

        // Prepare reader datafile with the requested entry
        let mut missed_reader = Datafile::new_writer(&db.dir_path, 3).unwrap();
        assert!(missed_reader.append_record(&record).is_ok());
        missed_reader.is_writer = false;

        // Prepare KeyDir to store a mapping of the key to the file that isn't present in LRU
        let value_offset = DATAFILE_HEADER_SIZE_BYTES as u64 + record.header.key_size as u64;
        db.keydir.put(record, 3, value_offset);

        // Make sure the datafile is not known
        assert_eq!(db.readers.len(), 1);

        assert_eq!(
            db.get(key.as_bytes()).unwrap().unwrap(),
            Box::from(value.to_be_bytes())
        );
        // Verify the missed reader has been discovered
        assert_eq!(db.readers.len(), 2);
    }

    #[test]
    fn open_new() {
        let temp_dir = tempfile::tempdir().unwrap();

        let db = BitcaskInstance::open(temp_dir.path().to_path_buf(), BitcaskOptions::default())
            .unwrap();

        assert_eq!(db.writer.id, 1);
        assert_eq!(db.writer.offset, 0);
        assert!(db.writer.is_writer);

        assert!(db.readers.is_empty());
        assert!(db.keydir.is_empty());
    }

    #[test]
    fn open_existing() {
        let temp_dir = tempfile::tempdir().unwrap();
        let options = BitcaskOptions::default();

        const RECORD_COUNT: usize = 1000;
        const RECORD_SIZE: usize = 29;

        let fratio: f32 =
            (RECORD_COUNT * RECORD_SIZE) as f32 / options.get_datafile_max_size() as f32;

        // Temporary scope to fill the data
        {
            let mut db =
                BitcaskInstance::open(temp_dir.path().to_path_buf(), BitcaskOptions::default())
                    .unwrap();

            // 28B per entry - 2 readers and 1 writer to fit 1000 entries with 10K Datafile max length
            for i in 0..RECORD_COUNT as u32 {
                db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
            }

            assert_eq!(db.writer.id, fratio.ceil() as u32);

            assert!(if fratio.round() == fratio {
                db.writer.offset == 0
            } else {
                db.writer.offset != 0
            });

            assert_eq!(db.readers.len(), fratio.floor() as usize);
            assert_eq!(db.keydir.len(), RECORD_COUNT);
        } // File handles should be released at this point

        {
            let mut db =
                BitcaskInstance::open(temp_dir.path().to_path_buf(), BitcaskOptions::default())
                    .unwrap();

            assert_eq!(db.writer.id, fratio.ceil() as u32 + 1); // New writer should be created
            assert_eq!(db.writer.offset, 0);

            assert_eq!(db.readers.len(), fratio.ceil() as usize);
            assert_eq!(db.keydir.len(), RECORD_COUNT);

            // 28B per entry - 2 readers and 1 writer to fit 1000 entries with 10K Datafile max length
            for i in 0..RECORD_COUNT as u32 {
                assert_eq!(
                    db.get(&i.to_be_bytes()).unwrap().unwrap(),
                    Box::from(i.to_be_bytes())
                );
            }
        }
    }

    #[test]
    fn remove_ok() {
        let temp_dir = tempfile::tempdir().unwrap();

        const ENTRY_COUNT: usize = 20;
        const ENTRY_SIZE: usize = 29;
        // Tombstone log is 21 + 4 = 25 bytes
        const DATAFILE_TOMBSTONE_SIZE: usize = 25;

        let mut db =
            BitcaskInstance::open(temp_dir.path().to_path_buf(), BitcaskOptions::default())
                .unwrap();

        for i in 0..ENTRY_COUNT as u32 {
            db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
        }
        assert_eq!(db.writer.offset, (ENTRY_SIZE * ENTRY_COUNT) as u64);

        let delete_count = ENTRY_COUNT.div_ceil(2);
        for i in 0..delete_count as u32 {
            db.remove(&i.to_be_bytes()).unwrap();
        }
        assert_eq!(db.keydir.len(), ENTRY_COUNT - delete_count);

        assert!(db.get(&0u32.to_be_bytes()).unwrap().is_none());
        assert!(
            db.get(&(delete_count as u32 - 1).to_be_bytes())
                .unwrap()
                .is_none()
        );
        assert!(
            db.get(&((delete_count + 1) as u32).to_be_bytes())
                .unwrap()
                .is_some()
        );

        // We must have a writer offset incremented by delete_count * DATAFILE_TOMBSTONE_SIZE bytes
        // to make sure all of the tombstones are present
        assert_eq!(
            db.writer.offset,
            (ENTRY_SIZE * ENTRY_COUNT + delete_count * DATAFILE_TOMBSTONE_SIZE) as u64
        );
    }

    #[test]
    fn remove_with_replay_ok() {
        let temp_dir = tempfile::tempdir().unwrap();

        const RECORD_COUNT: usize = 50;
        const DELETES_COUNT: usize = RECORD_COUNT.div_ceil(2);

        // Temporary scope to fill the data
        {
            let mut db =
                BitcaskInstance::open(temp_dir.path().to_path_buf(), BitcaskOptions::default())
                    .unwrap();

            for i in 0..RECORD_COUNT as u32 {
                db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
            }

            for i in 0..DELETES_COUNT as u32 {
                db.remove(&i.to_be_bytes()).unwrap();
            }

            assert_eq!(db.keydir.len(), RECORD_COUNT - DELETES_COUNT);
        } // File handles should be released at this point

        {
            let db =
                BitcaskInstance::open(temp_dir.path().to_path_buf(), BitcaskOptions::default())
                    .unwrap();

            assert_eq!(db.keydir.len(), RECORD_COUNT - DELETES_COUNT);
        }
    }

    #[test]
    fn put_commits_when_oversized_entry_but_df_empty_and_soft_cap() {
        let temp_dir = tempfile::tempdir().unwrap();

        let mut db = BitcaskInstance::open(
            temp_dir.path().to_path_buf(),
            BitcaskOptions::default()
                .datafile_max_size(crate::options::DatafileMaxSize::SoftCappedBytes(40)),
        )
        .unwrap();

        assert!(db.put(&0u128.to_be_bytes(), &0u128.to_be_bytes()).is_ok());
        assert_eq!(db.writer.id, 1); // Due to current writer being empty, we expect it to have at
        // least one entry
        assert_eq!(db.writer.offset, 53);

        assert!(db.put(&1u128.to_be_bytes(), &1u128.to_be_bytes()).is_ok());
        assert_eq!(db.writer.id, 2); // First writer is now oversized, so we switchover and commit
        // on the new writer
        assert_eq!(db.writer.offset, 53);
    }

    #[test]
    fn put_not_commits_when_oversized_entry_but_df_empty_and_hard_cap() {
        let temp_dir = tempfile::tempdir().unwrap();

        let mut db = BitcaskInstance::open(
            temp_dir.path().to_path_buf(),
            BitcaskOptions::default()
                .datafile_max_size(crate::options::DatafileMaxSize::HardCappedBytes(40)),
        )
        .unwrap();

        assert!(matches!(
            db.put(&0u128.to_be_bytes(), &0u128.to_be_bytes()),
            Err(BitcaskError::RecordLargerThanMaxDatafileSize(53usize))
        ));
        assert_eq!(db.keydir.len(), 0);
        assert_eq!(db.writer.id, 1); // No writer switchover should occur

        // First write should go through normally
        assert!(db.put(&0u32.to_be_bytes(), &0u32.to_be_bytes()).is_ok());
        assert_eq!(db.keydir.len(), 1);
        assert_eq!(db.writer.id, 1);
        assert_eq!(db.writer.offset, 29);

        // The writer overflows, so we should switchover and write
        assert!(db.put(&1u32.to_be_bytes(), &1u32.to_be_bytes()).is_ok());
        assert_eq!(db.keydir.len(), 2);
        assert_eq!(db.writer.id, 2);
        assert_eq!(db.writer.offset, 29);
    }
}
