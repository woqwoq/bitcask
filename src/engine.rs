use std::{
    fs::{self, read_dir},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock, atomic::AtomicU32},
};

use lru::LruCache;

use crate::{
    datafile::{Datafile, DatafileRecordIterator},
    error::{
        BitcaskError, DatafileError, GetError, HintfileError, MergeError, OpenError, PutError,
        RemoveError,
    },
    hint::Hintfile,
    keydir::KeyDir,
    options::{
        BitcaskOptions, DATAFILE_HEADER_SIZE_BYTES, DatafileMaxSize, RecoveryMode, SyncFrequency,
        SyncMode,
    },
    record::{DATAFILE_RECORD_TYPE_NORMAL, DATAFILE_RECORD_TYPE_TOMBSTONE, DatafileRecord},
};

pub(crate) struct StorageTracker {
    active: bool,
    bytes_written: u64,
    dead_bytes_written: u64,
    writes_performed: usize,
}

impl Default for StorageTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageTracker {
    pub(crate) fn new() -> Self {
        Self {
            active: true,
            bytes_written: 0,
            dead_bytes_written: 0,
            writes_performed: 0,
        }
    }

    pub(crate) fn increment_write(&mut self) {
        self.writes_performed += 1;
    }

    pub(crate) fn write_bytes(&mut self, size: u64) {
        if self.active {
            self.bytes_written += size;
        }
    }

    pub(crate) fn remove_bytes(&mut self, size: u64) {
        if self.active {
            self.dead_bytes_written += size;
        }
    }

    pub(crate) fn reset_bytes(&mut self) {
        self.bytes_written = 0;
        self.dead_bytes_written = 0;
    }

    pub(crate) fn disable_tracking_io_traffic(&mut self) {
        self.active = false;
    }

    pub(crate) fn enable_tracking_io_traffic(&mut self) {
        self.active = true;
    }

    pub(crate) fn should_merge(&self, options: &BitcaskOptions) -> bool {
        let dead = self.dead_bytes_written as u128;
        let total = self.bytes_written as u128;
        let threshold = options.merge_dead_space_threshold_percent as u128;

        if self.active
            && total > 0
            && (dead * 100) > (threshold * total)
            && self.dead_bytes_written > options.merge_dead_space_threshold_bytes
        {
            return true;
        }

        false
    }
}

pub struct BitcaskInstance {
    dir: PathBuf,                                 // For opening files through LRU
    keydir: RwLock<KeyDir>,                       // In-memory keydir map
    writer: Mutex<Datafile>,                      // Writer file handle
    writer_id: AtomicU32, // Writer file id to avoid locking during concurrent reads
    readers: Mutex<LruCache<u32, Arc<Datafile>>>, // Use LRU cache to track recently used files
    storage_tracker: Mutex<StorageTracker>, // For tracking dead space, writes for sync triggers and trigger compaction
    options: BitcaskOptions,                // User-customisable options
}

impl Drop for BitcaskInstance {
    fn drop(&mut self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.flush();
            let _ = writer.sync(&SyncMode::SyncAll);
        }
    }
}

#[hotpath::measure_all]
impl BitcaskInstance {
    pub(crate) fn open(dir: &Path, options: BitcaskOptions) -> Result<Self, BitcaskError> {
        options.validate().map_err(OpenError::InvalidDBOptions)?;
        let mut storage_tracker = StorageTracker::new();
        let mut keydir = KeyDir::with_capacity(options.expected_keydir_max_size);
        let mut readers: LruCache<u32, Arc<Datafile>> =
            LruCache::new(NonZeroUsize::new(options.reader_lru_cache_size).unwrap());

        let (latest_datafile, total_df_size) = BitcaskStartupHandler::replay_and_init_readers(
            dir,
            &options,
            &mut keydir,
            &mut readers,
        )?;

        // Restore merge tracker state
        storage_tracker.write_bytes(total_df_size);
        storage_tracker.remove_bytes(total_df_size.saturating_sub(keydir.get_live_log_size()));

        let writer = BitcaskStartupHandler::create_writer_with_options(
            dir,
            &options,
            latest_datafile,
            &mut readers,
        )?;

        Ok(BitcaskInstance {
            keydir: RwLock::new(keydir),
            writer_id: AtomicU32::new(writer.id),
            writer: Mutex::new(writer),
            dir: dir.to_path_buf(),
            readers: Mutex::new(readers),
            storage_tracker: Mutex::new(storage_tracker),
            options,
        })
    }

    pub(crate) fn put(&self, key: &[u8], value: &[u8]) -> Result<(), BitcaskError> {
        let record = DatafileRecord::new(key, value).map_err(PutError::Datafile)?;

        // Hold a single write lock across whole operation
        let mut writer = self.writer.lock().unwrap();

        self.write_datafile_record(&record, &mut writer)?;

        Ok(())
    }

    pub(crate) fn keys(&self) -> Vec<Box<[u8]>> {
        // We either do a heap allocation or a hold a read lock for as long as the iterator is alive
        self.keydir
            .read()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    }

    pub(crate) fn contains(&self, key: &[u8]) -> bool {
        self.keydir.read().unwrap().contains(key)
    }

    pub(crate) fn len(&self) -> usize {
        self.keydir.read().unwrap().len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.keydir.read().unwrap().is_empty()
    }

    pub(crate) fn get(&self, key: &[u8]) -> Result<Option<Box<[u8]>>, BitcaskError> {
        let keydir = self.keydir.read().unwrap();

        let Some(keydir_entry) = keydir.get(key).copied() else {
            return Ok(None);
        };

        // If the current writer contains the entry
        {
            if keydir_entry.file_id == self.writer_id.load(std::sync::atomic::Ordering::Acquire) {
                drop(keydir); // No need to hold a keydir lock, we are sure that the active writer
                // houses the file
                let mut writer = self.writer.lock().unwrap();

                writer.flush().map_err(GetError::Datafile)?;

                return Ok(Some(
                    writer
                        .read_bytes(keydir_entry.value_offset, keydir_entry.value_size as usize)
                        .map_err(GetError::Datafile)?,
                ));
            }
        }

        let reader = self.get_or_open_reader(keydir_entry.file_id)?;
        drop(keydir); // Now that we know we have the right reader open we can drop the keydir to
        // allow concurrent writes

        Ok(Some(
            reader
                .read_bytes(keydir_entry.value_offset, keydir_entry.value_size as usize)
                .map_err(GetError::Datafile)?,
        ))
    }

    fn get_or_open_reader(&self, file_id: u32) -> Result<Arc<Datafile>, GetError> {
        // If the LRU cache houses the reader containing the entry
        if let Some(reader) = self.readers.lock().unwrap().get(&file_id) {
            return Ok(Arc::clone(reader));
        }

        let reader =
            match Datafile::open_reader(&self.dir, file_id, &self.options.datafile_extension) {
                Ok(reader) => Arc::new(reader),
                Err(DatafileError::DatafileNotFound(_)) => {
                    return Err(GetError::DatafileMissing(file_id));
                }
                Err(e) => {
                    return Err(GetError::Datafile(e));
                }
            };

        self.readers
            .lock()
            .unwrap()
            .put(file_id, Arc::clone(&reader));

        Ok(reader)
    }

    fn write_datafile_record(
        &self,
        record: &DatafileRecord,
        writer: &mut Datafile,
    ) -> Result<u64, PutError> {
        let rsz = record.size().map_err(PutError::Datafile)?;

        // If our record is larger than the writer max size, we either throw an
        // error or let it happen and continue
        if rsz > self.options.datafile_max_size.get_size()
            && matches!(
                self.options.datafile_max_size,
                DatafileMaxSize::HardCappedBytes(_)
            )
        {
            return Err(PutError::RecordLargerThanMaxDatafileSize(rsz));
        }

        if writer
            .should_switch_writer_before_write_commit(rsz as u64, &self.options.datafile_max_size)
            .map_err(PutError::Datafile)?
        {
            self.switch_writer_datafile(writer)?;
        }

        let offset = writer.append_record(record).map_err(PutError::Datafile)?; // Write record first, so that it can be
        // replayed in case of a crash.
        self.storage_tracker.lock().unwrap().increment_write();

        self.sync_writer_if_needed(writer)
            .map_err(PutError::Datafile)?; // Sync IO according to user's options

        self.commit_keydir_write(record, writer.id, offset)
            .map_err(PutError::Datafile)?; // Append entry to the keydir

        Ok(offset)
    }

    pub(crate) fn remove(&self, key: &[u8]) -> Result<(), BitcaskError> {
        // Hold a single write lock across whole operation
        let mut writer = self.writer.lock().unwrap();

        if self.keydir.read().unwrap().contains(key) {
            let record = DatafileRecord::new_tombstone(key).map_err(RemoveError::Datafile)?;
            self.write_datafile_record(&record, &mut writer)
                .map_err(|e| match e {
                    PutError::Datafile(d) => RemoveError::Datafile(d),
                    PutError::AutoMerge(m) => RemoveError::AutoMerge(m),
                    PutError::RecordLargerThanMaxDatafileSize(_) => unreachable!(),
                })?;
        }

        Ok(())
    }

    pub(crate) fn sync(&self) -> Result<(), BitcaskError> {
        let mut writer = self.writer.lock().unwrap();

        self.flush_and_sync_writer(&mut writer)
            .map_err(BitcaskError::SyncFailure)
    }

    fn flush_and_sync_writer(&self, writer: &mut Datafile) -> Result<(), DatafileError> {
        writer.flush()?;
        writer.sync(&self.options.sync_mode)?;
        Ok(())
    }

    fn sync_writer_if_needed(&self, writer: &mut Datafile) -> Result<(), DatafileError> {
        match self.options.sync_frequency {
            SyncFrequency::EveryWrite => self.flush_and_sync_writer(writer)?,
            SyncFrequency::EveryNthWrite(n) => {
                if self
                    .storage_tracker
                    .lock()
                    .unwrap()
                    .writes_performed
                    .is_multiple_of(n)
                {
                    self.flush_and_sync_writer(writer)?;
                }
            }
            SyncFrequency::Never => {}
        }

        Ok(())
    }

    fn commit_keydir_write(
        &self,
        record: &DatafileRecord,
        writer_id: u32,
        offset: u64,
    ) -> Result<(), DatafileError> {
        let ksz = record.header.key_size;
        let header_and_key_size = DATAFILE_HEADER_SIZE_BYTES as u32 + ksz;
        let mut storage_tracker = self.storage_tracker.lock().unwrap();

        match record.header.record_type {
            DATAFILE_RECORD_TYPE_NORMAL => {
                let old = self.keydir.write().unwrap().put(
                    record.key.clone(),
                    writer_id,
                    record.header.value_size,
                    offset + DATAFILE_HEADER_SIZE_BYTES as u64 + ksz as u64,
                    record.header.timestamp,
                );

                if let Some(old_val) = old {
                    storage_tracker
                        .remove_bytes(header_and_key_size as u64 + old_val.value_size as u64); // mark old
                    // value dead
                }
                storage_tracker
                    .write_bytes(header_and_key_size as u64 + record.header.value_size as u64); // mark
                // the new value as active
            }
            DATAFILE_RECORD_TYPE_TOMBSTONE => {
                let old = self.keydir.write().unwrap().remove(&record.key);

                storage_tracker.write_bytes(header_and_key_size as u64); // mark the tombstone written
                storage_tracker.remove_bytes(header_and_key_size as u64); // mark the tombstone dead
                // on arrival
                if let Some(old_val) = old {
                    storage_tracker
                        .remove_bytes(header_and_key_size as u64 + old_val.value_size as u64); // mark old
                    // value dead
                }
            }
            _ => return Err(DatafileError::UnexpectedDatafileRecordType),
        }

        Ok(())
    }

    // High level writer switchover orchestrator, handles merge triggers
    fn switch_writer_datafile(&self, writer: &mut Datafile) -> Result<(), MergeError> {
        if self
            .storage_tracker
            .lock()
            .unwrap()
            .should_merge(&self.options)
        {
            self.perform_merge(writer)?; // Will also provide a fresh new writer
            return Ok(());
        }

        self.rotate_writer(writer)?;
        Ok(())
    }

    // Actual implementation of writer switchover
    fn rotate_writer(&self, writer: &mut Datafile) -> Result<(), DatafileError> {
        writer.flush()?;
        writer.demote_to_reader();
        writer.sync(&SyncMode::SyncAll)?; // Force sync to persist

        let new_writer =
            Datafile::new_writer(&self.dir, writer.id + 1, &self.options.datafile_extension)?;

        let old_writer = std::mem::replace(&mut *writer, new_writer);
        self.writer_id
            .store(writer.id, std::sync::atomic::Ordering::Release);

        self.readers
            .lock()
            .unwrap()
            .put(old_writer.id, Arc::new(old_writer));

        Ok(())
    }

    pub(crate) fn merge(&self) -> Result<(), BitcaskError> {
        let mut writer = self.writer.lock().unwrap();

        self.perform_merge(&mut writer)
            .map_err(BitcaskError::FatalMergeFailure)
    }

    fn perform_merge(&self, writer: &mut Datafile) -> Result<(), MergeError> {
        self.storage_tracker
            .lock()
            .unwrap()
            .disable_tracking_io_traffic();

        let merge = self.compact_datafiles(writer);

        let mut storage_tracker = self.storage_tracker.lock().unwrap();
        storage_tracker.enable_tracking_io_traffic();

        if merge.is_ok() {
            storage_tracker.reset_bytes();
            storage_tracker.write_bytes(self.keydir.read().unwrap().get_live_log_size());
        }

        merge
    }

    fn compact_datafiles(&self, writer: &mut Datafile) -> Result<(), MergeError> {
        self.rotate_writer(writer)?; // Seal the latest writer

        let writer_id = writer.id;
        let candidate_ids = self.merge_candidate_ids(writer_id)?;

        let mut hf = Hintfile::new_writer(&self.dir, writer_id, &self.options.hintfile_extension)?;

        for &id in &candidate_ids {
            self.compact_datafile(id, writer, &mut hf)?;
        }

        hf.flush()?; // Flush the buffer
        hf.sync()?; // Sync the final hintfile
        self.remove_old_readers_post_merge(&candidate_ids)?;
        self.rotate_writer(writer)?; // Seal the last writer that stores merged
        // records to avoid inconsistencies between hintfile and datafile
        Ok(())
    }

    fn merge_candidate_ids(&self, writer_id: u32) -> Result<Vec<u32>, MergeError> {
        let ids = BitcaskStartupHandler::parse_db_datafile_ids_at_path(
            &self.dir,
            &self.options.datafile_extension,
        )
        .map_err(|e| match e {
            OpenError::InvalidDBPath(io) => MergeError::DirectoryScan(io),
            other => MergeError::DirectoryScan(std::io::Error::other(other.to_string())),
        })?;

        Ok(ids.into_iter().filter(|&id| id < writer_id).collect())
    }

    fn compact_datafile(
        &self,
        id: u32,
        writer: &mut Datafile,
        hf: &mut Hintfile,
    ) -> Result<(), MergeError> {
        let df = Datafile::open_reader(&self.dir, id, &self.options.datafile_extension)?;
        let mut iter = DatafileRecordIterator::new(&df)?;

        while let Some(df_entry) = iter.next_record()? {
            if df_entry.header.record_type == DATAFILE_RECORD_TYPE_NORMAL {
                let keydir_entry = self.keydir.read().unwrap().get(&df_entry.key).copied();
                if let Some(keydir_entry) = keydir_entry
                    && keydir_entry.file_id == id
                    && keydir_entry.value_offset == df_entry.value_offset
                {
                    // Positive outcome - extract value
                    let mut hasher = df_entry.header.crc();
                    hasher.update(&df_entry.key);

                    // TODO: Possibly stream-write the value for hasher and write
                    // No need to stream the value in chunks as we need it allocated to perform the
                    // new write anyway
                    let value = iter
                        .next_value(keydir_entry.value_size as usize)?
                        .ok_or(MergeError::Datafile(DatafileError::TornRecord))?;

                    hasher.update(&value);

                    if hasher.finalize() != df_entry.header.crc {
                        return Err(MergeError::Datafile(DatafileError::CrcMismatch));
                    }

                    let record = DatafileRecord::with_timestamp(
                        &df_entry.key,
                        &value,
                        df_entry.header.timestamp,
                    )?;

                    let record_start_offset =
                        self.write_datafile_record(&record, writer)
                            .map_err(|e| match e {
                                PutError::Datafile(df_err) => MergeError::Datafile(df_err),
                                PutError::AutoMerge(merge_err) => merge_err,
                                PutError::RecordLargerThanMaxDatafileSize(_) => unreachable!(),
                            })?; // We want to keep original timestamp
                    self.write_hintfile_record(hf, writer.id, &record, record_start_offset)?;
                } else {
                    // Otherwise we skip the value as it is either dead or the tombstone itself
                    iter.skip_next(df_entry.header.value_size as usize)?;
                }
            }
        }

        Ok(())
    }

    fn write_hintfile_record(
        &self,
        hf: &mut Hintfile,
        writer_id: u32,
        record: &DatafileRecord,
        record_start_offset: u64,
    ) -> Result<(), MergeError> {
        // When a writer switchover has occured
        if hf.id != writer_id {
            let new = Hintfile::new_writer(&self.dir, writer_id, &self.options.hintfile_extension)?;

            let mut old = std::mem::replace(hf, new);
            old.flush()?; // Flush the buffer
            old.sync()?; // Sync before releasing the handle
            drop(old); // Seal old hintfile
        }

        let value_offset =
            record_start_offset + DATAFILE_HEADER_SIZE_BYTES as u64 + record.header.key_size as u64;

        hf.append_hint_record(record, value_offset)?;

        Ok(())
    }

    fn remove_old_readers_post_merge(&self, ids: &[u32]) -> Result<(), MergeError> {
        // Block any reads during removal of old datafiles
        let _keydir = self.keydir.write().unwrap();

        for &id in ids {
            self.readers.lock().unwrap().pop(&id); // Evict the file from LRU

            // Remove the reader datafile from filesystem
            fs::remove_file(Datafile::generate_filename(
                &self.dir,
                id,
                &self.options.datafile_extension,
            ))
            .map_err(MergeError::FileCleanup)?;

            // Remove the reader's hintfile from filesystem
            let hf = Datafile::generate_filename(
                // Remove the file from file system
                &self.dir,
                id,
                &self.options.hintfile_extension,
            );
            if hf.exists() {
                fs::remove_file(hf).map_err(MergeError::FileCleanup)?;
            }
        }

        Ok(())
    }
}

pub(crate) struct BitcaskRecoveryHandler;

#[hotpath::measure_all]
impl BitcaskRecoveryHandler {
    pub(crate) fn reopen_reader_for_recovery(
        dir: &Path,
        options: &BitcaskOptions,
        df: Datafile,
    ) -> Result<Datafile, DatafileError> {
        let id = df.id;
        let offset = df.offset;
        drop(df);
        Datafile::open_reader_for_recovery(dir, id, offset, &options.datafile_extension)
    }

    pub(crate) fn recover_post_df_replay_failure(
        dir: &Path,
        options: &BitcaskOptions,
        mut df: Datafile,
        error: DatafileError,
        is_last: bool,
    ) -> Result<Datafile, OpenError> {
        match options.recovery_mode {
            // Recover only the last datafile
            RecoveryMode::TolerateCorruptedTailRecords => {
                if !is_last {
                    // Failure in non-last datafiles is fatal
                    return Err(OpenError::FatalDatafileReplay(error));
                }

                df = Self::reopen_reader_for_recovery(dir, options, df)
                    .map_err(OpenError::FatalDatafileReplay)?;
                match error {
                    DatafileError::TornRecord
                    | DatafileError::PayloadSizeGreaterThanDatafile { .. }
                    | DatafileError::UndersizedDatafileHeader(_)
                    | DatafileError::CrcMismatch => {
                        df.truncate(df.offset)
                            .map_err(OpenError::FatalDatafileReplay)?;
                    }
                    e => return Err(OpenError::FatalDatafileReplay(e)),
                }
                Ok(df)
            }
            RecoveryMode::AbsoluteConsistency => Err(OpenError::FatalDatafileReplay(error)),
        }
    }

    // TODO: Implement handling for TolerateCorruptedTailRecords
    pub(crate) fn recover_post_hf_replay_failure(
        e: HintfileError,
        options: &BitcaskOptions,
    ) -> Result<(), OpenError> {
        match options.recovery_mode {
            RecoveryMode::AbsoluteConsistency => Err(OpenError::FatalHintfileReplay(e)),
            RecoveryMode::TolerateCorruptedTailRecords => Ok(()),
        }
    }
}

pub(crate) struct BitcaskStartupHandler;

#[hotpath::measure_all]
impl BitcaskStartupHandler {
    pub(crate) fn reopen_datafile_as_writer(
        dir: &Path,
        options: &BitcaskOptions,
        df: Datafile,
    ) -> Result<Datafile, DatafileError> {
        let id = df.id;
        let offset = df.file_size()?;
        drop(df);
        Datafile::open_datafile_as_writer(dir, id, offset, &options.datafile_extension)
    }

    pub(crate) fn parse_db_datafile_ids_at_path(
        dir: &Path,
        datafile_extension: &str,
    ) -> Result<Vec<u32>, OpenError> {
        if !dir.exists() {
            std::fs::create_dir_all(dir).map_err(OpenError::InvalidDBPath)?;
        }

        if !dir.is_dir() {
            return Err(OpenError::PathIsFile);
        }

        let mut ids: Vec<u32> = Vec::new();
        for file in read_dir(dir).map_err(OpenError::InvalidDBPath)? {
            let file = file.map_err(OpenError::InvalidDBPath)?;
            let filename = file
                .file_name()
                .into_string()
                .map_err(|_| OpenError::FilenameConversion)?;

            if let Some((name, ext)) = filename.rsplit_once('.') {
                if ext == datafile_extension {
                    match name.parse::<u32>() {
                        Ok(res) => ids.push(res),
                        Err(_) => continue,
                    }
                }
            } else {
                continue;
            }
        }
        ids.sort();

        Ok(ids)
    }

    // TODO: Increment merge tracker inside replay
    pub(crate) fn replay_and_init_readers(
        dir: &Path,
        options: &BitcaskOptions,
        keydir: &mut KeyDir,
        readers: &mut LruCache<u32, Arc<Datafile>>,
    ) -> Result<(Option<Datafile>, u64), OpenError> {
        let df_ids = Self::parse_db_datafile_ids_at_path(dir, &options.datafile_extension)?;
        let last = df_ids.last().cloned();
        let mut total_df_size = 0u64;

        for id in df_ids {
            let is_last = Some(id) == last;
            let mut df = Datafile::open_reader(dir, id, &options.datafile_extension)
                .map_err(OpenError::FatalDatafileReplay)?;

            let hint_count = match Hintfile::open_if_exists(dir, id, &options.hintfile_extension)
                .map_err(OpenError::FatalHintfileReplay)?
            {
                Some(hf) => match hf.replay(keydir) {
                    Ok(n) => n,
                    Err(e) => {
                        BitcaskRecoveryHandler::recover_post_hf_replay_failure(e, options)?;
                        0
                    }
                },
                None => 0,
            };

            if hint_count == 0
                && let Err(e) = df.replay(keydir)
            {
                df = BitcaskRecoveryHandler::recover_post_df_replay_failure(
                    dir, options, df, e, is_last,
                )?;
            }

            total_df_size += df.file_size().map_err(OpenError::FatalDatafileReplay)?;

            if is_last {
                return Ok((Some(df), total_df_size));
            }

            readers.put(id, Arc::new(df));
        }

        Ok((None, total_df_size))
    }

    pub(crate) fn create_writer_with_options(
        dir: &Path,
        options: &BitcaskOptions,
        latest_datafile: Option<Datafile>,
        readers: &mut LruCache<u32, Arc<Datafile>>,
    ) -> Result<Datafile, OpenError> {
        if let Some(df) = latest_datafile {
            if options.reuse_old_writer_on_startup
                || df.file_size().map_err(OpenError::WriterInit)? == 0
            {
                return Self::reopen_datafile_as_writer(dir, options, df)
                    .map_err(OpenError::WriterInit);
            }

            let id = df.id;
            readers.put(id, Arc::new(df));

            return Datafile::new_writer(dir, id + 1, &options.datafile_extension)
                .map_err(OpenError::WriterInit);
        }

        Datafile::new_writer(dir, 1, &options.datafile_extension).map_err(OpenError::WriterInit)
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
        sync::Arc,
    };

    use lru::LruCache;
    use tempfile::TempDir;

    use crate::{
        datafile::Datafile,
        engine::{BitcaskInstance, BitcaskStartupHandler},
        error::{BitcaskError, DatafileError, GetError, OpenError, PutError},
        hint::{HINTFILE_RECORD_HEADER_SIZE_BYTES, Hintfile},
        keydir::{KeyDir, KeyDirEntry},
        options::{BitcaskOptions, DATAFILE_HEADER_SIZE_BYTES, DatafileMaxSize},
        record::DatafileRecord,
    };

    pub(crate) fn init_test() -> (TempDir, BitcaskOptions, BitcaskInstance) {
        let temp_dir = tempfile::tempdir().unwrap();
        let options = BitcaskOptions::default();
        let db = BitcaskInstance::open(temp_dir.path(), BitcaskOptions::default().clone()).unwrap();

        (temp_dir, options, db)
    }

    pub(crate) fn keydir_entry_for(
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

    pub(crate) fn datafile_record_for_string_u32(key: &str, value: u32) -> DatafileRecord {
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

    fn replay_env(_dir: &Path, options: &BitcaskOptions) -> (KeyDir, LruCache<u32, Arc<Datafile>>) {
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

            {
                let writer = db.writer.lock().unwrap();

                assert_eq!(writer.id, 1);
                assert_eq!(writer.offset, 0);
                assert!(writer.is_writer);
            }

            assert!(db.readers.lock().unwrap().is_empty());
            assert!(db.keydir.read().unwrap().is_empty());
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
                let db = BitcaskInstance::open(temp_dir.path(), BitcaskOptions::default()).unwrap();

                // 28B per entry - 2 readers and 1 writer to fit 1000 entries with 10K Datafile max length
                for i in 0..RECORD_COUNT as u32 {
                    db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
                }

                {
                    let writer = db.writer.lock().unwrap();

                    assert_eq!(writer.id, fratio.ceil() as u32);

                    assert!(if fratio.round() == fratio {
                        writer.offset == 0
                    } else {
                        writer.offset != 0
                    });
                }

                assert_eq!(db.readers.lock().unwrap().len(), fratio.floor() as usize);
                assert_eq!(db.keydir.read().unwrap().len(), RECORD_COUNT);
            } // File handles should be released at this point

            {
                let db = BitcaskInstance::open(temp_dir.path(), BitcaskOptions::default()).unwrap();

                {
                    let writer = db.writer.lock().unwrap();
                    assert_eq!(writer.id, fratio.ceil() as u32 + 1); // New writer should be created
                    assert_eq!(writer.offset, 0);
                }

                assert_eq!(db.readers.lock().unwrap().len(), fratio.ceil() as usize);
                assert_eq!(db.keydir.read().unwrap().len(), RECORD_COUNT);

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
            let (_, _, db) = init_test();

            let key = "hello world";
            let value: u32 = 2;

            let datafile_record = datafile_record_for_string_u32(key, value);
            let expected = keydir_entry_for(&datafile_record, db.writer.lock().unwrap().id, 0);

            assert!(db.put(key.as_bytes(), &value.to_be_bytes()).is_ok());
            db.writer.lock().unwrap().flush().unwrap();

            let mut expected_bytes = Vec::new();
            Datafile::write_record_to(&mut expected_bytes, &datafile_record).unwrap();
            let read_bytes = db
                .writer
                .lock()
                .unwrap()
                .read_bytes(0, datafile_record.size().unwrap())
                .unwrap();
            assert_eq!(&read_bytes[0..1], &expected_bytes[0..1]);
            assert_eq!(&read_bytes[13..], &expected_bytes[13..]);

            {
                let keydir = db.keydir.read().unwrap();
                let keydir_written = keydir.get(key.as_bytes()).unwrap();
                assert_eq!(keydir.len(), 1);
                assert_eq!(keydir_written.file_id, expected.file_id);
                assert_eq!(keydir_written.value_size, expected.value_size);
                assert_eq!(keydir_written.value_offset, expected.value_offset);
            }

            assert_eq!(
                db.writer.lock().unwrap().offset,
                datafile_record.size().unwrap() as u64
            );
        }

        #[test]
        fn switches_writer_when_full() {
            #[allow(unused_variables)]
            let (temp_dir, options, db) = init_test();

            {
                let mut writer = db.writer.lock().unwrap();

                writer.offset = options.datafile_max_size.get_size() as u64;

                assert_eq!(writer.id, 1);
                assert!(writer.is_writer);
                assert_eq!(writer.offset, options.datafile_max_size.get_size() as u64);
            }

            let key = "hello world";
            let value: u32 = 2;

            assert!(db.put(key.as_bytes(), &value.to_be_bytes()).is_ok());

            // Make sure new writer is created with proper id, role and offset
            {
                let writer = db.writer.lock().unwrap();
                assert_eq!(writer.id, 2);
                assert!(writer.is_writer);
                assert_eq!(
                    writer.offset,
                    datafile_record_for_string_u32(key, value).size().unwrap() as u64
                );
            }

            // Verify the new writer is properly handled on get syncpath
            assert_eq!(
                db.get(key.as_bytes()).unwrap().unwrap(),
                Box::from(value.to_be_bytes())
            )
        }

        #[test]
        fn commits_oversized_entry_on_empty_writer_with_soft_cap() {
            let temp_dir = tempfile::tempdir().unwrap();

            let db = BitcaskInstance::open(
                temp_dir.path(),
                BitcaskOptions::default()
                    .datafile_max_size(crate::options::DatafileMaxSize::SoftCappedBytes(40)),
            )
            .unwrap();

            assert!(db.put(&0u128.to_be_bytes(), &0u128.to_be_bytes()).is_ok());
            {
                let writer = db.writer.lock().unwrap();
                assert_eq!(writer.id, 1); // Due to current writer being empty, we expect it to have at
                // least one entry
                assert_eq!(writer.offset, 53);
            }

            assert!(db.put(&1u128.to_be_bytes(), &1u128.to_be_bytes()).is_ok());
            {
                let writer = db.writer.lock().unwrap();
                assert_eq!(writer.id, 2); // First writer is now oversized, so we switchover and commit
                // on the new writer
                assert_eq!(writer.offset, 53);
            }
        }

        #[test]
        fn rejects_oversized_entry_on_empty_writer_with_hard_cap() {
            let temp_dir = tempfile::tempdir().unwrap();

            let db = BitcaskInstance::open(
                temp_dir.path(),
                BitcaskOptions::default()
                    .datafile_max_size(crate::options::DatafileMaxSize::HardCappedBytes(40)),
            )
            .unwrap();

            assert!(matches!(
                db.put(&0u128.to_be_bytes(), &0u128.to_be_bytes()),
                Err(BitcaskError::PutFailure(
                    PutError::RecordLargerThanMaxDatafileSize(53usize)
                ))
            ));
            assert_eq!(db.keydir.read().unwrap().len(), 0);
            assert_eq!(db.writer.lock().unwrap().id, 1); // No writer switchover should occur

            // First write should go through normally
            assert!(db.put(&0u32.to_be_bytes(), &0u32.to_be_bytes()).is_ok());
            assert_eq!(db.keydir.read().unwrap().len(), 1);
            {
                let writer = db.writer.lock().unwrap();
                assert_eq!(writer.id, 1);
                assert_eq!(writer.offset, 29);
            }

            // The writer overflows, so we should switchover and write
            assert!(db.put(&1u32.to_be_bytes(), &1u32.to_be_bytes()).is_ok());
            assert_eq!(db.keydir.read().unwrap().len(), 2);
            {
                let writer = db.writer.lock().unwrap();
                assert_eq!(writer.id, 2);
                assert_eq!(writer.offset, 29);
            }
        }
    }

    mod get {
        use super::*;

        #[test]
        fn returns_written_value() {
            let (_, _, db) = init_test();

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
            let (temp_dir, _, db) = init_test();

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
            db.keydir.write().unwrap().put(
                record.key,
                3,
                record.header.value_size,
                value_offset,
                record.header.timestamp,
            );

            // Make sure the datafile is not known
            assert_eq!(db.readers.lock().unwrap().len(), 0);

            assert_eq!(
                db.get(key.as_bytes()).unwrap().unwrap(),
                Box::from(value.to_be_bytes())
            );
            // Verify the missed reader has been discovered
            assert_eq!(db.readers.lock().unwrap().len(), 1);
        }

        #[test]
        fn get_missing_datafile_returns_error() {
            let (_, _, db) = init_test();
            db.keydir
                .write()
                .unwrap()
                .put(Box::from(b"missing_key" as &[u8]), 999, 10, 0, 12345);
            let res = db.get(b"missing_key");
            assert!(matches!(
                res,
                Err(BitcaskError::GetFailure(GetError::DatafileMissing(999)))
            ));
        }
    }

    mod remove {
        use super::*;

        #[test]
        fn writes_tombstones() {
            let (_, _, db) = init_test();

            const ENTRY_COUNT: usize = 20;
            const ENTRY_SIZE: usize = 29;
            // Tombstone log is 21 + 4 = 25 bytes
            const DATAFILE_TOMBSTONE_SIZE: usize = 25;

            for i in 0..ENTRY_COUNT as u32 {
                db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
            }

            assert_eq!(
                db.writer.lock().unwrap().offset,
                (ENTRY_SIZE * ENTRY_COUNT) as u64
            );

            let delete_count = ENTRY_COUNT.div_ceil(2);
            for i in 0..delete_count as u32 {
                db.remove(&i.to_be_bytes()).unwrap();
            }
            assert_eq!(db.keydir.read().unwrap().len(), ENTRY_COUNT - delete_count);

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
                db.writer.lock().unwrap().offset,
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
                let db = BitcaskInstance::open(temp_dir.path(), BitcaskOptions::default()).unwrap();

                for i in 0..RECORD_COUNT as u32 {
                    db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
                }

                for i in 0..DELETES_COUNT as u32 {
                    db.remove(&i.to_be_bytes()).unwrap();
                }

                assert_eq!(
                    db.keydir.read().unwrap().len(),
                    RECORD_COUNT - DELETES_COUNT
                );
            } // File handles should be released at this point

            {
                let db = BitcaskInstance::open(temp_dir.path(), BitcaskOptions::default()).unwrap();

                assert_eq!(
                    db.keydir.read().unwrap().len(),
                    RECORD_COUNT - DELETES_COUNT
                );
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

            let (latest_datafile, _) = BitcaskStartupHandler::replay_and_init_readers(
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

            let (latest_datafile, _) = BitcaskStartupHandler::replay_and_init_readers(
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
                Err(OpenError::FatalDatafileReplay(
                    DatafileError::DatafileNotFound(_)
                ))
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

            let (latest_datafile, _) = BitcaskStartupHandler::replay_and_init_readers(
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
                Err(OpenError::FatalDatafileReplay(
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
                let db = BitcaskInstance::open(temp_dir.path(), options.clone()).unwrap();

                db.put(b"1", b"1").unwrap();
                db.put(b"2", b"2").unwrap();

                Hintfile::new_writer(
                    temp_dir.path(),
                    db.writer.lock().unwrap().id,
                    &options.hintfile_extension,
                )
                .unwrap();

                assert_eq!(db.storage_tracker.lock().unwrap().bytes_written, (23 * 2));
            }

            // Verify the database has the 2 entries even though the hintfile was empty
            // Perform merge (append 2 records from above to hintfile), reopen hintfile and add a fake record
            {
                let db = BitcaskInstance::open(temp_dir.path(), options.clone()).unwrap();

                assert_eq!(db.storage_tracker.lock().unwrap().bytes_written, (23 * 2));
                assert_eq!(db.writer.lock().unwrap().id, 2);
                assert_eq!(db.len(), 2);

                db.merge().unwrap(); // Writes the values to the hintfile

                let mut hf = Hintfile::new_writer(
                    temp_dir.path(),
                    db.writer.lock().unwrap().id,
                    &options.hintfile_extension,
                )
                .unwrap();

                hf.append_hint_record(&DatafileRecord::new(b"3", b"3").unwrap(), 0)
                    .unwrap();
            }

            // Reopen database and verify all records were loaded from hintfile (fake one should be
            // present)
            let db = BitcaskInstance::open(temp_dir.path(), options.clone()).unwrap();
            assert_eq!(db.writer.lock().unwrap().id, 4);
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

        #[test]
        fn replay_restores_merge_tracker_state() {
            let options = BitcaskOptions::default();
            let temp_dir = tempfile::tempdir().unwrap();

            const ENTRY_COUNT: usize = 100;
            const DEAD_ENTRY_COUNT: usize = 50;
            const TOMBSTONED_ENTRY_COUNT: usize = 10;
            const ENTRY_SIZE: usize = 29;

            {
                let db = BitcaskInstance::open(temp_dir.path(), options.clone()).unwrap();

                for i in 0..ENTRY_COUNT as u32 {
                    db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
                }

                assert_eq!(
                    db.storage_tracker.lock().unwrap().bytes_written,
                    (ENTRY_COUNT * ENTRY_SIZE) as u64
                );

                for i in 0..DEAD_ENTRY_COUNT as u32 {
                    db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
                }

                {
                    let storage_tracker = db.storage_tracker.lock().unwrap();
                    assert_eq!(
                        storage_tracker.bytes_written,
                        (ENTRY_COUNT * ENTRY_SIZE) as u64 + (DEAD_ENTRY_COUNT * ENTRY_SIZE) as u64
                    );

                    assert_eq!(
                        storage_tracker.dead_bytes_written,
                        (DEAD_ENTRY_COUNT * ENTRY_SIZE) as u64
                    );
                }

                for i in DEAD_ENTRY_COUNT as u32..(DEAD_ENTRY_COUNT + TOMBSTONED_ENTRY_COUNT) as u32
                {
                    db.remove(&i.to_be_bytes()).unwrap();
                }

                {
                    let storage_tracker = db.storage_tracker.lock().unwrap();
                    assert_eq!(
                        storage_tracker.bytes_written,
                        (ENTRY_COUNT * ENTRY_SIZE) as u64
                            + (DEAD_ENTRY_COUNT * ENTRY_SIZE) as u64
                            + (TOMBSTONED_ENTRY_COUNT * (DATAFILE_HEADER_SIZE_BYTES + 4)) as u64
                    );
                    assert_eq!(
                        storage_tracker.dead_bytes_written,
                        (DEAD_ENTRY_COUNT * ENTRY_SIZE) as u64
                            + (TOMBSTONED_ENTRY_COUNT * ENTRY_SIZE) as u64
                            + (TOMBSTONED_ENTRY_COUNT * (DATAFILE_HEADER_SIZE_BYTES + 4)) as u64
                    );
                }
            }

            {
                let db = BitcaskInstance::open(temp_dir.path(), options).unwrap();
                let storage_tracker = db.storage_tracker.lock().unwrap();
                assert_eq!(
                    storage_tracker.bytes_written,
                    (DEAD_ENTRY_COUNT * ENTRY_SIZE + ENTRY_COUNT * ENTRY_SIZE) as u64
                        + (TOMBSTONED_ENTRY_COUNT * (DATAFILE_HEADER_SIZE_BYTES + 4)) as u64
                );
                assert_eq!(
                    storage_tracker.dead_bytes_written,
                    (DEAD_ENTRY_COUNT * ENTRY_SIZE) as u64
                        + (TOMBSTONED_ENTRY_COUNT * ENTRY_SIZE) as u64
                        + (TOMBSTONED_ENTRY_COUNT * (DATAFILE_HEADER_SIZE_BYTES + 4)) as u64
                );
            }
        }
    }

    #[test]
    fn commit_keydir_write() {
        let (_, _, db) = init_test();

        let datafile_record = datafile_record_for_string_u32("hello world", 2);
        let expected = keydir_entry_for(&datafile_record, db.writer.lock().unwrap().id, 0);

        assert!(
            db.commit_keydir_write(&datafile_record.clone(), db.writer.lock().unwrap().id, 0)
                .is_ok()
        );
        {
            let keydir = db.keydir.read().unwrap();
            assert_eq!(keydir.len(), 1);
            assert_eq!(keydir.get(&datafile_record.key).unwrap(), &expected);
        }

        let datafile_record = datafile_record_for_string_u32("byeworld", 2);
        let expected = keydir_entry_for(&datafile_record, db.writer.lock().unwrap().id, 35);

        assert!(
            db.commit_keydir_write(&datafile_record.clone(), db.writer.lock().unwrap().id, 35)
                .is_ok()
        );
        {
            let keydir = db.keydir.read().unwrap();
            assert_eq!(keydir.len(), 2);
            assert_eq!(keydir.get(&datafile_record.key).unwrap(), &expected);
        }

        let datafile_record = datafile_record_for_string_u32("byeworld", 3);
        let expected = keydir_entry_for(&datafile_record, db.writer.lock().unwrap().id, 70);

        assert!(
            db.commit_keydir_write(&datafile_record.clone(), db.writer.lock().unwrap().id, 70)
                .is_ok()
        );
        {
            let keydir = db.keydir.read().unwrap();
            assert_ne!(keydir.len(), 3);
            assert_eq!(keydir.get(&datafile_record.key).unwrap(), &expected);
        }
    }

    #[test]
    fn switch_writer_datafile() {
        #[allow(unused_variables)]
        let (temp_dir, _, db) = init_test();

        {
            let writer = db.writer.lock().unwrap();
            assert_eq!(writer.id, 1);
            assert!(writer.is_writer);
            assert_eq!(writer.offset, 0);
        }

        assert_eq!(db.readers.lock().unwrap().len(), 0);
        {
            assert!(
                db.switch_writer_datafile(&mut db.writer.lock().unwrap())
                    .is_ok()
            );
        }

        // Make sure old writer is demoted and added to LRU cache of readers
        {
            let mut readers = db.readers.lock().unwrap();
            assert_eq!(readers.len(), 1);
            let old = readers.get(&1u32).unwrap();

            assert_eq!(old.id, 1);
            assert!(!old.is_writer);
        }

        // Make sure new writer is created with proper id, role and offset
        let mut writer = db.writer.lock().unwrap();
        assert_eq!(writer.id, 2);
        assert!(writer.is_writer);
        assert_eq!(writer.offset, 0);

        // Make sure the new datafile accepts writes
        let key = "hello world";
        let value: u32 = 2;

        let datafile_record = datafile_record_for_string_u32(key, value);
        assert!(writer.append_record(&datafile_record).is_ok());
        assert_eq!(writer.offset, datafile_record.size().unwrap() as u64);
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
            let db = BitcaskInstance::open(temp_dir.path(), options.clone()).unwrap();

            // 29B Entries
            for i in 0..ENTRY_COUNT as u32 {
                db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
            }

            assert_eq!(db.keydir.read().unwrap().len(), ENTRY_COUNT);

            assert_eq!(
                db.writer.lock().unwrap().id,
                entry_total_size.div_ceil(options.datafile_max_size.get_size()) as u32
            );

            assert_eq!(
                db.readers.lock().unwrap().len(),
                entry_total_size.div(options.datafile_max_size.get_size())
            );
        }

        {
            let db = BitcaskInstance::open(temp_dir.path(), options.clone()).unwrap();

            assert_eq!(db.keydir.read().unwrap().len(), ENTRY_COUNT);

            // Same writer ID should be used
            {
                let writer = db.writer.lock().unwrap();
                assert_eq!(
                    writer.id,
                    entry_total_size.div_ceil(options.datafile_max_size.get_size()) as u32
                );

                assert_eq!(
                    writer.offset
                        + db.readers
                            .lock()
                            .unwrap()
                            .iter()
                            .map(|(_, reader)| reader.offset)
                            .sum::<u64>(),
                    entry_total_size as u64
                );
            }

            assert_eq!(
                db.readers.lock().unwrap().len(),
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
                db.writer.lock().unwrap().offset
                    + db.readers
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|(_, reader)| reader.offset)
                        .sum::<u64>(),
                (entry_total_size + ENTRY_SIZE) as u64
            );
        }
    }

    #[test]
    fn write_hintfile_record() {
        let (temp_dir, options, db) = init_test();

        let mut hf = Hintfile::new_writer(
            temp_dir.path(),
            db.writer.lock().unwrap().id,
            &options.hintfile_extension,
        )
        .unwrap();

        const HINT_ENTRY_SIZE: usize = HINTFILE_RECORD_HEADER_SIZE_BYTES + 1;

        assert_eq!(hf.id, 1);
        let record = DatafileRecord::new(b"1", b"1").unwrap();
        assert!(
            db.write_hintfile_record(&mut hf, db.writer.lock().unwrap().id, &record, 0)
                .is_ok()
        );
        hf.flush().unwrap();
        assert_eq!(hf.file_size().unwrap(), HINT_ENTRY_SIZE as u64);

        {
            db.switch_writer_datafile(&mut db.writer.lock().unwrap())
                .unwrap();
        }
        assert_eq!(hf.id, 1);
        let record = DatafileRecord::new(b"2", b"2").unwrap();
        assert!(
            db.write_hintfile_record(&mut hf, db.writer.lock().unwrap().id, &record, 0)
                .is_ok()
        );
        hf.flush().unwrap();
        assert_eq!(hf.id, 2);
        assert_eq!(hf.file_size().unwrap(), HINT_ENTRY_SIZE as u64);
    }

    mod merge {
        use crate::hint::{HINTFILE_RECORD_HEADER_SIZE_BYTES, Hintfile};

        use super::*;

        #[test]
        fn merge() {
            let (temp_dir, options, db) = init_test();

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

            assert_eq!(
                db.writer.lock().unwrap().id,
                datafiles_to_house_payload as u32
            );
            assert_eq!(db.len(), ENTRY_COUNT);

            for i in 0..ENTRY_COUNT as u32 {
                db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
            }
            assert_eq!(
                db.writer.lock().unwrap().id,
                datafiles_to_house_dead_payload as u32
            );
            assert_eq!(db.len(), ENTRY_COUNT);

            assert!(db.merge().is_ok());

            assert_eq!(
                db.writer.lock().unwrap().id,
                datafiles_to_house_dead_payload as u32 // All sealed during merge
                    + datafiles_to_house_payload as u32 // New files to house the data
                + 1 // Last sealed writer
            );
            assert_eq!(db.len(), ENTRY_COUNT); // All KV pairs should be still present
            assert_eq!(
                db.readers.lock().unwrap().len(),
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
            let last = db.writer.lock().unwrap().id - 1;
            {
                let readers = db.readers.lock().unwrap();

                for (&id, _) in readers.iter() {
                    let hf = Hintfile::open_if_exists(temp_dir.path(), id, "hint")
                        .unwrap()
                        .unwrap();

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
            }

            // Old writer used during merge has been sealed, so current one should be empty
            assert_eq!(
                Hintfile::new_writer(temp_dir.path(), db.writer.lock().unwrap().id, "hint")
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

            let db =
                BitcaskInstance::open(temp_dir.path(), BitcaskOptions::default().clone()).unwrap();

            for i in 0..ENTRY_COUNT as u32 {
                db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
            }

            {
                let storage_tracker = db.storage_tracker.lock().unwrap();
                assert_eq!(
                    storage_tracker.bytes_written,
                    (ENTRY_SIZE * ENTRY_COUNT) as u64
                );
                assert_eq!(storage_tracker.dead_bytes_written, 0u64);
            }

            for i in 0..ENTRY_COUNT as u32 {
                db.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
            }

            {
                let storage_tracker = db.storage_tracker.lock().unwrap();
                assert_eq!(
                    storage_tracker.bytes_written,
                    (2 * ENTRY_SIZE * ENTRY_COUNT) as u64
                );
                assert_eq!(
                    storage_tracker.dead_bytes_written,
                    (ENTRY_SIZE * ENTRY_COUNT) as u64
                );

                assert!(storage_tracker.should_merge(&options));
            }
        }
    }

    mod concurrency {
        use std::thread::{self};

        use crate::{options::KB, test_utils::*};

        use super::*;

        #[test]
        fn writes_are_served() {
            let thread_count: usize = 8;
            let op_count: usize = 10000;
            let thread_ops = op_count / thread_count;
            let (ksz, vsz) = (4, 64);

            let (_dir, _, db) = init_test();
            let db = Arc::new(db);

            let all_ops = provision_db_data(op_count, ksz, vsz, OperationType::Write);

            thread::scope(|s| {
                for chunk in all_ops.chunks(thread_ops) {
                    s.spawn(|| {
                        fill_db(&db, chunk);
                    });
                }
            });

            for op in &all_ops {
                assert_eq!(db.get(&op.key).unwrap().unwrap(), op.value);
            }
        }

        #[test]
        fn reads_are_served() {
            let thread_count: usize = 8;
            let op_count: usize = 10000;
            let thread_ops = op_count / thread_count;
            let (ksz, vsz) = (4, 64);

            let (_dir, _, db) = init_test();
            let db = Arc::new(db);

            let ops = provision_db_data(op_count, ksz, vsz, OperationType::Write);
            fill_db(&db, &ops);

            thread::scope(|s| {
                for chunk in ops.chunks(thread_ops) {
                    s.spawn(|| {
                        for op in chunk.iter().cloned() {
                            assert_eq!(db.get(&op.key).unwrap().unwrap(), op.value);
                        }
                    });
                }
            });
        }

        // TODO: Race between get() and merge() in between get's non-atomic read
        #[test]
        fn chaos() {
            const READ_PERCENTAGE: usize = 40;

            let thread_count: usize = 8;
            let op_count: usize = 10000;
            let thread_ops = op_count / thread_count;
            let (ksz, vsz) = (4, 64);

            let temp_dir = tempfile::tempdir().unwrap();
            let options = BitcaskOptions::default()
                .datafile_max_size(DatafileMaxSize::SoftCappedBytes(KB))
                .merge_dead_space_threshold_percent(50)
                .merge_dead_space_min_bytes(1);

            let db = Arc::new(BitcaskInstance::open(temp_dir.path(), options).unwrap());

            let read_count = (op_count * READ_PERCENTAGE) / 100;
            let write_count = op_count - read_count;

            let read_kv = provision_db_data(read_count, ksz, vsz, OperationType::Read);
            let write_kv = provision_db_data(write_count, ksz, vsz, OperationType::Write);

            fill_db(&db, &read_kv);

            let active_ops = merge_and_shuffle(read_kv, write_kv);

            thread::scope(|s| {
                for (i, ops) in active_ops.chunks(thread_ops).enumerate() {
                    if i == thread_count / 2 {
                        db.merge().unwrap();
                    }

                    s.spawn(|| {
                        perform_operations(&db, ops, thread_ops);
                    });
                }
            });
        }
    }

    mod misc {
        use crate::test_utils::{self, perform_operations};

        use super::*;

        #[test]
        fn contains() {
            let (_dir, _, db) = init_test();
            let (n, ksz, vsz) = (100, 4, 1);
            let ops = test_utils::provision_db_data(n, ksz, vsz, test_utils::OperationType::Write);

            perform_operations(&db, &ops, n);

            for op in ops {
                assert!(db.contains(&op.key));
            }
        }

        #[test]
        fn keys() {
            let (_dir, _, db) = init_test();
            let (n, ksz, vsz) = (100, 4, 1);
            let ops = test_utils::provision_db_data(n, ksz, vsz, test_utils::OperationType::Write);

            perform_operations(&db, &ops, n);

            let keys = db.keys();

            assert_eq!(keys.len(), ops.len());
            assert_eq!(keys.len(), db.keydir.read().unwrap().len());

            for key in db.keys() {
                assert!(db.contains(&key));
            }
        }

        #[test]
        fn drop_flushes_writer_buff_and_fsyncs() {
            let (dir, options, db) = init_test();
            let (n, ksz, vsz) = (4, 4, 1);
            let ops = test_utils::provision_db_data(n, ksz, vsz, test_utils::OperationType::Write);

            perform_operations(&db, &ops, n);

            drop(db);

            let db = BitcaskInstance::open(dir.path(), options).unwrap();
            assert_eq!(db.len(), n);
            for op in ops {
                assert_eq!(db.get(&op.key).unwrap().unwrap(), op.value);
            }
        }
    }
}
