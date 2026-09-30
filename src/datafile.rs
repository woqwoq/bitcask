use std::time::{SystemTime, SystemTimeError, UNIX_EPOCH};

use crc32fast::Hasher;

const DATAFILE_HEADER_SIZE_BYTES: usize = 20;

pub enum DatafileError {
    NegativeTimestamp(SystemTimeError),
}

pub struct DatafileHeader {
    crc: u32,        // CRC32 of all fields below
    timestamp: u64,  // UNIX Timestamp when the entry was created
    key_size: u32,   // Size of the key in bytes
    value_size: u32, // Size of the value in bytes
}

impl DatafileHeader {
    pub fn from(key: &[u8], value: &[u8]) -> Result<Self, DatafileError> {
        Self::_from(
            key,
            value,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(DatafileError::NegativeTimestamp)?
                .as_millis() as u64,
        )
    }

    fn _from(key: &[u8], value: &[u8], timestamp: u64) -> Result<Self, DatafileError> {
        let mut hasher = Hasher::new();

        hasher.update(&timestamp.to_be_bytes());

        let key_size = key.len() as u32;
        hasher.update(&key_size.to_be_bytes());

        let value_size = value.len() as u32;
        hasher.update(&value_size.to_be_bytes());

        hasher.update(key);
        hasher.update(value);

        Ok(DatafileHeader {
            crc: hasher.finalize(),
            timestamp,
            key_size,
            value_size,
        })
    }
}

// TODO: Encode(), Decode()
pub struct DatafileEntry {
    header: DatafileHeader,
    key: Box<[u8]>,
    value: Box<[u8]>,
}

impl DatafileEntry {
    pub fn from(key: &[u8], value: &[u8]) -> Result<Self, DatafileError> {
        Ok(Self {
            header: DatafileHeader::from(key, value)?,
            key: Box::from(key),
            value: Box::from(value),
        })
    }
}
