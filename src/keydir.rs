use std::{collections::HashMap, fmt::Display};

use crate::datafile::DatafileLog;

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

    pub fn get(&self, key: &[u8]) -> Option<&KeyDirEntry> {
        self.dir.get(key)
    }

    pub fn put(&mut self, entry: DatafileLog, file_id: u32, value_offset: u64) {
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
}

impl Display for KeyDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "| {:<20} | {:<16} | {:<8} | {:<10} | {:<12} |",
            "key", "timestamp", "file_id", "value_size", "value_offset"
        )?;
        writeln!(
            f,
            "|{:-<22}|{:-<18}|{:-<10}|{:-<12}|{:-<14}|",
            "", "", "", "", ""
        )?;
        for (key, val) in self.dir.iter() {
            let key_str = String::from_utf8_lossy(key);
            writeln!(
                f,
                "| {:<20} | {:<16} | {:<8} | {:<10} | {:<12} |",
                key_str, val.timestamp, val.file_id, val.value_size, val.value_offset
            )?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod keydir_tests {
    use crate::{
        datafile::{DATAFILE_HEADER_SIZE_BYTES, DatafileLog},
        keydir::{KeyDir, KeyDirEntry},
    };

    #[test]
    fn new() {
        let keydir = KeyDir::new();
        assert_eq!(keydir.size(), 0);
    }

    #[test]
    fn put_get() {
        let mut keydir = KeyDir::new();
        assert_eq!(keydir.size(), 0);

        let key = "hello world".as_bytes();
        let value = 67u32.to_be_bytes();

        let datafile_entry = DatafileLog::new(key, &value).unwrap();
        let offset = DATAFILE_HEADER_SIZE_BYTES as u64 + datafile_entry.header.key_size as u64;

        keydir.put(datafile_entry.clone(), 0, offset);

        let expected = KeyDirEntry::new(
            0,
            datafile_entry.header.value_size,
            offset,
            datafile_entry.header.timestamp,
        );

        assert_eq!(keydir.get(key).unwrap(), &expected);
    }
}
