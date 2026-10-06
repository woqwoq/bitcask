#[derive(Clone, PartialEq)]
pub enum SyncMode {
    EveryWrite,
    EveryNthWrite(usize),
    Never,
}

impl Default for SyncMode {
    fn default() -> Self {
        Self::Never
    }
}

#[derive(Clone, PartialEq)]
pub enum DatafileMaxSize {
    SoftCappedBytes(usize),
    HardCappedBytes(usize),
}

impl Default for DatafileMaxSize {
    fn default() -> Self {
        Self::SoftCappedBytes(10000)
    }
}

#[derive(Clone)]
pub struct BitcaskOptions {
    pub sync_mode: SyncMode,
    pub datafile_max_size: DatafileMaxSize,
    pub reuse_old_writer_on_startup: bool,
}

impl BitcaskOptions {
    pub fn sync_mode(mut self, new: SyncMode) -> Self {
        self.sync_mode = new;
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

    pub fn get_datafile_max_size(&self) -> usize {
        match self.datafile_max_size {
            DatafileMaxSize::SoftCappedBytes(n) => n,
            DatafileMaxSize::HardCappedBytes(n) => n,
        }
    }
}

impl Default for BitcaskOptions {
    fn default() -> Self {
        Self {
            sync_mode: SyncMode::default(),
            datafile_max_size: DatafileMaxSize::default(),
            reuse_old_writer_on_startup: false,
        }
    }
}
