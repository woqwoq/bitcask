#[derive(Clone, PartialEq)]
pub enum SyncMode {
    EveryWrite,
    EveryNthWrite(usize),
    Never,
}

#[derive(Clone, PartialEq)]
pub enum BitcaskOption {
    ReuseExistingNonFullWriterOnStartup,
    SyncMode(SyncMode),
}

#[derive(Clone, Default)]
pub struct BitcaskOptions {
    options: Vec<BitcaskOption>,
}

impl BitcaskOptions {
    pub fn add(mut self, option: BitcaskOption) -> Self {
        self.options.push(option);
        self
    }

    pub fn is_enabled(&self, option: BitcaskOption) -> bool {
        self.options.contains(&option)
    }
}
