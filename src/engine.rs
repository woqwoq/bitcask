use std::{
    fs::{self, read_dir},
    io::ErrorKind,
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use lru::LruCache;

use crate::{
    datafile::{Datafile, DatafileRecordIterator},
    error::{BitcaskError, DatafileError, HintfileError},
    hint::Hintfile,
    keydir::KeyDir,
    options::{BitcaskOptions, DatafileMaxSize, RecoveryMode, SyncFrequency, SyncMode},
    record::{
        DATAFILE_HEADER_SIZE_BYTES, DATAFILE_RECORD_TYPE_NORMAL, DATAFILE_RECORD_TYPE_TOMBSTONE,
        DatafileRecord,
    },
};

pub struct MergeTracker {
    pub active: bool,
    pub bytes_written: u64,
    pub dead_bytes_written: u64,
}

impl Default for MergeTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl MergeTracker {
    pub fn new() -> Self {
        Self {
            active: true,
            bytes_written: 0,
            dead_bytes_written: 0,
        }
    }

    pub fn write(&mut self, size: u64) {
        if self.active {
            self.bytes_written += size;
        }
    }

    pub fn remove(&mut self, size: u64) {
        if self.active {
            self.dead_bytes_written += size;
        }
    }

    pub fn reset(&mut self) {
        self.bytes_written = 0;
        self.dead_bytes_written = 0;
    }

    pub fn should_merge(&self, options: &BitcaskOptions) -> bool {
        let dead = self.dead_bytes_written as u128;
        let total = self.bytes_written as u128;
        let threshold = options.merge_dead_space_threshold_percent as u128;

        if self.active
            && total > 0
            && (dead * 100) > (threshold * total)
            && self.dead_bytes_written > options.merge_dead_space_threshold_bytes
        {
            println!(
                "Merging...\nDead: {dead}/{perc}%, Total:{total}, Threshold: {threshold}",
                perc = (dead * 100) / total
            );

            return true;
        }

        false
    }
}

pub struct BitcaskInstance {
    dir: PathBuf,                         // For opening files through LRU
    pub(crate) keydir: KeyDir,            // In-memory keydir map
    pub writer: Datafile,                 // Writer file handle
    pub readers: LruCache<u32, Datafile>, // Use LRU cache to track recently used files
    pub merge_tracker: MergeTracker,      // For tracking dead space and trigger compaction
    write_count: usize,                   // Write count tracker for sync
    options: BitcaskOptions,              // User-customisable options
}

pub struct BitcaskRecoveryHandler;

impl BitcaskRecoveryHandler {
    pub fn reopen_reader_for_recovery(
        dir: &Path,
        options: &BitcaskOptions,
        df: Datafile,
    ) -> Result<Datafile, DatafileError> {
        let id = df.id;
        let offset = df.offset;
        drop(df);
        Datafile::open_reader_for_recovery(dir, id, offset, &options.datafile_extension)
    }

    pub fn recover_post_df_replay_failure(
        dir: &Path,
        options: &BitcaskOptions,
        mut df: Datafile,
        error: DatafileError,
        is_last: bool,
    ) -> Result<Datafile, BitcaskError> {
        match options.recovery_mode {
            // Recover only the last datafile
            RecoveryMode::TolerateCorruptedTailRecords => {
                if !is_last {
                    // Failure in non-last datafiles is fatal
                    return Err(BitcaskError::FatalDatafileReplayFailure(error));
                }

                df = Self::reopen_reader_for_recovery(dir, options, df)?;
                match error {
                    DatafileError::TornWrite => df.truncate(df.offset)?,
                    DatafileError::UndersizedDatafileHeader(_) => df.truncate(df.offset)?,
                    DatafileError::IO(e) => {
                        if e.kind() == ErrorKind::UnexpectedEof {
                            df.truncate(df.offset)?
                        } else {
                            return Err(BitcaskError::FatalDatafileReplayFailure(
                                DatafileError::IO(e),
                            ));
                        }
                    }
                    DatafileError::CrcMismatch => df.truncate(df.offset)?,
                    e => return Err(BitcaskError::FatalDatafileReplayFailure(e)),
                }
                Ok(df)
            }
            RecoveryMode::AbsoluteConsistency => {
                Err(BitcaskError::FatalDatafileReplayFailure(error))
            }
        }
    }

    // TODO: Implement handling for TolerateCorruptedTailRecords
    pub fn recover_post_hf_replay_failure(
        e: HintfileError,
        options: &BitcaskOptions,
    ) -> Result<(), BitcaskError> {
        match options.recovery_mode {
            RecoveryMode::AbsoluteConsistency => Err(BitcaskError::FatalHintfileReplayFailure(e)),
            RecoveryMode::TolerateCorruptedTailRecords => Ok(()),
        }
    }
}

pub struct BitcaskStartupHandler;

impl BitcaskStartupHandler {
    pub fn reopen_datafile_as_writer(
        dir: &Path,
        options: &BitcaskOptions,
        df: Datafile,
    ) -> Result<Datafile, DatafileError> {
        let id = df.id;
        let offset = df.file_size()?;
        drop(df);
        Datafile::open_datafile_as_writer(dir, id, offset, &options.datafile_extension)
    }

    pub fn parse_db_datafile_ids_at_path(
        dir: &Path,
        datafile_extension: &str,
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
                if ext == datafile_extension {
                    ids.push(name.parse::<u32>()?);
                }
            } else {
                continue;
            }
        }
        ids.sort();

        Ok(ids)
    }

    // TODO: Increment merge tracker inside replay
    pub fn replay_and_init_readers(
        dir: &Path,
        options: &BitcaskOptions,
        keydir: &mut KeyDir,
        readers: &mut LruCache<u32, Datafile>,
    ) -> Result<Option<Datafile>, BitcaskError> {
        let df_ids = Self::parse_db_datafile_ids_at_path(dir, &options.datafile_extension)?;
        let last = df_ids.last().cloned();

        for id in df_ids {
            let is_last = Some(id) == last;
            let mut df = Datafile::open_reader(dir, id, &options.datafile_extension)?;

            let hint_entries_read =
                match Hintfile::open_if_exists(dir, id, &options.hintfile_extension)? {
                    Some(hf) => match hf.replay(keydir) {
                        Ok(n) => n,
                        Err(e) => {
                            BitcaskRecoveryHandler::recover_post_hf_replay_failure(e, options)?;
                            0
                        }
                    },
                    None => 0,
                };

            if hint_entries_read == 0
                && let Err(e) = df.replay(keydir)
            {
                df = BitcaskRecoveryHandler::recover_post_df_replay_failure(
                    dir, options, df, e, is_last,
                )?;
            }

            if is_last {
                return Ok(Some(df));
            }

            readers.put(id, df);
        }

        Ok(None)
    }

    pub fn create_writer_with_options(
        dir: &Path,
        options: &BitcaskOptions,
        latest_datafile: Option<Datafile>,
        readers: &mut LruCache<u32, Datafile>,
    ) -> Result<Datafile, BitcaskError> {
        if let Some(df) = latest_datafile {
            if options.reuse_old_writer_on_startup {
                return Ok(Self::reopen_datafile_as_writer(dir, options, df)?);
            }

            let id = df.id;
            readers.put(id, df);

            return Ok(Datafile::new_writer(
                dir,
                id + 1,
                &options.datafile_extension,
            )?);
        }

        Ok(Datafile::new_writer(dir, 1, &options.datafile_extension)?)
    }
}

impl BitcaskInstance {
    pub fn open(dir: PathBuf, options: BitcaskOptions) -> Result<Self, BitcaskError> {
        let mut keydir = KeyDir::with_capacity(options.expected_keydir_max_size);
        let mut readers: LruCache<u32, Datafile> =
            LruCache::new(NonZeroUsize::new(options.reader_lru_cache_size).unwrap());

        let latest_datafile = BitcaskStartupHandler::replay_and_init_readers(
            &dir,
            &options,
            &mut keydir,
            &mut readers,
        )?;

        let writer = BitcaskStartupHandler::create_writer_with_options(
            &dir,
            &options,
            latest_datafile,
            &mut readers,
        )?;

        Ok(BitcaskInstance {
            keydir,
            writer,
            dir,
            readers,
            merge_tracker: MergeTracker::default(),
            write_count: 0,
            options,
        })
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), BitcaskError> {
        let _ = self.write_datafile_record(&DatafileRecord::new(key, value)?)?;

        Ok(())
    }

    pub fn len(&self) -> usize {
        self.keydir.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keydir.is_empty()
    }

    pub fn get(&mut self, key: &[u8]) -> Result<Option<Box<[u8]>>, BitcaskError> {
        if let Some(keydir_entry) = self.keydir.get(key) {
            let reader = if self.writer.id == keydir_entry.file_id {
                self.writer.flush()?;
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

    fn write_datafile_record(&mut self, record: &DatafileRecord) -> Result<u64, BitcaskError> {
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

        let offset = self.commit_datafile_write(record)?; // Write record first, so that it can be
        // replayed in case of a crash.
        self.write_count += 1;

        self.sync_writer()?; // Sync IO according to user's options

        self.commit_keydir_write(record, offset)?; // Append entry to the keydir

        Ok(offset)
    }

    pub fn remove(&mut self, key: &[u8]) -> Result<(), BitcaskError> {
        if self.keydir.contains(key) {
            self.write_datafile_record(&DatafileRecord::new_tombstone(key)?)?;
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
        record: &DatafileRecord,
        offset: u64,
    ) -> Result<(), BitcaskError> {
        let ksz = record.header.key_size;
        let header_and_key_size = DATAFILE_HEADER_SIZE_BYTES as u32 + ksz;
        let old_vsz = self.keydir.get(&record.key).map_or(0, |kde| kde.value_size);

        match record.header.record_type {
            DATAFILE_RECORD_TYPE_NORMAL => {
                self.keydir.put(
                    record.key.clone(),
                    self.writer.id,
                    record.header.value_size,
                    offset + DATAFILE_HEADER_SIZE_BYTES as u64 + ksz as u64,
                    record.header.timestamp,
                );

                if old_vsz > 0 {
                    self.merge_tracker
                        .remove(header_and_key_size as u64 + old_vsz as u64);
                }
                self.merge_tracker
                    .write(header_and_key_size as u64 + record.header.value_size as u64);
            }
            DATAFILE_RECORD_TYPE_TOMBSTONE => {
                self.keydir.remove(&record.key);

                self.merge_tracker
                    .remove(header_and_key_size as u64 + old_vsz as u64);
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
        self.writer.flush()?;

        if self.merge_tracker.should_merge(&self.options) {
            self.merge()?; // Will also provide a fresh new writer
            return Ok(());
        }

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

    pub fn merge(&mut self) -> Result<(), BitcaskError> {
        self.merge_tracker.reset();
        self.merge_tracker.active = false;
        self.switch_writer_datafile()?; // Lock the latest writer

        let candidate_ids: Vec<u32> = BitcaskStartupHandler::parse_db_datafile_ids_at_path(
            &self.dir,
            &self.options.datafile_extension,
        )?
        .into_iter()
        .filter(|&id| id < self.writer.id)
        .collect();

        let mut hf = Hintfile::new_or_open_existing(
            &self.dir,
            self.writer.id,
            &self.options.hintfile_extension,
        )?;
        for id in candidate_ids.clone() {
            let df = Datafile::open_reader(&self.dir, id, &self.options.datafile_extension)?;
            let mut iter = DatafileRecordIterator::new(&df)?;

            while let Some(df_entry) = iter.next_record()? {
                if df_entry.header.record_type == DATAFILE_RECORD_TYPE_NORMAL
                    && let Some(keydir_entry) = self.keydir.get(&df_entry.key)
                    && keydir_entry.file_id == id
                    && keydir_entry.value_offset == df_entry.value_offset
                {
                    // Positive outcome - extract value
                    let mut hasher = df_entry.header.crc();
                    hasher.update(&df_entry.key);

                    // TODO: Possibly stream-write the value for hasher and write
                    // No need to stream the value in chunks as we need it allocated to perform the
                    // new write anyway
                    let value =
                        df.read_bytes(keydir_entry.value_offset, keydir_entry.value_size as usize)?;
                    hasher.update(&value);

                    if hasher.finalize() != df_entry.header.crc {
                        return Err(BitcaskError::DatafileError(DatafileError::CrcMismatch));
                    }

                    let record = DatafileRecord::with_timestamp(
                        &df_entry.key,
                        &value,
                        df_entry.header.timestamp,
                    )?;

                    let record_start_offset = self.write_datafile_record(&record)?; // We want to keep original timestamp
                    self.write_hintfile_record(&mut hf, &record, record_start_offset)?;
                }
                // Otherwise we ignore the value as it is either dead or the tombstone itself
            }
        }

        hf.sync()?; // Sync the final hintfile
        self.remove_old_readers_post_merge(candidate_ids)?;
        self.switch_writer_datafile()?; // Seal the last writer to avoid inconsistencies between
        // hintfile and datafile
        self.merge_tracker.active = true;
        Ok(())
    }

    fn write_hintfile_record(
        &mut self,
        hf: &mut Hintfile,
        record: &DatafileRecord,
        record_start_offset: u64,
    ) -> Result<(), BitcaskError> {
        // When a writer switchover has occured
        if hf.id != self.writer.id {
            let new = Hintfile::new_or_open_existing(
                &self.dir,
                self.writer.id,
                &self.options.hintfile_extension,
            )?;

            let mut old = std::mem::replace(hf, new);
            old.sync()?; // Sync before releasing the handle
            drop(old); // Seal old hintfile
        }

        let value_offset =
            record_start_offset + DATAFILE_HEADER_SIZE_BYTES as u64 + record.header.key_size as u64;

        hf.append_hint_record(record, value_offset)?;

        Ok(())
    }

    fn remove_old_readers_post_merge(&mut self, ids: Vec<u32>) -> Result<(), BitcaskError> {
        for id in ids {
            self.readers.pop(&id); // Evict the file from LRU

            // Remove the reader datafile from filesystem
            fs::remove_file(Datafile::generate_filename(
                &self.dir,
                id,
                &self.options.datafile_extension,
            ))?;

            // Remove the reader's hintfile from filesystem
            let hf = Datafile::generate_filename(
                // Remove the file from file system
                &self.dir,
                id,
                &self.options.hintfile_extension,
            );
            if hf.exists() {
                fs::remove_file(hf)?;
            }
        }

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
        engine::{BitcaskInstance, BitcaskStartupHandler},
        error::{BitcaskError, DatafileError},
        hint::{HINTFILE_RECORD_HEADER_SIZE_BYTES, Hintfile},
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
        Datafile::generate_filename(dir, id, &options.datafile_extension)
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

    fn replay_env(_dir: &Path, options: &BitcaskOptions) -> (KeyDir, LruCache<u32, Datafile>) {
        (
            KeyDir::default(),
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
            db.writer.flush().unwrap();

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
            missed_reader.flush().unwrap();

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

            let (mut keydir, mut readers) = replay_env(dir, &options);

            let latest_datafile = BitcaskStartupHandler::replay_and_init_readers(
                dir,
                &options,
                &mut keydir,
                &mut readers,
            )
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

            let (mut keydir, mut readers) = replay_env(temp_dir.path(), &options);

            let latest_datafile = BitcaskStartupHandler::replay_and_init_readers(
                temp_dir.path(),
                &options,
                &mut keydir,
                &mut readers,
            )
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

            let (mut keydir, mut readers) = replay_env(dir, &options);

            let result = BitcaskStartupHandler::replay_and_init_readers(
                dir,
                &options,
                &mut keydir,
                &mut readers,
            );

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

            let (mut keydir, mut readers) = replay_env(dir, &options);

            let latest_datafile = BitcaskStartupHandler::replay_and_init_readers(
                dir,
                &options,
                &mut keydir,
                &mut readers,
            )
            .unwrap();

            assert_eq!(keydir.len(), 3);
            assert!(!keydir.contains(b"torn"));
            assert!(!latest_datafile.unwrap().is_writer);

            assert_eq!(
                std::fs::metadata(datafile_path(dir, 1)).unwrap().len(),
                records_total_size(1, 3)
            );

            let (mut keydir, mut readers) = replay_env(dir, &options);
            BitcaskStartupHandler::replay_and_init_readers(
                dir,
                &options,
                &mut keydir,
                &mut readers,
            )
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

            let (mut keydir, mut readers) = replay_env(dir, &options);

            BitcaskStartupHandler::replay_and_init_readers(
                dir,
                &options,
                &mut keydir,
                &mut readers,
            )
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

            let (mut keydir, mut readers) = replay_env(dir, &options);

            BitcaskStartupHandler::replay_and_init_readers(
                dir,
                &options,
                &mut keydir,
                &mut readers,
            )
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

            let (mut keydir, mut readers) = replay_env(dir, &options);

            let result = BitcaskStartupHandler::replay_and_init_readers(
                dir,
                &options,
                &mut keydir,
                &mut readers,
            );

            assert!(matches!(
                result,
                Err(BitcaskError::FatalDatafileReplayFailure(
                    DatafileError::UnexpectedDatafileRecordType
                ))
            ));

            assert_eq!(
                std::fs::metadata(datafile_path(dir, 1)).unwrap().len(),
                good_len + bytes.len() as u64
            );
        }

        #[test]
        fn replay_fallback_to_datafile_when_hf_empty() {
            let temp_dir = tempfile::tempdir().unwrap();
            let options = BitcaskOptions::default();

            // Create database with 2 entries, create a hintfile for the writer
            {
                let mut db =
                    BitcaskInstance::open(temp_dir.path().to_path_buf(), options.clone()).unwrap();

                db.put(b"1", b"1").unwrap();
                db.put(b"2", b"2").unwrap();

                Hintfile::new_or_open_existing(
                    temp_dir.path(),
                    db.writer.id,
                    &options.hintfile_extension,
                )
                .unwrap();
            }

            // Verify the database has the 2 entries even though the hintfile was empty
            // Perform merge (append 2 records from above to hintfile), reopen hintfile and add a fake record
            {
                let mut db =
                    BitcaskInstance::open(temp_dir.path().to_path_buf(), options.clone()).unwrap();

                assert_eq!(db.writer.id, 2);
                assert_eq!(db.len(), 2);

                db.merge().unwrap(); // Writes the values to the hintfile

                let mut hf = Hintfile::new_or_open_existing(
                    temp_dir.path(),
                    db.writer.id,
                    &options.hintfile_extension,
                )
                .unwrap();

                hf.append_hint_record(&DatafileRecord::new(b"3", b"3").unwrap(), 0)
                    .unwrap();
            }

            // Reopen database and verify all records were loaded from hintfile (fake one should be
            // present)
            let db = BitcaskInstance::open(temp_dir.path().to_path_buf(), options.clone()).unwrap();
            assert_eq!(db.writer.id, 5);
            assert_eq!(db.len(), 3);
        }

        #[test]
        fn reopen_datafile_as_writer_opens_with_offset_set() {
            let options = BitcaskOptions::default();
            let temp_dir = tempfile::tempdir().unwrap();

            {
                let mut df =
                    Datafile::new_writer(temp_dir.path(), 1, &options.datafile_extension).unwrap();

                df.append_record(&DatafileRecord::new(b"1", b"1").unwrap())
                    .unwrap();
            }

            let df =
                Datafile::open_reader(temp_dir.path(), 1, &options.datafile_extension).unwrap();

            let df =
                BitcaskStartupHandler::reopen_datafile_as_writer(temp_dir.path(), &options, df)
                    .unwrap();

            assert_eq!(df.offset, df.file_size().unwrap());
        }
    }

    #[test]
    fn commit_keydir_write() {
        let (_, _, mut db) = init_test();

        let datafile_record = datafile_record_for_string_u32("hello world", 2);
        let expected = keydir_entry_for(&datafile_record, db.writer.id, 0);

        assert!(db.commit_keydir_write(&datafile_record.clone(), 0).is_ok());
        assert_eq!(db.keydir.len(), 1);
        assert_eq!(db.keydir.get(&datafile_record.key).unwrap(), &expected);

        let datafile_record = datafile_record_for_string_u32("byeworld", 2);
        let expected = keydir_entry_for(&datafile_record, db.writer.id, 35);

        assert!(db.commit_keydir_write(&datafile_record.clone(), 35).is_ok());
        assert_eq!(db.keydir.len(), 2);
        assert_eq!(db.keydir.get(&datafile_record.key).unwrap(), &expected);

        let datafile_record = datafile_record_for_string_u32("byeworld", 3);
        let expected = keydir_entry_for(&datafile_record, db.writer.id, 70);

        assert!(db.commit_keydir_write(&datafile_record.clone(), 70).is_ok());
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

    #[test]
    fn write_hintfile_record() {
        let (temp_dir, options, mut db) = init_test();

        let mut hf = Hintfile::new_or_open_existing(
            temp_dir.path(),
            db.writer.id,
            &options.hintfile_extension,
        )
        .unwrap();

        const HINT_ENTRY_SIZE: usize = HINTFILE_RECORD_HEADER_SIZE_BYTES + 1;

        assert_eq!(hf.id, 1);
        let record = DatafileRecord::new(b"1", b"1").unwrap();
        assert!(db.write_hintfile_record(&mut hf, &record, 0).is_ok());
        assert_eq!(hf.file_size().unwrap(), HINT_ENTRY_SIZE as u64);

        db.switch_writer_datafile().unwrap();
        assert_eq!(hf.id, 1);
        let record = DatafileRecord::new(b"2", b"2").unwrap();
        assert!(db.write_hintfile_record(&mut hf, &record, 0).is_ok());
        assert_eq!(hf.id, 2);
        assert_eq!(hf.file_size().unwrap(), HINT_ENTRY_SIZE as u64);
    }

    mod merge {
        use crate::hint::{HINTFILE_RECORD_HEADER_SIZE_BYTES, Hintfile};

        use super::*;

        #[test]
        fn merge() {
            let (temp_dir, options, mut db) = init_test();

            const ENTRY_COUNT: usize = 500;
            const ENTRY_SIZE: usize = DATAFILE_HEADER_SIZE_BYTES + 4 + 4;
            const HINT_ENTRY_SIZE: usize = HINTFILE_RECORD_HEADER_SIZE_BYTES + 4;
            let entries_to_fit_on_single_df = options.datafile_max_size.get_size() / ENTRY_SIZE;

            let entries_last_merge_writer = match ENTRY_COUNT % entries_to_fit_on_single_df {
                0 => entries_to_fit_on_single_df,
                remainder => remainder,
            };

            for i in 0..ENTRY_COUNT as u32 {
                db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
            }

            let datafiles_to_house_payload: usize =
                ENTRY_COUNT.div_ceil(entries_to_fit_on_single_df);

            let datafiles_to_house_dead_payload: usize =
                (2 * ENTRY_COUNT).div_ceil(entries_to_fit_on_single_df);

            assert_eq!(db.writer.id, datafiles_to_house_payload as u32);
            assert_eq!(db.len(), ENTRY_COUNT);

            for i in 0..ENTRY_COUNT as u32 {
                db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
            }
            assert_eq!(db.writer.id, datafiles_to_house_dead_payload as u32);
            assert_eq!(db.len(), ENTRY_COUNT);

            assert!(db.merge().is_ok());

            assert_eq!(
                db.writer.id,
                datafiles_to_house_dead_payload as u32 // All sealed during merge
                    + datafiles_to_house_payload as u32 // New files to house the data
                + 1 // Last sealed writer
            );
            assert_eq!(db.len(), ENTRY_COUNT); // All KV pairs should be still present
            assert_eq!(
                db.readers.len(),
                options
                    .reader_lru_cache_size
                    .min(datafiles_to_house_payload)
            ); // LRU Should have only 1
            // 1 reader post-merge as the rest were evicted

            // Old datafiles should be gone from the filesystem
            for id in 0..datafiles_to_house_dead_payload as u32 {
                assert!(
                    !Datafile::generate_filename(temp_dir.path(), id, &options.datafile_extension)
                        .exists()
                );
            }

            // Old KV pairs should be still valid
            for i in 0..ENTRY_COUNT as u32 {
                assert_eq!(
                    db.get(&i.to_be_bytes()).unwrap(),
                    Some(Box::from(i.to_be_bytes()))
                );
            }

            // Hints should be written to the new datafiles including writer
            let last = db.writer.id - 1;
            for (id, _) in db.readers {
                let hf = Hintfile::new_or_open_existing(temp_dir.path(), id, "hint").unwrap();

                if id == last {
                    assert_eq!(
                        hf.file_size().unwrap(),
                        (entries_last_merge_writer * HINT_ENTRY_SIZE) as u64
                    );
                    continue;
                }

                assert_eq!(
                    hf.file_size().unwrap(),
                    (entries_to_fit_on_single_df * HINT_ENTRY_SIZE) as u64
                );
            }

            // Old writer used during merge has been sealed, so current one should be empty
            assert_eq!(
                Hintfile::new_or_open_existing(temp_dir.path(), db.writer.id, "hint")
                    .unwrap()
                    .file_size()
                    .unwrap(),
                0
            );
        }

        #[test]
        fn merge_tracker() {
            let temp_dir = tempfile::tempdir().unwrap();

            const ENTRY_COUNT: usize = 100;
            const ENTRY_SIZE: usize = DATAFILE_HEADER_SIZE_BYTES + 4 + 4;

            let options = BitcaskOptions::default()
                .merge_dead_space_threshold_percent(40)
                .merge_dead_space_min_bytes((ENTRY_COUNT * ENTRY_SIZE) as u64 - 1u64);

            let mut db = BitcaskInstance::open(
                temp_dir.path().to_path_buf(),
                BitcaskOptions::default().clone(),
            )
            .unwrap();

            for i in 0..ENTRY_COUNT as u32 {
                db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
            }

            assert_eq!(
                db.merge_tracker.bytes_written,
                (ENTRY_SIZE * ENTRY_COUNT) as u64
            );
            assert_eq!(db.merge_tracker.dead_bytes_written, 0u64);

            for i in 0..ENTRY_COUNT as u32 {
                db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
            }

            assert_eq!(
                db.merge_tracker.bytes_written,
                (2 * ENTRY_SIZE * ENTRY_COUNT) as u64
            );
            assert_eq!(
                db.merge_tracker.dead_bytes_written,
                (ENTRY_SIZE * ENTRY_COUNT) as u64
            );

            assert!(db.merge_tracker.should_merge(&options));
        }
    }
}
