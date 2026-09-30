use std::time::{SystemTime, SystemTimeError, UNIX_EPOCH};

use crc32fast::Hasher;

const DATAFILE_HEADER_SIZE_BYTES: usize = 20;

#[derive(Debug)]
pub enum DatafileError {
    NegativeTimestamp(SystemTimeError),
}

#[derive(Debug, PartialEq)]
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

    fn encode(&self) -> [u8; DATAFILE_HEADER_SIZE_BYTES] {
        let mut arr: [u8; DATAFILE_HEADER_SIZE_BYTES] = [0; DATAFILE_HEADER_SIZE_BYTES];

        arr[0..4].clone_from_slice(&self.crc.to_be_bytes());
        arr[4..12].clone_from_slice(&self.timestamp.to_be_bytes());
        arr[12..16].clone_from_slice(&self.key_size.to_be_bytes());
        arr[16..20].clone_from_slice(&self.value_size.to_be_bytes());

        arr
    }
}

// TODO: Decode()
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
    pub fn _from(key: &[u8], value: &[u8], timestamp: u64) -> Result<Self, DatafileError> {
        Ok(Self {
            header: DatafileHeader::_from(key, value, timestamp)?,
            key: Box::from(key),
            value: Box::from(value),
        })
    }

    pub fn encode(&self) -> Box<[u8]> {
        let size =
            DATAFILE_HEADER_SIZE_BYTES + (self.header.key_size + self.header.value_size) as usize;

        // TODO: Rework into vectored writes to avoid allocation
        let mut buf: Vec<u8> = Vec::with_capacity(size);

        buf.extend_from_slice(&self.header.encode());
        buf.extend_from_slice(&self.key);
        buf.extend_from_slice(&self.value);

        Box::from(buf)
    }
}

#[cfg(test)]
mod datafile_tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use crc32fast::Hasher;

    use crate::datafile::{DatafileEntry, DatafileHeader};

    pub fn crc_of(timestamp: u64, key_size: u32, value_size: u32, key: &[u8], value: &[u8]) -> u32 {
        let mut hasher = Hasher::new();

        hasher.update(&timestamp.to_be_bytes());
        hasher.update(&key_size.to_be_bytes());
        hasher.update(&value_size.to_be_bytes());
        hasher.update(key);
        hasher.update(value);

        hasher.finalize()
    }

    #[test]
    fn datafile_header_int() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let key: u32 = 1;
        let key_size: u32 = key.to_be_bytes().len() as u32;

        let value: u64 = 2;
        let value_size: u32 = value.to_be_bytes().len() as u32;

        let datafile_header =
            DatafileHeader::_from(&key.to_be_bytes(), &value.to_be_bytes(), timestamp);

        assert!(datafile_header.is_ok());
        if let Ok(datafile_header) = datafile_header {
            assert_eq!(
                datafile_header.crc,
                crc_of(
                    timestamp,
                    key_size,
                    value_size,
                    &key.to_be_bytes(),
                    &value.to_be_bytes()
                )
            );
            assert_eq!(datafile_header.timestamp, timestamp);
            assert_eq!(datafile_header.key_size, key_size);
            assert_eq!(datafile_header.value_size, value_size);
        }
    }

    #[test]
    fn datafile_header_string() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let key: String = String::from("hello world");
        let key_size: u32 = key.len() as u32;

        let value: u64 = 2;
        let value_size: u32 = value.to_be_bytes().len() as u32;

        let datafile_header =
            DatafileHeader::_from(key.as_bytes(), &value.to_be_bytes(), timestamp).unwrap();

        assert_eq!(
            datafile_header.crc,
            crc_of(
                timestamp,
                key_size,
                value_size,
                &key.into_bytes(),
                &value.to_be_bytes()
            )
        );
        assert_eq!(datafile_header.timestamp, timestamp);
        assert_eq!(datafile_header.key_size, key_size);
        assert_eq!(datafile_header.value_size, value_size);
    }

    #[test]
    fn datafile_entry() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let key: String = String::from("hello world");
        let value: u64 = 2;

        let expected_datafile_header =
            DatafileHeader::_from(key.as_bytes(), &value.to_be_bytes(), timestamp).unwrap();

        let datafile_entry =
            DatafileEntry::_from(key.as_bytes(), &value.to_be_bytes(), timestamp).unwrap();

        assert_eq!(datafile_entry.header, expected_datafile_header);
        assert_eq!(&datafile_entry.key.to_vec(), &key.into_bytes());
        assert_eq!(&datafile_entry.value.to_vec(), &value.to_be_bytes());
    }

    #[test]
    fn datafile_header_encode() {
        let key: String = String::from("hello world");
        let value: u64 = 2;

        let datafile_header = DatafileHeader::from(key.as_bytes(), &value.to_be_bytes()).unwrap();

        let mut expected: Vec<u8> = Vec::new();
        datafile_header
            .crc
            .to_be_bytes()
            .iter()
            .for_each(|b| expected.push(*b));
        datafile_header
            .timestamp
            .to_be_bytes()
            .iter()
            .for_each(|b| expected.push(*b));
        datafile_header
            .key_size
            .to_be_bytes()
            .iter()
            .for_each(|b| expected.push(*b));
        datafile_header
            .value_size
            .to_be_bytes()
            .iter()
            .for_each(|b| expected.push(*b));

        assert_eq!(datafile_header.encode().to_vec(), expected);
    }

    #[test]
    fn datafile_entry_encode() {
        let key: String = String::from("hello world");
        let value: u64 = 2;

        let datafile_entry = DatafileEntry::from(key.as_bytes(), &value.to_be_bytes()).unwrap();

        let mut kv: Vec<u8> = Vec::new();
        kv.extend_from_slice(key.as_bytes());
        kv.extend_from_slice(&value.to_be_bytes());

        let encoded = datafile_entry.encode();

        assert_eq!(encoded[0..20], datafile_entry.header.encode());
        assert_eq!(encoded[20..], kv);
    }
}
