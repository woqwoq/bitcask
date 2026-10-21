use std::fmt::Display;

use ahash::AHashMap;

use crate::options::DATAFILE_HEADER_SIZE_BYTES;

// We are using explicit types to have platform-agnostic behaviors on both 32 and 64 bit CPUs
#[derive(Debug, PartialEq, Clone)]
pub struct KeyDirEntry {
    pub file_id: u32, // ID (e.g. 00000001.data) of the file that contains the given value
    pub value_size: u32, // Size of the value in bytes
    pub value_offset: u64, // Absolute byyte offset to the location of the value in the  datafile
    pub timestamp: u64, // UNIX Timestamp when the entry was created
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
    dir: AHashMap<Box<[u8]>, KeyDirEntry>, // We are using Box<[u8]> instead of a vector to save 8
                                           // bytes per key on the `capacity` field that vector would store
}

impl Default for KeyDir {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyDir {
    pub fn new() -> Self {
        KeyDir {
            dir: AHashMap::new(),
        }
    }

    pub fn with_capacity(n: usize) -> Self {
        KeyDir {
            dir: AHashMap::with_capacity(n),
        }
    }

    pub fn get(&self, key: &[u8]) -> Option<&KeyDirEntry> {
        self.dir.get(key)
    }

    pub fn put(
        &mut self,
        key: Box<[u8]>,
        file_id: u32,
        value_size: u32,
        value_offset: u64,
        timestamp: u64,
    ) -> Option<KeyDirEntry> {
        self.dir.insert(
            key,
            KeyDirEntry::new(file_id, value_size, value_offset, timestamp),
        )
    }

    pub fn get_live_log_size(&self) -> u64 {
        self.dir
            .iter()
            .map(|(k, v)| DATAFILE_HEADER_SIZE_BYTES as u64 + k.len() as u64 + v.value_size as u64)
            .sum()
    }

    pub fn remove(&mut self, key: &[u8]) -> Option<KeyDirEntry> {
        self.dir.remove(key)
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        self.dir.contains_key(key)
    }

    pub fn is_empty(&self) -> bool {
        self.dir.is_empty()
    }

    pub fn len(&self) -> usize {
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
mod tests {
    use crate::{
        keydir::{KeyDir, KeyDirEntry},
        options::DATAFILE_HEADER_SIZE_BYTES,
        record::DatafileRecord,
    };

    #[test]
    fn new_is_empty() {
        let keydir = KeyDir::default();
        assert_eq!(keydir.len(), 0);
    }

    #[test]
    fn put_then_get() {
        let mut keydir = KeyDir::default();
        assert_eq!(keydir.len(), 0);

        let key = "hello world".as_bytes();
        let value = 67u32.to_be_bytes();

        let datafile_record = DatafileRecord::new(key, &value).unwrap();
        let offset = DATAFILE_HEADER_SIZE_BYTES as u64 + datafile_record.header.key_size as u64;

        keydir.put(
            datafile_record.key,
            0,
            datafile_record.header.value_size,
            offset,
            datafile_record.header.timestamp,
        );

        let expected = KeyDirEntry::new(
            0,
            datafile_record.header.value_size,
            offset,
            datafile_record.header.timestamp,
        );

        assert_eq!(keydir.get(key).unwrap(), &expected);
    }
}
