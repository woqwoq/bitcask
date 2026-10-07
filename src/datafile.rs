use std::{
    fmt::Display,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crc32fast::Hasher;

use crate::{
    error::DatafileError,
    keydir::KeyDir,
    options::{DatafileMaxSize, SyncMode},
};

pub const DATAFILE_HEADER_SIZE_BYTES: usize = 21;
pub const DATAFILE_HEADER_RECORD_TYPE_SIZE_BYTES: usize = 1;
pub const DATAFILE_HEADER_CRC_SIZE_BYTES: usize = 4;
pub const DATAFILE_HEADER_TIMESTAMP_SIZE_BYTES: usize = 8;
pub const DATAFILE_HEADER_KSZ_SIZE_BYTES: usize = 4;
pub const DATAFILE_HEADER_VSZ_SIZE_BYTES: usize = 4;

pub const DATAFILE_RECORD_TYPE_NORMAL: u8 = 0;
pub const DATAFILE_RECORD_TYPE_TOMBSTONE: u8 = 1;

pub const DATAFILE_VALUE_STREAM_CHUNK_SIZE_BYTES: usize = 50;

pub struct Datafile {
    file: File,
    pub id: u32,
    pub offset: u64,
    pub is_writer: bool,
}

impl Datafile {
    pub fn new_writer(
        dir: &Path,
        id: u32,
        datafile_extension: &str,
    ) -> Result<Datafile, DatafileError> {
        let filename = Self::generate_filename(dir, id, datafile_extension)?;

        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(filename)?;

        Ok(Datafile {
            file,
            id,
            offset: 0,
            is_writer: true,
        })
    }

    // Proxy call
    fn open_datafile(
        dir: &Path,
        id: u32,
        datafile_extension: &str,
        offset: u64,
        writeable: bool,
        is_writer: bool,
    ) -> Result<Datafile, DatafileError> {
        let filename = Self::generate_filename(dir, id, datafile_extension)?;

        let file = OpenOptions::new()
            .read(true)
            .write(writeable)
            .create(false)
            .open(filename)?;

        Ok(Datafile {
            file,
            id,
            offset,
            is_writer,
        })
    }

    pub fn open_reader(
        dir: &Path,
        id: u32,
        datafile_extension: &str,
    ) -> Result<Datafile, DatafileError> {
        Self::open_datafile(dir, id, datafile_extension, 0, false, false)
    }

    pub fn open_reader_for_recovery(
        dir: &Path,
        id: u32,
        offset: u64,
        datafile_extension: &str,
    ) -> Result<Self, DatafileError> {
        Self::open_datafile(dir, id, datafile_extension, offset, true, false)
    }

    pub fn replay(&mut self, keydir: &mut KeyDir) -> Result<(), DatafileError> {
        let file_size = self.file.metadata().unwrap().len();
        let mut start_offset = 0;

        while start_offset < file_size {
            // TODO: Handle unexpected eof
            let header = DatafileRecordHeader::decode(
                &self.read_bytes(start_offset, DATAFILE_HEADER_SIZE_BYTES)?,
            )?;

            let mut hasher = header.crc();

            let key = self.read_bytes(
                start_offset + DATAFILE_HEADER_SIZE_BYTES as u64,
                header.key_size as usize,
            )?;
            hasher.update(&key);

            let mut value_buffer: [u8; DATAFILE_VALUE_STREAM_CHUNK_SIZE_BYTES] =
                [0u8; DATAFILE_VALUE_STREAM_CHUNK_SIZE_BYTES];
            let mut value_bytes_read: usize = 0;

            while header.value_size > (value_bytes_read) as u32 {
                let to_read = DATAFILE_VALUE_STREAM_CHUNK_SIZE_BYTES
                    .min(header.value_size as usize - value_bytes_read);

                let bytes = self.file.read_at(
                    &mut value_buffer[..to_read],
                    start_offset
                        + DATAFILE_HEADER_SIZE_BYTES as u64
                        + header.key_size as u64
                        + value_bytes_read as u64,
                )?;

                if bytes == 0 {
                    break;
                }

                value_bytes_read += bytes;
                hasher.update(&value_buffer[..bytes]);
            }

            if header.value_size > (value_bytes_read) as u32 {
                return Err(DatafileError::TornWrite);
            }

            if hasher.finalize() != header.crc {
                return Err(DatafileError::CrcMismatch);
            }

            match header.record_type {
                DATAFILE_RECORD_TYPE_NORMAL => {
                    keydir.put(
                        key,
                        self.id,
                        header.value_size,
                        start_offset + DATAFILE_HEADER_SIZE_BYTES as u64 + header.key_size as u64,
                        header.timestamp,
                    );
                }
                DATAFILE_RECORD_TYPE_TOMBSTONE => {
                    keydir.remove(&key);
                }
                _ => return Err(DatafileError::UnexpectedDatafileRecordType),
            }

            start_offset += header.size()? as u64;
            self.offset = start_offset;
        }

        Ok(())
    }

    pub fn truncate(&mut self, new_size: u64) -> Result<(), DatafileError> {
        self.file
            .set_len(new_size)
            .map_err(DatafileError::TruncateFailure)
    }

    pub fn append_record(&mut self, record: &DatafileRecord) -> Result<u64, DatafileError> {
        if !self.is_writer {
            return Err(DatafileError::WriteAttemptToReaderDatafile);
        }

        let start_offset = self.offset;

        // Move offset, so first so we fail-fast before we commit the transaction
        self.move_writer_offset(record.size()? as u64)?;

        // Commit
        Self::write_record_to(&mut self.file, record)?;

        Ok(start_offset)
    }

    pub fn write_record_to<W: Write>(
        writer: &mut W,
        record: &DatafileRecord,
    ) -> Result<(), DatafileError> {
        writer.write_all(&record.header.encode())?;
        writer.write_all(&record.key)?;
        writer.write_all(&record.value)?;

        Ok(())
    }

    pub fn should_switch_writer_before_write_commit(
        &self,
        record_size: u64,
        datafile_max_size: &DatafileMaxSize,
    ) -> Result<bool, DatafileError> {
        let is_writer_oversized = self
            .offset
            .checked_add(record_size)
            .unwrap_or(datafile_max_size.get_size() as u64)
            >= datafile_max_size.get_size() as u64;

        let is_writer_overcapped = match datafile_max_size {
            DatafileMaxSize::SoftCappedBytes(_) => self.offset > 0, // Soft cap will allow one
            // write no matter the size, so we conditionally switchover
            DatafileMaxSize::HardCappedBytes(_) => true, // Hard cap needs to be strictly within
                                                         // the max size, so we enforce switchover
        };

        Ok(is_writer_oversized && is_writer_overcapped)
    }

    pub fn sync(&mut self, mode: &SyncMode) -> Result<(), DatafileError> {
        match mode {
            SyncMode::SyncAll => self.file.sync_all(),
            SyncMode::SyncData => self.file.sync_data(),
        }
        .map_err(DatafileError::SyncError)
    }

    fn move_writer_offset(&mut self, offset: u64) -> Result<(), DatafileError> {
        let new_offset = self
            .offset
            .checked_add(offset)
            .ok_or(DatafileError::WriterFileOffsetOverflow)?;

        self.offset = new_offset;

        Ok(())
    }

    pub fn read_bytes(&self, offset: u64, count: usize) -> Result<Box<[u8]>, DatafileError> {
        let mut buf = vec![0u8; count];

        self.file.read_exact_at(&mut buf, offset)?;

        Ok(Box::from(buf))
    }

    pub fn demote_to_reader(&mut self) {
        self.is_writer = false;
    }

    pub fn generate_filename(
        dir: &Path,
        file_id: u32,
        datafile_extension: &str,
    ) -> Result<PathBuf, DatafileError> {
        Ok(dir.join(format!("{:08}.{}", file_id, datafile_extension)))
    }
}

impl Display for Datafile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "| {:<6} | {:<10} | {:<10} |",
            "id", "offset", "is_writer"
        )?;
        writeln!(f, "|{:-<8}|{:-<12}|{:-<12}|", "", "", "")?;
        write!(
            f,
            "| {:<6} | {:<10} | {:<10} |",
            self.id, self.offset, self.is_writer
        )
    }
}

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

    pub fn size(&self) -> Result<usize, DatafileError> {
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

    pub fn new_tombstone(key: &[u8]) -> Result<Self, DatafileError> {
        Ok(Self {
            header: DatafileRecordHeader::new(key, &[], DATAFILE_RECORD_TYPE_TOMBSTONE)?,
            key: Box::from(key),
            value: Box::from([]),
        })
    }

    pub fn size(&self) -> Result<usize, DatafileError> {
        self.header.size()
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

        let key_bytes: Box<[u8]> =
            Box::from(&bytes[DATAFILE_HEADER_SIZE_BYTES..expected_datafile_size_with_key as usize]);

        let expected_datafile_size_with_kv =
            expected_datafile_size_with_key + u64::from(header.value_size);
        if expected_datafile_size_with_kv > datafile_size {
            return Err(DatafileError::UndersizedDatafileValue {
                expected: expected_datafile_size_with_kv as usize,
                actual: datafile_size as usize,
            });
        }

        let value_bytes: Box<[u8]> = Box::from(
            &bytes
                [expected_datafile_size_with_key as usize..expected_datafile_size_with_kv as usize],
        );

        if header.crc
            != DatafileRecordHeader::get_crc_for(
                header.record_type,
                header.timestamp,
                header.key_size,
                header.value_size,
                &key_bytes,
                &value_bytes,
            )
        {
            return Err(DatafileError::CrcMismatch);
        }

        Ok(Self {
            header,
            key: key_bytes,
            value: value_bytes,
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
    use std::{
        io::Seek,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{
        datafile::{
            DATAFILE_HEADER_SIZE_BYTES, DATAFILE_RECORD_TYPE_NORMAL, Datafile, DatafileError,
            DatafileRecord, DatafileRecordHeader,
        },
        keydir::KeyDir,
        options::BitcaskOptions,
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

    mod writer {
        use super::*;

        #[test]
        fn append_and_read_record() {
            let temp_dir = tempfile::tempdir().unwrap();
            let mut df = Datafile::new_writer(
                temp_dir.path(),
                1,
                &BitcaskOptions::default().datafile_extension,
            )
            .unwrap();

            let datafile_record =
                DatafileRecord::new("hello world".as_bytes(), &2u32.to_be_bytes()).unwrap();
            let record_size = datafile_record.size().unwrap();

            let offset = df.append_record(&datafile_record).unwrap();
            assert_eq!(offset, 0);
            assert_eq!(df.offset, record_size as u64);

            let mut expected = Vec::new();
            Datafile::write_record_to(&mut expected, &datafile_record).unwrap();
            let read_bytes = df.read_bytes(offset, record_size).unwrap();
            assert_eq!(&*read_bytes, &expected[..]);
        }

        #[test]
        fn append_advances_offset() {
            let temp_dir = tempfile::tempdir().unwrap();
            let mut df = Datafile::new_writer(
                temp_dir.path(),
                1,
                &BitcaskOptions::default().datafile_extension,
            )
            .unwrap();

            let datafile_record =
                DatafileRecord::new("hello world".as_bytes(), &2u32.to_be_bytes()).unwrap();
            let record_size = datafile_record.size().unwrap() as u64;

            assert_eq!(df.offset, 0);
            let offset = df.append_record(&datafile_record).unwrap();
            assert_eq!(offset, 0);
            assert_eq!(df.offset, record_size);
        }

        #[test]
        fn append_rejected_after_demote() {
            let temp_dir = tempfile::tempdir().unwrap();
            let mut df = Datafile::new_writer(
                temp_dir.path(),
                1,
                &BitcaskOptions::default().datafile_extension,
            )
            .unwrap();
            df.demote_to_reader();

            let datafile_record =
                DatafileRecord::new("hello world".as_bytes(), &2u32.to_be_bytes()).unwrap();

            assert!(matches!(
                df.append_record(&datafile_record),
                Err(DatafileError::WriteAttemptToReaderDatafile)
            ));
        }

        #[test]
        fn append_allows_oversized_record_on_empty_writer() {
            let temp_dir = tempfile::tempdir().unwrap();
            let options = BitcaskOptions::default();
            let mut df = Datafile::new_writer(
                temp_dir.path(),
                1,
                &BitcaskOptions::default().datafile_extension,
            )
            .unwrap();

            let mut datafile_record =
                DatafileRecord::new("hello world".as_bytes(), &2u32.to_be_bytes()).unwrap();

            // We issue a write because one log file should accept at least one entry
            datafile_record.header.value_size = options.datafile_max_size.get_size() as u32;
            assert!(df.append_record(&datafile_record).is_ok());
        }

        #[test]
        fn offset_overflow_err() {
            let temp_dir = tempfile::tempdir().unwrap();
            let mut df = Datafile::new_writer(
                temp_dir.path(),
                1,
                &BitcaskOptions::default().datafile_extension,
            )
            .unwrap();
            df.offset = u64::MAX;

            assert!(matches!(
                df.move_writer_offset(1),
                Err(DatafileError::WriterFileOffsetOverflow)
            ));
        }
    }

    mod replay {
        use super::*;

        #[test]
        fn rebuilds_keydir() {
            let temp_dir = tempfile::tempdir().unwrap();
            let mut df = Datafile::new_writer(
                temp_dir.path(),
                1,
                &BitcaskOptions::default().datafile_extension,
            )
            .unwrap();

            // Pre-fill datafile
            for i in 0..30usize {
                assert!(
                    df.append_record(
                        &DatafileRecord::new(format!("{i}").as_bytes(), &i.to_be_bytes()).unwrap()
                    )
                    .is_ok()
                );
            }
            let _ = df.file.rewind();

            let mut keydir = KeyDir::new();

            df.replay(&mut keydir).unwrap();

            assert_eq!(keydir.len(), 30);
            (0..30usize).for_each(|i| assert!(keydir.contains(format!("{i}").as_bytes())));
        }

        #[test]
        fn handles_tombstones() {
            let temp_dir = tempfile::tempdir().unwrap();
            let mut df = Datafile::new_writer(
                temp_dir.path(),
                1,
                &BitcaskOptions::default().datafile_extension,
            )
            .unwrap();

            // Pre-fill datafile
            for i in 0..50usize {
                assert!(
                    df.append_record(
                        &DatafileRecord::new(format!("{i}").as_bytes(), &i.to_be_bytes()).unwrap()
                    )
                    .is_ok()
                );
            }
            for i in 0..25usize {
                assert!(
                    df.append_record(
                        &DatafileRecord::new_tombstone(format!("{i}").as_bytes()).unwrap()
                    )
                    .is_ok()
                );
            }

            let _ = df.file.rewind();

            let mut keydir = KeyDir::new();

            df.replay(&mut keydir).unwrap();

            assert_eq!(keydir.len(), 25);
            (25..50).for_each(|i| assert!(keydir.contains(format!("{i}").as_bytes())));
        }

        #[test]
        fn handles_values_larger_than_chunk_size() {
            let temp_dir = tempfile::tempdir().unwrap();
            let mut df = Datafile::new_writer(
                temp_dir.path(),
                1,
                &BitcaskOptions::default().datafile_extension,
            )
            .unwrap();

            // Pre-fill datafile with a value larger than chunk size
            assert!(
                df.append_record(
                    &DatafileRecord::new(
                        "1".as_bytes(),
                        "11111111111111111111111111111111111111".as_bytes()
                    )
                    .unwrap()
                )
                .is_ok()
            );
            let _ = df.file.rewind();

            let mut keydir = KeyDir::new();

            assert!(df.replay(&mut keydir).is_ok());
        }
    }

    mod recovery {
        use super::*;

        #[test]
        fn rejects_append_but_allows_truncate() {
            let temp_dir = tempfile::tempdir().unwrap();
            let mut df = Datafile::new_writer(
                temp_dir.path(),
                1,
                &BitcaskOptions::default().datafile_extension,
            )
            .unwrap();

            // Pre-fill datafile
            for i in 0..50usize {
                assert!(
                    df.append_record(
                        &DatafileRecord::new(format!("{i}").as_bytes(), &i.to_be_bytes()).unwrap()
                    )
                    .is_ok()
                );
            }
            df.demote_to_reader();
            let id = df.id;
            let offset = df.offset;
            drop(df); // Free the file handle

            let mut reader = Datafile::open_reader_for_recovery(
                temp_dir.path(),
                id,
                offset,
                &BitcaskOptions::default().datafile_extension,
            )
            .unwrap();

            assert!(
                reader
                    .append_record(&DatafileRecord::new("1".as_bytes(), "1".as_bytes()).unwrap())
                    .is_err()
            ); // The reader should still be immutable record-wise

            assert!(reader.truncate(offset).is_ok());
        }
    }
}
