use std::{
    fmt::Display,
    time::{SystemTime, UNIX_EPOCH},
};

use crc32fast::Hasher;

use crate::{error::DatafileError, options::DATAFILE_HEADER_SIZE_BYTES};

pub const DATAFILE_HEADER_RECORD_TYPE_SIZE_BYTES: usize = 1;
pub const DATAFILE_HEADER_CRC_SIZE_BYTES: usize = 4;
pub const DATAFILE_HEADER_TIMESTAMP_SIZE_BYTES: usize = 8;
pub const DATAFILE_HEADER_KSZ_SIZE_BYTES: usize = 4;
pub const DATAFILE_HEADER_VSZ_SIZE_BYTES: usize = 4;

pub const DATAFILE_RECORD_TYPE_NORMAL: u8 = 0;
pub const DATAFILE_RECORD_TYPE_TOMBSTONE: u8 = 1;

#[derive(Debug, PartialEq, Clone)]
pub struct DatafileRecordHeader {
    pub crc: u32,        // CRC32 of all fields below
    pub record_type: u8, // 0 - Normal, 1 - Tombstone
    pub timestamp: u64,  // UNIX Timestamp when the entry was created
    pub key_size: u32,   // Size of the key in bytes
    pub value_size: u32, // Size of the value in bytes
}

impl DatafileRecordHeader {
    pub fn new(key: &[u8], value: &[u8], record_type: u8) -> Result<Self, DatafileError> {
        Self::with_timestamp(
            key,
            value,
            record_type,
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64,
        )
    }

    fn with_timestamp(
        key: &[u8],
        value: &[u8],
        record_type: u8,
        timestamp: u64,
    ) -> Result<Self, DatafileError> {
        let key_size =
            u32::try_from(key.len()).map_err(|_| DatafileError::OversizedDatafileRecord)?;
        let value_size =
            u32::try_from(value.len()).map_err(|_| DatafileError::OversizedDatafileRecord)?;

        Ok(DatafileRecordHeader {
            crc: Self::get_crc_for(record_type, timestamp, key_size, value_size, key, value),
            record_type,
            timestamp,
            key_size,
            value_size,
        })
    }

    pub fn crc(&self) -> Hasher {
        let mut hasher = Hasher::new();

        hasher.update(&self.record_type.to_be_bytes());
        hasher.update(&self.timestamp.to_be_bytes());
        hasher.update(&self.key_size.to_be_bytes());
        hasher.update(&self.value_size.to_be_bytes());

        hasher
    }

    fn get_crc_for(
        record_type: u8,
        timestamp: u64,
        key_size: u32,
        value_size: u32,
        key: &[u8],
        value: &[u8],
    ) -> u32 {
        let mut hasher = Hasher::new();

        hasher.update(&record_type.to_be_bytes());
        hasher.update(&timestamp.to_be_bytes());
        hasher.update(&key_size.to_be_bytes());
        hasher.update(&value_size.to_be_bytes());

        hasher.update(key);
        hasher.update(value);

        hasher.finalize()
    }

    pub fn encode(&self) -> [u8; DATAFILE_HEADER_SIZE_BYTES] {
        let mut arr: [u8; DATAFILE_HEADER_SIZE_BYTES] = [0; DATAFILE_HEADER_SIZE_BYTES];

        arr[0..1].copy_from_slice(&self.record_type.to_be_bytes());
        arr[1..5].copy_from_slice(&self.crc.to_be_bytes());
        arr[5..13].copy_from_slice(&self.timestamp.to_be_bytes());
        arr[13..17].copy_from_slice(&self.key_size.to_be_bytes());
        arr[17..21].copy_from_slice(&self.value_size.to_be_bytes());

        arr
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DatafileError> {
        if DATAFILE_HEADER_SIZE_BYTES > bytes.len() {
            return Err(DatafileError::UndersizedDatafileHeader(bytes.len()));
        }

        let record_type: [u8; DATAFILE_HEADER_RECORD_TYPE_SIZE_BYTES] = bytes[0..1].try_into()?;
        let crc_bytes: [u8; DATAFILE_HEADER_CRC_SIZE_BYTES] = bytes[1..5].try_into()?;
        let timestamp_bytes: [u8; DATAFILE_HEADER_TIMESTAMP_SIZE_BYTES] =
            bytes[5..13].try_into()?;
        let key_size_bytes: [u8; DATAFILE_HEADER_KSZ_SIZE_BYTES] = bytes[13..17].try_into()?;
        let value_size_bytes: [u8; DATAFILE_HEADER_VSZ_SIZE_BYTES] = bytes[17..21].try_into()?;

        Ok(Self {
            crc: u32::from_be_bytes(crc_bytes),
            record_type: u8::from_be_bytes(record_type),
            timestamp: u64::from_be_bytes(timestamp_bytes),
            key_size: u32::from_be_bytes(key_size_bytes),
            value_size: u32::from_be_bytes(value_size_bytes),
        })
    }

    pub fn payload_size(&self) -> Result<usize, DatafileError> {
        let size = u64::from(self.key_size)
            .checked_add(u64::from(self.value_size))
            .and_then(|kv| kv.checked_add(DATAFILE_HEADER_SIZE_BYTES as u64))
            .ok_or(DatafileError::OversizedDatafileRecord)?;

        usize::try_from(size).map_err(|_| DatafileError::OversizedDatafileRecord)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct DatafileRecord {
    pub header: DatafileRecordHeader,
    pub key: Box<[u8]>,
    pub value: Box<[u8]>,
}

impl DatafileRecord {
    pub fn new(key: &[u8], value: &[u8]) -> Result<Self, DatafileError> {
        Ok(Self {
            header: DatafileRecordHeader::new(key, value, DATAFILE_RECORD_TYPE_NORMAL)?,
            key: Box::from(key),
            value: Box::from(value),
        })
    }

    pub fn with_timestamp(key: &[u8], value: &[u8], timestamp: u64) -> Result<Self, DatafileError> {
        Ok(Self {
            header: DatafileRecordHeader::with_timestamp(
                key,
                value,
                DATAFILE_RECORD_TYPE_NORMAL,
                timestamp,
            )?,
            key: Box::from(key),
            value: Box::from(value),
        })
    }

    pub fn new_tombstone(key: &[u8]) -> Result<Self, DatafileError> {
        Ok(Self {
            header: DatafileRecordHeader::new(key, &[], DATAFILE_RECORD_TYPE_TOMBSTONE)?,
            key: Box::from(key),
            value: Box::from([]),
        })
    }

    pub fn size(&self) -> Result<usize, DatafileError> {
        self.header.payload_size()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DatafileError> {
        let datafile_size = bytes.len() as u64;
        let header = DatafileRecordHeader::decode(bytes)?;

        let expected_datafile_size_with_key =
            DATAFILE_HEADER_SIZE_BYTES as u64 + u64::from(header.key_size);
        if expected_datafile_size_with_key > datafile_size {
            return Err(DatafileError::UndersizedDatafileKey {
                expected: expected_datafile_size_with_key as usize,
                actual: datafile_size as usize,
            });
        }

        let key_bytes =
            &bytes[DATAFILE_HEADER_SIZE_BYTES..expected_datafile_size_with_key as usize];

        let expected_datafile_size_with_kv =
            expected_datafile_size_with_key + u64::from(header.value_size);
        if expected_datafile_size_with_kv > datafile_size {
            return Err(DatafileError::UndersizedDatafileValue {
                expected: expected_datafile_size_with_kv as usize,
                actual: datafile_size as usize,
            });
        }

        let value_bytes = &bytes
            [expected_datafile_size_with_key as usize..expected_datafile_size_with_kv as usize];

        if header.crc
            != DatafileRecordHeader::get_crc_for(
                header.record_type,
                header.timestamp,
                header.key_size,
                header.value_size,
                key_bytes,
                value_bytes,
            )
        {
            return Err(DatafileError::CrcMismatch);
        }

        Ok(Self {
            header,
            key: Box::from(key_bytes),
            value: Box::from(value_bytes),
        })
    }
}

impl Display for DatafileRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key_str = String::from_utf8_lossy(&self.key);
        let value_str = String::from_utf8_lossy(&self.value);

        writeln!(
            f,
            "| {:<11} | {:<10} | {:<16} | {:<10} | {:<10} | {:<20} | {:<20} |",
            "record_type", "crc", "timestamp", "key_size", "value_size", "key", "value"
        )?;
        writeln!(
            f,
            "|{:-<13}|{:-<12}|{:-<18}|{:-<12}|{:-<12}|{:-<22}|{:-<22}|",
            "", "", "", "", "", "", ""
        )?;
        write!(
            f,
            "| {:<11} | {:<10} | {:<16} | {:<10} | {:<10} | {:<20} | {:<20} |",
            self.header.record_type,
            self.header.crc,
            self.header.timestamp,
            self.header.key_size,
            self.header.value_size,
            key_str,
            value_str
        )
    }
}

#[cfg(test)]
mod tests {
    use crc32fast::Hasher;

    use crate::{
        error::DatafileError,
        record::{DATAFILE_RECORD_TYPE_NORMAL, DatafileRecord, DatafileRecordHeader},
    };

    pub fn with_timestamp(
        key: &[u8],
        value: &[u8],
        timestamp: u64,
    ) -> Result<DatafileRecord, DatafileError> {
        Ok(DatafileRecord {
            header: DatafileRecordHeader::with_timestamp(
                key,
                value,
                DATAFILE_RECORD_TYPE_NORMAL,
                timestamp,
            )?,
            key: Box::from(key),
            value: Box::from(value),
        })
    }

    pub fn crc_of(
        record_type: u8,
        timestamp: u64,
        key_size: u32,
        value_size: u32,
        key: &[u8],
        value: &[u8],
    ) -> u32 {
        let mut hasher = Hasher::new();

        hasher.update(&record_type.to_be_bytes());
        hasher.update(&timestamp.to_be_bytes());
        hasher.update(&key_size.to_be_bytes());
        hasher.update(&value_size.to_be_bytes());
        hasher.update(key);
        hasher.update(value);

        hasher.finalize()
    }

    mod header {
        use std::time::{SystemTime, UNIX_EPOCH};

        use super::*;

        #[test]
        fn int_key() {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;

            let key: u32 = 1;
            let key_size: u32 = key.to_be_bytes().len() as u32;

            let value: u64 = 2;
            let value_size: u32 = value.to_be_bytes().len() as u32;

            let datafile_header = DatafileRecordHeader::with_timestamp(
                &key.to_be_bytes(),
                &value.to_be_bytes(),
                DATAFILE_RECORD_TYPE_NORMAL,
                timestamp,
            );

            assert!(datafile_header.is_ok());
            if let Ok(datafile_header) = datafile_header {
                assert_eq!(
                    datafile_header.crc,
                    crc_of(
                        DATAFILE_RECORD_TYPE_NORMAL,
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
        fn string_key() {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;

            let key: String = String::from("hello world");
            let key_size: u32 = key.len() as u32;

            let value: u64 = 2;
            let value_size: u32 = value.to_be_bytes().len() as u32;

            let datafile_header = DatafileRecordHeader::with_timestamp(
                key.as_bytes(),
                &value.to_be_bytes(),
                DATAFILE_RECORD_TYPE_NORMAL,
                timestamp,
            )
            .unwrap();

            assert_eq!(
                datafile_header.crc,
                crc_of(
                    DATAFILE_RECORD_TYPE_NORMAL,
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
        fn encode() {
            let key: String = String::from("hello world");
            let value: u64 = 2;

            let datafile_header = DatafileRecordHeader::new(
                key.as_bytes(),
                &value.to_be_bytes(),
                DATAFILE_RECORD_TYPE_NORMAL,
            )
            .unwrap();

            let mut expected: Vec<u8> = Vec::new();
            datafile_header
                .record_type
                .to_be_bytes()
                .iter()
                .for_each(|b| expected.push(*b));
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
        fn decode_ok() {
            let datafile_header = DatafileRecordHeader::new(
                String::from("hello world").as_bytes(),
                &2u64.to_be_bytes(),
                DATAFILE_RECORD_TYPE_NORMAL,
            )
            .unwrap();

            assert_eq!(
                datafile_header,
                DatafileRecordHeader::decode(&datafile_header.encode()).unwrap()
            );
        }

        #[test]
        fn decode_err() {
            let datafile_header = DatafileRecordHeader::new(
                String::from("hello world").as_bytes(),
                &2u64.to_be_bytes(),
                DATAFILE_RECORD_TYPE_NORMAL,
            )
            .unwrap();

            assert!(matches!(
                DatafileRecordHeader::decode(&datafile_header.encode()[0..9]),
                Err(DatafileError::UndersizedDatafileHeader(9))
            ));
        }
    }

    mod record {
        use std::time::{SystemTime, UNIX_EPOCH};

        use crate::{datafile::Datafile, record::DATAFILE_HEADER_SIZE_BYTES};

        use super::*;

        #[test]
        fn new() {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;

            let key: String = String::from("hello world");
            let value: u64 = 2;

            let expected_datafile_header = DatafileRecordHeader::with_timestamp(
                key.as_bytes(),
                &value.to_be_bytes(),
                DATAFILE_RECORD_TYPE_NORMAL,
                timestamp,
            )
            .unwrap();

            let datafile_record =
                with_timestamp(key.as_bytes(), &value.to_be_bytes(), timestamp).unwrap();

            assert_eq!(datafile_record.header, expected_datafile_header);
            assert_eq!(&datafile_record.key.to_vec(), &key.into_bytes());
            assert_eq!(&datafile_record.value.to_vec(), &value.to_be_bytes());
        }

        #[test]
        fn encode() {
            let key: String = String::from("hello world");
            let value: u64 = 2;

            let datafile_record =
                DatafileRecord::new(key.as_bytes(), &value.to_be_bytes()).unwrap();

            let mut kv: Vec<u8> = Vec::new();
            kv.extend_from_slice(key.as_bytes());
            kv.extend_from_slice(&value.to_be_bytes());

            let mut encoded: Vec<u8> = Vec::new();

            Datafile::write_record_to(&mut encoded, &datafile_record).unwrap();

            assert_eq!(
                encoded[0..DATAFILE_HEADER_SIZE_BYTES],
                datafile_record.header.encode()
            );
            assert_eq!(encoded[DATAFILE_HEADER_SIZE_BYTES..], kv);
        }

        #[test]
        fn decode_ok() {
            let datafile_record =
                DatafileRecord::new(String::from("hello world").as_bytes(), &2u64.to_be_bytes())
                    .unwrap();

            let mut encoded: Vec<u8> = Vec::new();

            Datafile::write_record_to(&mut encoded, &datafile_record).unwrap();

            assert_eq!(datafile_record, DatafileRecord::decode(&encoded).unwrap());
        }

        #[test]
        fn decode_err() {
            let datafile_record =
                DatafileRecord::new(String::from("hello world").as_bytes(), &2u64.to_be_bytes())
                    .unwrap();

            let mut encoded: Vec<u8> = Vec::new();

            Datafile::write_record_to(&mut encoded, &datafile_record).unwrap();

            assert!(matches!(
                DatafileRecord::decode(&encoded[0..DATAFILE_HEADER_SIZE_BYTES + 5]),
                Err(DatafileError::UndersizedDatafileKey { .. })
            ));

            assert!(matches!(
                DatafileRecord::decode(&encoded[..encoded.len() - 1]),
                Err(DatafileError::UndersizedDatafileValue { .. })
            ));
        }

        #[test]
        fn decode_err_crc_mismatch() {
            let datafile_record =
                DatafileRecord::new(String::from("hello world").as_bytes(), &2u64.to_be_bytes())
                    .unwrap();

            let mut encoded: Vec<u8> = Vec::new();
            Datafile::write_record_to(&mut encoded, &datafile_record).unwrap();

            let last = encoded.len();
            encoded[last - 1] = 0u8;

            assert!(matches!(
                DatafileRecord::decode(&encoded),
                Err(DatafileError::CrcMismatch)
            ));
        }

        #[test]
        fn decode_err_corrupt_sizes() {
            let mut bytes = vec![0u8; DATAFILE_HEADER_SIZE_BYTES];

            bytes[13..17].copy_from_slice(&u32::MAX.to_be_bytes());
            assert!(matches!(
                DatafileRecord::decode(&bytes),
                Err(DatafileError::UndersizedDatafileKey { .. })
            ));

            bytes[13..17].copy_from_slice(&0u32.to_be_bytes());
            bytes[17..21].copy_from_slice(&u32::MAX.to_be_bytes());
            assert!(matches!(
                DatafileRecord::decode(&bytes),
                Err(DatafileError::UndersizedDatafileValue { .. })
            ));
        }

        #[test]
        fn size_ok() {
            let datafile_record =
                DatafileRecord::new(String::from("hello world").as_bytes(), &2u64.to_be_bytes())
                    .unwrap();

            assert!(datafile_record.size().is_ok());
        }

        #[test]
        fn display() {
            let record = DatafileRecord::new("hello".as_bytes(), "world".as_bytes()).unwrap();
            let formatted = format!("{record}");
            assert!(formatted.contains("hello"));
            assert!(formatted.contains("world"));
        }
    }
}
