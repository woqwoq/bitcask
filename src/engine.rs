use std::{
    fs::read_dir,
    io::ErrorKind,
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use lru::LruCache;

use crate::{
    datafile::Datafile,
    error::{BitcaskError, DatafileError},
    keydir::KeyDir,
    options::{BitcaskOptions, DatafileMaxSize, RecoveryMode, SyncFrequency, SyncMode},
    record::{
        DATAFILE_HEADER_SIZE_BYTES, DATAFILE_RECORD_TYPE_NORMAL, DATAFILE_RECORD_TYPE_TOMBSTONE,
        DatafileRecord,
    },
};

pub struct BitcaskInstance {
    dir: PathBuf,                         // For opening files through LRU
    pub(crate) keydir: KeyDir,            // In-memory keydir map
    pub writer: Datafile,                 // Writer file handle
    pub readers: LruCache<u32, Datafile>, // Use LRU cache to track recently used files
    write_count: usize,                   // Write count tracker for sync
    options: BitcaskOptions,              // User-customisable options
}

pub struct BitcaskRecoveryHandler {
    dir: PathBuf,
    recovery_mode: RecoveryMode,
    datafile_extension: String,
}

impl BitcaskRecoveryHandler {
    pub fn new(dir: &Path, bitcask_options: &BitcaskOptions) -> Self {
        Self {
            dir: dir.to_path_buf(),
            recovery_mode: bitcask_options.recovery_mode.clone(),
            datafile_extension: bitcask_options.datafile_extension.clone(),
        }
    }

    pub fn reopen_reader_for_recovery(&self, df: Datafile) -> Result<Datafile, DatafileError> {
        let id = df.id;
        let offset = df.offset;
        drop(df);
        Datafile::open_reader_for_recovery(&self.dir, id, offset, &self.datafile_extension)
    }

    pub fn recover_post_failure(
        &self,
        mut df: Datafile,
        error: DatafileError,
        is_last: bool,
    ) -> Result<Datafile, BitcaskError> {
        match self.recovery_mode {
            // Recover only the last datafile
            RecoveryMode::TolerateCorruptedTailRecords => {
                if !is_last {
                    // Failure in non-last datafiles is fatal
                    return Err(BitcaskError::FatalReplayFailure(error));
                }

                df = self.reopen_reader_for_recovery(df)?;
                match error {
                    DatafileError::TornWrite => df.truncate(df.offset)?,
                    DatafileError::UndersizedDatafileHeader(_) => df.truncate(df.offset)?,
                    DatafileError::IO(e) => {
                        if e.kind() == ErrorKind::UnexpectedEof {
                            df.truncate(df.offset)?
                        } else {
                            return Err(BitcaskError::FatalReplayFailure(DatafileError::IO(e)));
                        }
                    }
                    DatafileError::CrcMismatch => df.truncate(df.offset)?,
                    e => return Err(BitcaskError::FatalReplayFailure(e)),
                }
                Ok(df)
            }
            RecoveryMode::AbsoluteConsistency => Err(BitcaskError::FatalReplayFailure(error)),
        }
    }
}

pub struct BitcaskStartupHandler {
    dir: PathBuf,
    reuse_old_writer_on_startup: bool,
    datafile_extension: String,
}

impl BitcaskStartupHandler {
    pub fn new(dir: &Path, options: &BitcaskOptions) -> Self {
        Self {
            dir: dir.to_path_buf(),
            reuse_old_writer_on_startup: options.reuse_old_writer_on_startup,
            datafile_extension: options.datafile_extension.clone(),
        }
    }

    pub fn reopen_datafile_as_writer(&self, df: Datafile) -> Result<Datafile, DatafileError> {
        let id = df.id;
        let offset = df.offset;
        drop(df);
        Datafile::open_datafile_as_writer(&self.dir, id, offset, &self.datafile_extension)
    }

    pub fn parse_db_datafile_ids_at_path(&self) -> Result<Vec<u32>, BitcaskError> {
        if !self.dir.exists() {
            std::fs::create_dir_all(&self.dir)?;
        }

        if !self.dir.is_dir() {
            return Err(BitcaskError::PathIsFile);
        }

        let mut ids: Vec<u32> = Vec::new();
        for file in read_dir(&self.dir)? {
            let filename = file?
                .file_name()
                .into_string()
                .map_err(|_| BitcaskError::FilenameConversion)?;

            if let Some((name, ext)) = filename.rsplit_once('.') {
                if ext == self.datafile_extension {
                    ids.push(name.parse::<u32>()?);
                }
            } else {
                continue;
            }
        }
        ids.sort();

        Ok(ids)
    }

    pub fn replay_and_init_readers(
        &self,
        recovery_handler: &BitcaskRecoveryHandler,
        keydir: &mut KeyDir,
        readers: &mut LruCache<u32, Datafile>,
    ) -> Result<Option<Datafile>, BitcaskError> {
        let ids = self.parse_db_datafile_ids_at_path()?;

        let last = ids.last().cloned();
        for id in ids {
            let mut df = Datafile::open_reader(&self.dir, id, &self.datafile_extension)?;
            let is_last = Some(id) == last;
            if let Err(e) = df.replay(keydir) {
                df = recovery_handler.recover_post_failure(df, e, is_last)?;
            }

            if is_last {
                return Ok(Some(df));
            }

            readers.put(id, df);
        }

        Ok(None)
    }

    pub fn create_writer_with_options(
        &self,
        latest_datafile: Option<Datafile>,
        readers: &mut LruCache<u32, Datafile>,
    ) -> Result<Datafile, BitcaskError> {
        if let Some(df) = latest_datafile {
            if self.reuse_old_writer_on_startup {
                return Ok(self.reopen_datafile_as_writer(df)?);
            }

            let id = df.id;
            readers.put(id, df);

            return Ok(Datafile::new_writer(
                &self.dir,
                id + 1,
                &self.datafile_extension,
            )?);
        }

        Ok(Datafile::new_writer(
            &self.dir,
            1,
            &self.datafile_extension,
        )?)
    }
}

impl BitcaskInstance {
    pub fn open(dir: PathBuf, options: BitcaskOptions) -> Result<Self, BitcaskError> {
        let startup_handler = BitcaskStartupHandler::new(&dir, &options);
        let recovery_handler = BitcaskRecoveryHandler::new(&dir, &options);

        let mut keydir = KeyDir::new();
        let mut readers: LruCache<u32, Datafile> =
            LruCache::new(NonZeroUsize::new(options.reader_lru_cache_size).unwrap());

        let latest_datafile = startup_handler.replay_and_init_readers(
            &recovery_handler,
            &mut keydir,
            &mut readers,
        )?;

        let writer = startup_handler.create_writer_with_options(latest_datafile, &mut readers)?;
        Ok(BitcaskInstance {
            keydir,
            writer,
            dir,
            readers,
            write_count: 0,
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
            } else if let Ok(reader) = Datafile::open_reader(
                &self.dir,
                keydir_entry.file_id,
                &self.options.datafile_extension,
            ) {
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
        if rsz > self.options.datafile_max_size.get_size()
            && matches!(
                self.options.datafile_max_size,
                DatafileMaxSize::HardCappedBytes(_)
            )
        {
            return Err(BitcaskError::RecordLargerThanMaxDatafileSize(rsz));
        }

        if self
            .writer
            .should_switch_writer_before_write_commit(rsz as u64, &self.options.datafile_max_size)?
        {
            self.switch_writer_datafile()?;
        }

        let offset = self.commit_datafile_write(&record)?; // Write record first, so that it can be
        // replayed in case of a crash.
        self.write_count += 1;

        self.sync_writer()?; // Sync IO according to user's options

        self.commit_keydir_write(record, offset)?; // Append entry to the keydir

        Ok(())
    }

    pub fn remove(&mut self, key: &[u8]) -> Result<(), BitcaskError> {
        if self.keydir.contains(key) {
            self.write_record(DatafileRecord::new_tombstone(key)?)?;
        }

        Ok(())
    }

    fn sync_writer(&mut self) -> Result<(), BitcaskError> {
        match self.options.sync_frequency {
            SyncFrequency::EveryWrite => self.writer.sync(&self.options.sync_mode)?,
            SyncFrequency::EveryNthWrite(n) => {
                if self.write_count.is_multiple_of(n) {
                    self.writer.sync(&self.options.sync_mode)?
                }
            }
            SyncFrequency::Never => {}
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
                    record.key,
                    self.writer.id,
                    record.header.value_size,
                    offset + DATAFILE_HEADER_SIZE_BYTES as u64 + ksz as u64,
                    record.header.timestamp,
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
        old_writer.sync(&SyncMode::SyncAll)?; // Force sync to persist

        let new_writer = Datafile::new_writer(
            &self.dir,
            old_writer.id + 1,
            &self.options.datafile_extension,
        )?;

        let old_writer = std::mem::replace(old_writer, new_writer);

        self.readers.put(old_writer.id, old_writer);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::OpenOptions,
        io::{Seek, SeekFrom, Write},
        num::NonZeroUsize,
        ops::Div,
        os::unix::fs::symlink,
        path::{Path, PathBuf},
    };

    use lru::LruCache;
    use tempfile::TempDir;

    use crate::{
        datafile::Datafile,
        engine::{BitcaskInstance, BitcaskRecoveryHandler, BitcaskStartupHandler},
        error::{BitcaskError, DatafileError},
        keydir::{KeyDir, KeyDirEntry},
        options::{BitcaskOptions, DatafileMaxSize},
        record::{DATAFILE_HEADER_SIZE_BYTES, DatafileRecord},
    };

    pub fn init_test() -> (TempDir, BitcaskOptions, BitcaskInstance) {
        let temp_dir = tempfile::tempdir().unwrap();
        let options = BitcaskOptions::default();
        let db = BitcaskInstance::open(
            temp_dir.path().to_path_buf(),
            BitcaskOptions::default().clone(),
        )
        .unwrap();

        (temp_dir, options, db)
    }

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

    fn fill_datafile(dir: &Path, id: u32, record_count: u32) {
        let options = BitcaskOptions::default();
        let mut df = Datafile::new_writer(dir, id, &options.datafile_extension).unwrap();
        for i in 0..record_count {
            df.append_record(&datafile_record_for_string_u32(&format!("{id}_{i}"), i))
                .unwrap();
        }
    }

    fn datafile_path(dir: &Path, id: u32) -> PathBuf {
        let options = BitcaskOptions::default();
        Datafile::generate_filename(dir, id, &options.datafile_extension).unwrap()
    }

    fn append_bytes(dir: &Path, id: u32, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .append(true)
            .open(datafile_path(dir, id))
            .unwrap();
        file.write_all(bytes).unwrap();
    }

    fn records_total_size(id: u32, record_count: u32) -> u64 {
        (0..record_count)
            .map(|i| {
                datafile_record_for_string_u32(&format!("{id}_{i}"), i)
                    .size()
                    .unwrap() as u64
            })
            .sum()
    }

    fn replay_env(
        dir: &Path,
        options: &BitcaskOptions,
    ) -> (
        BitcaskStartupHandler,
        BitcaskRecoveryHandler,
        KeyDir,
        LruCache<u32, Datafile>,
    ) {
        (
            BitcaskStartupHandler::new(dir, options),
            BitcaskRecoveryHandler::new(dir, options),
            KeyDir::new(),
            LruCache::new(NonZeroUsize::new(options.reader_lru_cache_size).unwrap()),
        )
    }

    mod open {
        use super::*;

        #[test]
        fn creates_new_instance_on_empty_dir() {
            let (_, _, db) = init_test();

            assert_eq!(db.writer.id, 1);
            assert_eq!(db.writer.offset, 0);
            assert!(db.writer.is_writer);

            assert!(db.readers.is_empty());
            assert!(db.keydir.is_empty());
        }

        #[test]
        fn replays_existing_data() {
            let temp_dir = tempfile::tempdir().unwrap();
            let options = BitcaskOptions::default();

            const RECORD_COUNT: usize = 1000;
            const RECORD_SIZE: usize = 29;

            let fratio: f32 =
                (RECORD_COUNT * RECORD_SIZE) as f32 / options.datafile_max_size.get_size() as f32;

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
    }

    mod put {
        use super::*;

        #[test]
        fn writes_record_to_writer_and_keydir() {
            let (_, _, mut db) = init_test();

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
        fn switches_writer_when_full() {
            #[allow(unused_variables)]
            let (temp_dir, options, mut db) = init_test();

            db.writer.offset = options.datafile_max_size.get_size() as u64;

            assert_eq!(db.writer.id, 1);
            assert!(db.writer.is_writer);
            assert_eq!(
                db.writer.offset,
                options.datafile_max_size.get_size() as u64
            );

            let key = "hello world";
            let value: u32 = 2;

            assert!(db.put(key.as_bytes(), &value.to_be_bytes()).is_ok());

            // Make sure new writer is created with proper id, role and offset
            assert_eq!(db.writer.id, 2);
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
        fn commits_oversized_entry_on_empty_writer_with_soft_cap() {
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
        fn rejects_oversized_entry_on_empty_writer_with_hard_cap() {
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

    mod get {
        use super::*;

        #[test]
        fn returns_written_value() {
            let (_, _, mut db) = init_test();

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
        fn opens_reader_on_lru_cache_miss() {
            #[allow(unused_variables)]
            let (temp_dir, _, mut db) = init_test();

            let key = "hello world";
            let value = 128u32;

            let record = datafile_record_for_string_u32(key, value);

            // Prepare reader datafile with the requested entry
            let mut missed_reader =
                Datafile::new_writer(&db.dir, 3, &db.options.datafile_extension).unwrap();
            assert!(missed_reader.append_record(&record).is_ok());
            missed_reader.is_writer = false;

            // Prepare KeyDir to store a mapping of the key to the file that isn't present in LRU
            let value_offset = DATAFILE_HEADER_SIZE_BYTES as u64 + record.header.key_size as u64;
            db.keydir.put(
                record.key,
                3,
                record.header.value_size,
                value_offset,
                record.header.timestamp,
            );

            // Make sure the datafile is not known
            assert_eq!(db.readers.len(), 0);

            assert_eq!(
                db.get(key.as_bytes()).unwrap().unwrap(),
                Box::from(value.to_be_bytes())
            );
            // Verify the missed reader has been discovered
            assert_eq!(db.readers.len(), 1);
        }
    }

    mod remove {
        use super::*;

        #[test]
        fn writes_tombstones() {
            let (_, _, mut db) = init_test();

            const ENTRY_COUNT: usize = 20;
            const ENTRY_SIZE: usize = 29;
            // Tombstone log is 21 + 4 = 25 bytes
            const DATAFILE_TOMBSTONE_SIZE: usize = 25;

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
        fn tombstones_survive_replay() {
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
    }

    mod replay_and_init_readers {
        use crate::record::DatafileRecordHeader;

        use super::*;

        #[test]
        fn replays_files_and_opens_read_only_readers() {
            let temp_dir = tempfile::tempdir().unwrap();
            let dir = temp_dir.path();
            let options = BitcaskOptions::default();

            fill_datafile(dir, 1, 3);
            fill_datafile(dir, 5, 2);
            fill_datafile(dir, 6, 0);

            let (startup, recovery, mut keydir, mut readers) = replay_env(dir, &options);

            let latest_datafile = startup
                .replay_and_init_readers(&recovery, &mut keydir, &mut readers)
                .unwrap();

            assert_eq!(keydir.len(), 5);
            assert_eq!(readers.len(), 2);

            for id in [1u32, 5] {
                let reader = readers.peek(&id).unwrap();
                assert_eq!(reader.id, id);
                assert!(!reader.is_writer);
            }

            let latest = latest_datafile.unwrap();
            assert_eq!(latest.id, 6);
            assert!(!latest.is_writer);

            let entry = keydir.get(b"1_0").unwrap();
            assert_eq!(entry.file_id, 1);
            let value = readers
                .peek(&entry.file_id)
                .unwrap()
                .read_bytes(entry.value_offset, entry.value_size as usize)
                .unwrap();
            assert_eq!(&*value, &0u32.to_be_bytes());

            let entry = keydir.get(b"5_1").unwrap();
            assert_eq!(entry.file_id, 5);
            let value = readers
                .peek(&entry.file_id)
                .unwrap()
                .read_bytes(entry.value_offset, entry.value_size as usize)
                .unwrap();
            assert_eq!(&*value, &1u32.to_be_bytes());
        }

        #[test]
        fn handles_empty_id_list() {
            let temp_dir = tempfile::tempdir().unwrap();
            let options = BitcaskOptions::default();

            let (startup, recovery, mut keydir, mut readers) =
                replay_env(temp_dir.path(), &options);

            let latest_datafile = startup
                .replay_and_init_readers(&recovery, &mut keydir, &mut readers)
                .unwrap();

            assert!(keydir.is_empty());
            assert!(readers.is_empty());
            assert!(latest_datafile.is_none());
        }

        #[test]
        fn missing_file_err() {
            let temp_dir = tempfile::tempdir().unwrap();
            let dir = temp_dir.path();
            let options = BitcaskOptions::default();

            symlink("00000007.db.missing", datafile_path(dir, 7)).unwrap();

            let (startup, recovery, mut keydir, mut readers) = replay_env(dir, &options);

            let result = startup.replay_and_init_readers(&recovery, &mut keydir, &mut readers);

            assert!(matches!(
                result,
                Err(BitcaskError::DatafileError(DatafileError::IO(_)))
            ));
        }

        #[test]
        fn recovers_torn_write() {
            let temp_dir = tempfile::tempdir().unwrap();
            let dir = temp_dir.path();
            let options = BitcaskOptions::default();

            fill_datafile(dir, 1, 3);

            let record = datafile_record_for_string_u32("torn", 4);
            let torn_len = DATAFILE_HEADER_SIZE_BYTES + record.header.key_size as usize + 2;
            let mut bytes = record.header.encode().to_vec();
            bytes.extend_from_slice(&record.key);
            bytes.truncate(torn_len);
            append_bytes(dir, 1, &bytes);

            let (startup, recovery, mut keydir, mut readers) = replay_env(dir, &options);

            let latest_datafile = startup
                .replay_and_init_readers(&recovery, &mut keydir, &mut readers)
                .unwrap();

            assert_eq!(keydir.len(), 3);
            assert!(!keydir.contains(b"torn"));
            assert!(!latest_datafile.unwrap().is_writer);

            assert_eq!(
                std::fs::metadata(datafile_path(dir, 1)).unwrap().len(),
                records_total_size(1, 3)
            );

            let (startup, recovery, mut keydir, mut readers) = replay_env(dir, &options);
            startup
                .replay_and_init_readers(&recovery, &mut keydir, &mut readers)
                .unwrap();
            assert_eq!(keydir.len(), 3);
        }

        #[test]
        fn recovers_incomplete_header() {
            let temp_dir = tempfile::tempdir().unwrap();
            let dir = temp_dir.path();
            let options = BitcaskOptions::default();

            fill_datafile(dir, 1, 2);

            append_bytes(dir, 1, &[0xDE, 0xAD, 0xBE]);

            let (startup, recovery, mut keydir, mut readers) = replay_env(dir, &options);

            startup
                .replay_and_init_readers(&recovery, &mut keydir, &mut readers)
                .unwrap();

            assert_eq!(keydir.len(), 2);
            assert_eq!(
                std::fs::metadata(datafile_path(dir, 1)).unwrap().len(),
                records_total_size(1, 2)
            );
        }

        #[test]
        fn recovers_crc_mismatch() {
            let temp_dir = tempfile::tempdir().unwrap();
            let dir = temp_dir.path();
            let options = BitcaskOptions::default();

            fill_datafile(dir, 1, 3);

            let first = datafile_record_for_string_u32("1_0", 0);
            let second = datafile_record_for_string_u32("1_1", 1);
            let corruption_offset = (first.size().unwrap() + DATAFILE_HEADER_SIZE_BYTES) as u64
                + second.header.key_size as u64;

            let mut file = OpenOptions::new()
                .write(true)
                .open(datafile_path(dir, 1))
                .unwrap();
            file.seek(SeekFrom::Start(corruption_offset)).unwrap();
            file.write_all(&[0xFF]).unwrap();

            let (startup, recovery, mut keydir, mut readers) = replay_env(dir, &options);

            startup
                .replay_and_init_readers(&recovery, &mut keydir, &mut readers)
                .unwrap();

            assert_eq!(keydir.len(), 1);
            assert!(keydir.contains(b"1_0"));
            assert!(!keydir.contains(b"1_1"));

            assert_eq!(
                std::fs::metadata(datafile_path(dir, 1)).unwrap().len(),
                first.size().unwrap() as u64
            );
        }

        #[test]
        fn fatal_on_unexpected_record_type() {
            let temp_dir = tempfile::tempdir().unwrap();
            let dir = temp_dir.path();
            let options = BitcaskOptions::default();

            fill_datafile(dir, 1, 1);
            let good_len = std::fs::metadata(datafile_path(dir, 1)).unwrap().len();

            let key = b"bad";
            let value = 1u32.to_be_bytes();
            let mut header = DatafileRecordHeader {
                crc: 0,
                record_type: 9,
                timestamp: 12345,
                key_size: key.len() as u32,
                value_size: value.len() as u32,
            };
            let mut hasher = header.crc();
            hasher.update(key);
            hasher.update(&value);
            header.crc = hasher.finalize();

            let mut bytes = header.encode().to_vec();
            bytes.extend_from_slice(key);
            bytes.extend_from_slice(&value);
            append_bytes(dir, 1, &bytes);

            let (startup, recovery, mut keydir, mut readers) = replay_env(dir, &options);

            let result = startup.replay_and_init_readers(&recovery, &mut keydir, &mut readers);

            assert!(matches!(
                result,
                Err(BitcaskError::FatalReplayFailure(
                    DatafileError::UnexpectedDatafileRecordType
                ))
            ));

            assert_eq!(
                std::fs::metadata(datafile_path(dir, 1)).unwrap().len(),
                good_len + bytes.len() as u64
            );
        }
    }

    #[test]
    fn commit_keydir_write() {
        let (_, _, mut db) = init_test();

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
    fn switch_writer_datafile() {
        #[allow(unused_variables)]
        let (temp_dir, _, mut db) = init_test();

        assert_eq!(db.writer.id, 1);
        assert!(db.writer.is_writer);
        assert_eq!(db.writer.offset, 0);
        assert_eq!(db.readers.len(), 0);

        assert!(db.switch_writer_datafile().is_ok());

        // Make sure old writer is demoted and added to LRU cache of readers
        assert_eq!(db.readers.len(), 1);
        let old = db.readers.get(&1u32).unwrap();
        assert_eq!(old.id, 1);
        assert!(!old.is_writer);

        // Make sure new writer is created with proper id, role and offset
        assert_eq!(db.writer.id, 2);
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
    fn reuse_old_writer_on_startup() {
        let temp_dir = tempfile::tempdir().unwrap();
        let options = BitcaskOptions::default()
            .datafile_max_size(DatafileMaxSize::HardCappedBytes(2000))
            .reuse_old_writer_on_startup(true);

        const ENTRY_COUNT: usize = 500;
        const ENTRY_SIZE: usize = DATAFILE_HEADER_SIZE_BYTES + 4 + 4;

        let entry_total_size = ENTRY_COUNT * ENTRY_SIZE;

        {
            let mut db =
                BitcaskInstance::open(temp_dir.path().to_path_buf(), options.clone()).unwrap();

            // 29B Entries
            for i in 0..ENTRY_COUNT as u32 {
                db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
            }

            assert_eq!(db.keydir.len(), ENTRY_COUNT);

            assert_eq!(
                db.writer.id,
                entry_total_size.div_ceil(options.datafile_max_size.get_size()) as u32
            );

            assert_eq!(
                db.readers.len(),
                entry_total_size.div(options.datafile_max_size.get_size())
            );
        }

        {
            let mut db =
                BitcaskInstance::open(temp_dir.path().to_path_buf(), options.clone()).unwrap();

            assert_eq!(db.keydir.len(), ENTRY_COUNT);

            // Same writer ID should be used
            assert_eq!(
                db.writer.id,
                entry_total_size.div_ceil(options.datafile_max_size.get_size()) as u32
            );

            assert_eq!(
                db.writer.offset
                    + db.readers
                        .iter()
                        .map(|(_, reader)| reader.offset)
                        .sum::<u64>(),
                entry_total_size as u64
            );

            assert_eq!(
                db.readers.len(),
                entry_total_size.div(options.datafile_max_size.get_size())
            );

            assert!(
                db.put(
                    &(ENTRY_COUNT as u32 + 1u32).to_be_bytes(),
                    &(ENTRY_COUNT as u32 + 1u32).to_be_bytes(),
                )
                .is_ok()
            );

            assert_eq!(
                db.writer.offset
                    + db.readers
                        .iter()
                        .map(|(_, reader)| reader.offset)
                        .sum::<u64>(),
                (entry_total_size + ENTRY_SIZE) as u64
            );
        }
    }
}
