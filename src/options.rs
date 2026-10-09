pub const DEFAULT_DATAFILE_EXTENSION: &str = "db";
pub const DEFAULT_LRU_CACHED_READERS_COUNT: usize = 10;

#[derive(Clone, PartialEq)]
pub enum RecoveryMode {
    TolerateCorruptedTailRecords, // Last reader failure -> truncate until failure, or fatal
    AbsoluteConsistency,          // Any failure -> fatal
}

impl Default for RecoveryMode {
    fn default() -> Self {
        Self::TolerateCorruptedTailRecords
    }
}

#[derive(Clone, PartialEq)]
pub enum SyncMode {
    SyncData,
    SyncAll,
}

impl Default for SyncMode {
    fn default() -> Self {
        Self::SyncAll
    }
}

#[derive(Clone, PartialEq)]
pub enum SyncFrequency {
    EveryWrite,
    EveryNthWrite(usize),
    Never,
}

impl Default for SyncFrequency {
    fn default() -> Self {
        Self::Never
    }
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
        Self::SoftCappedBytes(10000)
    }
}

#[derive(Clone)]
pub struct BitcaskOptions {
    pub sync_mode: SyncMode,
    pub recovery_mode: RecoveryMode,
    pub sync_frequency: SyncFrequency,
    pub datafile_max_size: DatafileMaxSize,
    pub reuse_old_writer_on_startup: bool,
    pub datafile_extension: String,
    pub reader_lru_cache_size: usize,
}

impl BitcaskOptions {
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
        self.datafile_max_size = new;
        self
    }
    pub fn reuse_old_writer_on_startup(mut self, new: bool) -> Self {
        self.reuse_old_writer_on_startup = new;
        self
    }
    pub fn datafile_extension(mut self, new: &str) -> Self {
        self.datafile_extension = new.to_string();
        self
    }
    pub fn reader_lru_cache_size(mut self, new: usize) -> Self {
        self.reader_lru_cache_size = new;
        self
    }
    pub fn recovery_mode(mut self, new: RecoveryMode) -> Self {
        self.recovery_mode = new;
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
            reuse_old_writer_on_startup: false,
            datafile_extension: DEFAULT_DATAFILE_EXTENSION.to_string(),
            reader_lru_cache_size: DEFAULT_LRU_CACHED_READERS_COUNT,
        }
    }
}
