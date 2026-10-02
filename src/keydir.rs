use std::collections::HashMap;

use crate::datafile::DatafileEntry;

// We are using explicit types to have platform-agnostic behaviors on both 32 and 64 bit CPUs
#[derive(Debug, PartialEq, Clone)]
pub struct KeyDirEntry {
    pub file_id: u32, // ID (e.g. 00000001.data) of the file that contains the given value
    pub value_size: u32, // Size of the value in bytes
    pub value_offset: u64, // Absolute byyte offset to the location of the value in the  datafile
    timestamp: u64,   // UNIX Timestamp when the entry was created
}
impl KeyDirEntry {
    pub fn new(file_id: u32, value_size: u32, value_offset: u64, timestamp: u64) -> Self {
        Self {
            file_id,
            value_size,
            value_offset,
            timestamp,
        }
    }
}

pub struct KeyDir {
    dir: HashMap<Box<[u8]>, KeyDirEntry>, // We are using Box<[u8]> instead of a vector to save 8
                                          // bytes per key on the `capacity` field that vector would store
}

impl KeyDir {
    pub fn new() -> Self {
        KeyDir {
            dir: HashMap::new(),
        }
    }

    pub fn put(&mut self, entry: DatafileEntry, file_id: u32, value_offset: u64) {
        self.dir.insert(
            entry.key,
            KeyDirEntry::new(
                file_id,
                entry.header.value_size,
                value_offset,
                entry.header.timestamp,
            ),
        );
    }

    pub fn size(&self) -> usize {
        self.dir.len()
    }

    pub fn get(&self, key: &[u8]) -> Option<&KeyDirEntry> {
        self.dir.get(key)
    }
}
