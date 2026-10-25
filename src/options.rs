use crate::error::InvalidOptionError;

pub const KB: usize = 1024;
pub const MB: usize = KB * KB;
pub const GB: usize = KB * MB;

pub const DATAFILE_HEADER_SIZE_BYTES: usize = 21;

pub const DEFAULT_DATAFILE_EXTENSION: &str = "db";
pub const DEFAULT_HINTFILE_EXTENSION: &str = "hint";

pub const DEFAULT_LRU_CACHED_READERS_COUNT: usize = 10;
pub const DEFAULT_DATAFILE_SIZE_BYTES: usize = 10 * KB;

pub const DEFAULT_MERGE_DEAD_SPACE_PERCENT_THRESHOLD: u8 = 80;
pub const DEFAULT_MERGE_DEAD_SPACE_BYTE_THRESHOLD: u64 = (8 * KB) as u64;

pub const EXPECTED_KEYDIR_MAX_SIZE: usize = 0;

#[derive(Clone, PartialEq, Default)]
pub enum RecoveryMode {
    #[default]
    TolerateCorruptedTailRecords, // Last reader failure -> truncate until failure, or fatal
    AbsoluteConsistency, // Any failure -> fatal
}

#[derive(Clone, PartialEq, Default)]
pub enum SyncMode {
    SyncData,
    #[default]
    SyncAll,
}

#[derive(Clone, PartialEq, Default)]
pub enum SyncFrequency {
    EveryWrite,
    EveryNthWrite(usize),
    #[default]
    Never,
}

#[derive(Clone, PartialEq)]
pub enum DatafileMaxSize {
    SoftCappedBytes(usize),
    HardCappedBytes(usize),
}

impl DatafileMaxSize {
    pub fn get_size(&self) -> usize {
        match self {
            DatafileMaxSize::SoftCappedBytes(n) => *n,
            DatafileMaxSize::HardCappedBytes(n) => *n,
        }
    }
}

impl Default for DatafileMaxSize {
    fn default() -> Self {
        Self::SoftCappedBytes(DEFAULT_DATAFILE_SIZE_BYTES)
    }
}

#[derive(Clone)]
pub struct BitcaskOptions {
    pub sync_mode: SyncMode,
    pub recovery_mode: RecoveryMode,
    pub sync_frequency: SyncFrequency,
    pub datafile_max_size: DatafileMaxSize,
    pub merge_dead_space_threshold_percent: u8,
    pub merge_dead_space_threshold_bytes: u64,
    pub reuse_old_writer_on_startup: bool,
    pub datafile_extension: String,
    pub hintfile_extension: String,
    pub reader_lru_cache_size: usize,
    pub expected_keydir_max_size: usize,
}

impl BitcaskOptions {
    pub fn validate(&self) -> Result<(), InvalidOptionError> {
        if matches!(self.sync_frequency, SyncFrequency::EveryNthWrite(0)) {
            return Err(InvalidOptionError::NonZeroField(String::from(
                "sync_frequency",
            )));
        }

        if self.datafile_max_size.get_size() <= DATAFILE_HEADER_SIZE_BYTES {
            return Err(InvalidOptionError::GreaterThanField(
                String::from("datafile_max_size"),
                format!("{}", DATAFILE_HEADER_SIZE_BYTES),
            ));
        }

        if self.merge_dead_space_threshold_percent == 0 {
            return Err(InvalidOptionError::NonZeroField(String::from(
                "merge_dead_space_threshold_percent",
            )));
        }

        if self.merge_dead_space_threshold_bytes == 0 {
            return Err(InvalidOptionError::NonZeroField(String::from(
                "merge_dead_space_threshold_bytes",
            )));
        }

        if self.reader_lru_cache_size == 0 {
            return Err(InvalidOptionError::NonZeroField(String::from(
                "reader_lru_cache_size",
            )));
        }

        if self.datafile_extension.is_empty() {
            return Err(InvalidOptionError::NonEmptyField(String::from(
                "datafile_extension",
            )));
        }

        if self.hintfile_extension.is_empty() {
            return Err(InvalidOptionError::NonEmptyField(String::from(
                "hintfile_extension",
            )));
        }

        Ok(())
    }
    pub fn sync_mode(mut self, new: SyncMode) -> Self {
        self.sync_mode = new;
        self
    }

    pub fn sync_frequency(mut self, new: SyncFrequency) -> Self {
        if let SyncFrequency::EveryNthWrite(n) = new
            && n == 0
        {
            panic!("SyncFrequency::EveryNthWrite cannot be set to 0");
        }

        self.sync_frequency = new;
        self
    }

    pub fn datafile_max_size(mut self, new: DatafileMaxSize) -> Self {
        if new.get_size() <= DATAFILE_HEADER_SIZE_BYTES {
            panic!(
                "DatafileMaxSize has to be set to a value greater than {DATAFILE_HEADER_SIZE_BYTES}"
            )
        }

        self.datafile_max_size = new;
        self
    }

    pub fn merge_dead_space_threshold_percent(mut self, new: u8) -> Self {
        if new == 0 {
            panic!("merge_dead_space_threshold_percent cannot be set to 0")
        }

        self.merge_dead_space_threshold_percent = new;
        self
    }

    pub fn merge_dead_space_min_bytes(mut self, new: u64) -> Self {
        if new == 0 {
            panic!("merge_dead_space_min_bytes cannot be set to 0")
        }

        self.merge_dead_space_threshold_bytes = new;
        self
    }

    pub fn reuse_old_writer_on_startup(mut self, new: bool) -> Self {
        self.reuse_old_writer_on_startup = new;
        self
    }

    pub fn datafile_extension(mut self, new: &str) -> Self {
        if new.is_empty() {
            panic!("datafile_extension cannot be empty")
        }

        self.datafile_extension = new.to_string();
        self
    }

    pub fn hintfile_extension(mut self, new: &str) -> Self {
        if new.is_empty() {
            panic!("hintfile_extension cannot be empty")
        }

        self.hintfile_extension = new.to_string();
        self
    }

    pub fn reader_lru_cache_size(mut self, new: usize) -> Self {
        if new == 0 {
            panic!("reader_lru_cache_size cannot be set to 0")
        }

        self.reader_lru_cache_size = new;
        self
    }

    pub fn recovery_mode(mut self, new: RecoveryMode) -> Self {
        self.recovery_mode = new;
        self
    }

    pub fn expected_keydir_max_size(mut self, new: usize) -> Self {
        self.expected_keydir_max_size = new;
        self
    }
}

impl Default for BitcaskOptions {
    fn default() -> Self {
        Self {
            sync_mode: SyncMode::default(),
            recovery_mode: RecoveryMode::default(),
            sync_frequency: SyncFrequency::default(),
            datafile_max_size: DatafileMaxSize::default(),
            merge_dead_space_threshold_percent: DEFAULT_MERGE_DEAD_SPACE_PERCENT_THRESHOLD,
            merge_dead_space_threshold_bytes: DEFAULT_MERGE_DEAD_SPACE_BYTE_THRESHOLD,
            reuse_old_writer_on_startup: false,
            datafile_extension: DEFAULT_DATAFILE_EXTENSION.to_string(),
            hintfile_extension: DEFAULT_HINTFILE_EXTENSION.to_string(),
            reader_lru_cache_size: DEFAULT_LRU_CACHED_READERS_COUNT,
            expected_keydir_max_size: EXPECTED_KEYDIR_MAX_SIZE,
        }
    }
}
