use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::FileExt,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use crc32fast::Hasher;

use crate::error::DatafileError;

pub const DATAFILE_HEADER_SIZE_BYTES: usize = 20;
pub const DATAFILE_HEADER_CRC_SIZE_BYTES: usize = 4;
pub const DATAFILE_HEADER_TIMESTAMP_SIZE_BYTES: usize = 8;
pub const DATAFILE_HEADER_KSZ_SIZE_BYTES: usize = 4;
pub const DATAFILE_HEADER_VSZ_SIZE_BYTES: usize = 4;

pub const DATAFILE_MAX_SIZE_BYTES: usize = 10_000_000;
const DATAFILE_EXTENSION: &str = "db";

pub struct Datafile {
    file: File,
    pub id: u32,
    pub offset: u64,
    path: PathBuf,
    pub is_writer: bool,
}

impl Datafile {
    pub fn new(dir: PathBuf, file_id: u32) -> Result<Datafile, DatafileError> {
        let filename = Self::generate_filename(&dir, file_id)?;

        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(filename)?;

        Ok(Datafile {
            file,
            id: file_id,
            offset: 0,
            path: dir.clone(),
            is_writer: true,
        })
    }

    pub fn write_log(&mut self, entry: &DatafileLog) -> Result<u64, DatafileError> {
        if !self.is_writer {
            return Err(DatafileError::WriteAttemptToReaderDatafile);
        }

        let start_offset = self.offset;

        // Move offset, so first so we fail-fast before we commit the transaction
        self.move_writer_offset(entry.size()? as u64)?;

        // Commit
        Self::write_log_to(&mut self.file, entry)?;

        Ok(start_offset)
    }

    pub fn write_log_to<W: Write>(
        writer: &mut W,
        entry: &DatafileLog,
    ) -> Result<(), DatafileError> {
        writer.write_all(&entry.header.encode())?;
        writer.write_all(&entry.key)?;
        writer.write_all(&entry.value)?;

        Ok(())
    }

    pub fn should_switch_writer_before_write_commit(
        &self,
        entry: &DatafileLog,
    ) -> Result<bool, DatafileError> {
        Ok(self
            .offset
            .checked_add(entry.size()? as u64)
            .unwrap_or(DATAFILE_MAX_SIZE_BYTES as u64)
            >= DATAFILE_MAX_SIZE_BYTES as u64)
    }

    fn move_writer_offset(&mut self, offset: u64) -> Result<(), DatafileError> {
        let new_offset = self
            .offset
            .checked_add(offset)
            .ok_or(DatafileError::WriterFileOffsetOverflow)?;

        if new_offset >= DATAFILE_MAX_SIZE_BYTES as u64 {
            return Err(DatafileError::DatafileFull);
        }

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

    fn generate_filename(dir: &PathBuf, file_id: u32) -> Result<PathBuf, DatafileError> {
        Ok(dir.join(format!("{:08}.{}", file_id, DATAFILE_EXTENSION)))
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct DatafileLogHeader {
    pub crc: u32,        // CRC32 of all fields below
    pub timestamp: u64,  // UNIX Timestamp when the entry was created
    pub key_size: u32,   // Size of the key in bytes
    pub value_size: u32, // Size of the value in bytes
}

impl DatafileLogHeader {
    pub fn new(key: &[u8], value: &[u8]) -> Result<Self, DatafileError> {
        Self::with_timestamp(
            key,
            value,
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64,
        )
    }

    fn with_timestamp(key: &[u8], value: &[u8], timestamp: u64) -> Result<Self, DatafileError> {
        let key_size =
            u32::try_from(key.len()).map_err(|_| DatafileError::OversizedDatafileEntry)?;
        let value_size =
            u32::try_from(value.len()).map_err(|_| DatafileError::OversizedDatafileEntry)?;

        Ok(DatafileLogHeader {
            crc: Self::get_crc_for(timestamp, key_size, value_size, key, value),
            timestamp,
            key_size,
            value_size,
        })
    }

    fn get_crc_for(
        timestamp: u64,
        key_size: u32,
        value_size: u32,
        key: &[u8],
        value: &[u8],
    ) -> u32 {
        let mut hasher = Hasher::new();

        hasher.update(&timestamp.to_be_bytes());
        hasher.update(&key_size.to_be_bytes());
        hasher.update(&value_size.to_be_bytes());

        hasher.update(key);
        hasher.update(value);

        hasher.finalize()
    }

    pub fn encode(&self) -> [u8; DATAFILE_HEADER_SIZE_BYTES] {
        let mut arr: [u8; DATAFILE_HEADER_SIZE_BYTES] = [0; DATAFILE_HEADER_SIZE_BYTES];

        arr[0..4].copy_from_slice(&self.crc.to_be_bytes());
        arr[4..12].copy_from_slice(&self.timestamp.to_be_bytes());
        arr[12..16].copy_from_slice(&self.key_size.to_be_bytes());
        arr[16..20].copy_from_slice(&self.value_size.to_be_bytes());

        arr
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DatafileError> {
        if DATAFILE_HEADER_SIZE_BYTES > bytes.len() {
            return Err(DatafileError::UndersizedDatafileHeader(bytes.len()));
        }

        let crc_bytes: [u8; DATAFILE_HEADER_CRC_SIZE_BYTES] = bytes[0..4].try_into()?;
        let timestamp_bytes: [u8; DATAFILE_HEADER_TIMESTAMP_SIZE_BYTES] =
            bytes[4..12].try_into()?;
        let key_size_bytes: [u8; DATAFILE_HEADER_KSZ_SIZE_BYTES] = bytes[12..16].try_into()?;
        let value_size_bytes: [u8; DATAFILE_HEADER_VSZ_SIZE_BYTES] = bytes[16..20].try_into()?;

        Ok(Self {
            crc: u32::from_be_bytes(crc_bytes),
            timestamp: u64::from_be_bytes(timestamp_bytes),
            key_size: u32::from_be_bytes(key_size_bytes),
            value_size: u32::from_be_bytes(value_size_bytes),
        })
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct DatafileLog {
    pub header: DatafileLogHeader,
    pub key: Box<[u8]>,
    pub value: Box<[u8]>,
}

impl DatafileLog {
    pub fn new(key: &[u8], value: &[u8]) -> Result<Self, DatafileError> {
        Ok(Self {
            header: DatafileLogHeader::new(key, value)?,
            key: Box::from(key),
            value: Box::from(value),
        })
    }
    pub fn with_timestamp(key: &[u8], value: &[u8], timestamp: u64) -> Result<Self, DatafileError> {
        Ok(Self {
            header: DatafileLogHeader::with_timestamp(key, value, timestamp)?,
            key: Box::from(key),
            value: Box::from(value),
        })
    }

    pub fn size(&self) -> Result<usize, DatafileError> {
        let size = u64::from(self.header.key_size)
            .checked_add(u64::from(self.header.value_size))
            .and_then(|kv| kv.checked_add(DATAFILE_HEADER_SIZE_BYTES as u64))
            .ok_or(DatafileError::OversizedDatafileEntry)?;

        usize::try_from(size).map_err(|_| DatafileError::OversizedDatafileEntry)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DatafileError> {
        let datafile_size = bytes.len() as u64;
        let header = DatafileLogHeader::decode(bytes)?;

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
            != DatafileLogHeader::get_crc_for(
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

#[cfg(test)]
mod datafile_tests {
    use crc32fast::Hasher;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::datafile::{
        DATAFILE_HEADER_SIZE_BYTES, DATAFILE_MAX_SIZE_BYTES, Datafile, DatafileError, DatafileLog,
        DatafileLogHeader,
    };

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
            DatafileLogHeader::with_timestamp(&key.to_be_bytes(), &value.to_be_bytes(), timestamp);

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
            DatafileLogHeader::with_timestamp(key.as_bytes(), &value.to_be_bytes(), timestamp)
                .unwrap();

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
            DatafileLogHeader::with_timestamp(key.as_bytes(), &value.to_be_bytes(), timestamp)
                .unwrap();

        let datafile_entry =
            DatafileLog::with_timestamp(key.as_bytes(), &value.to_be_bytes(), timestamp).unwrap();

        assert_eq!(datafile_entry.header, expected_datafile_header);
        assert_eq!(&datafile_entry.key.to_vec(), &key.into_bytes());
        assert_eq!(&datafile_entry.value.to_vec(), &value.to_be_bytes());
    }

    #[test]
    fn datafile_header_encode() {
        let key: String = String::from("hello world");
        let value: u64 = 2;

        let datafile_header = DatafileLogHeader::new(key.as_bytes(), &value.to_be_bytes()).unwrap();

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

        let datafile_entry = DatafileLog::new(key.as_bytes(), &value.to_be_bytes()).unwrap();

        let mut kv: Vec<u8> = Vec::new();
        kv.extend_from_slice(key.as_bytes());
        kv.extend_from_slice(&value.to_be_bytes());

        let mut encoded: Vec<u8> = Vec::new();

        Datafile::write_log_to(&mut encoded, &datafile_entry).unwrap();

        assert_eq!(encoded[0..20], datafile_entry.header.encode());
        assert_eq!(encoded[20..], kv);
    }

    #[test]
    fn datafile_header_decode_ok() {
        let datafile_header =
            DatafileLogHeader::new(String::from("hello world").as_bytes(), &2u64.to_be_bytes())
                .unwrap();

        assert_eq!(
            datafile_header,
            DatafileLogHeader::decode(&datafile_header.encode()).unwrap()
        );
    }

    #[test]
    fn datafile_header_decode_err() {
        let datafile_header =
            DatafileLogHeader::new(String::from("hello world").as_bytes(), &2u64.to_be_bytes())
                .unwrap();

        assert!(matches!(
            DatafileLogHeader::decode(&datafile_header.encode()[0..9]),
            Err(DatafileError::UndersizedDatafileHeader(9))
        ));
    }

    #[test]
    fn datafile_entry_decode_ok() {
        let datafile_entry =
            DatafileLog::new(String::from("hello world").as_bytes(), &2u64.to_be_bytes()).unwrap();

        let mut encoded: Vec<u8> = Vec::new();

        Datafile::write_log_to(&mut encoded, &datafile_entry).unwrap();

        assert_eq!(datafile_entry, DatafileLog::decode(&encoded).unwrap());
    }

    #[test]
    fn datafile_entry_decode_err() {
        let datafile_entry =
            DatafileLog::new(String::from("hello world").as_bytes(), &2u64.to_be_bytes()).unwrap();

        let mut encoded: Vec<u8> = Vec::new();

        Datafile::write_log_to(&mut encoded, &datafile_entry).unwrap();

        assert!(matches!(
            DatafileLog::decode(&encoded[0..DATAFILE_HEADER_SIZE_BYTES + 5]),
            Err(DatafileError::UndersizedDatafileKey { .. })
        ));

        assert!(matches!(
            DatafileLog::decode(&encoded[..encoded.len() - 1]),
            Err(DatafileError::UndersizedDatafileValue { .. })
        ));
    }

    #[test]
    fn datafile_entry_decode_err_crc_mismatch() {
        let datafile_entry =
            DatafileLog::new(String::from("hello world").as_bytes(), &2u64.to_be_bytes()).unwrap();

        let mut encoded: Vec<u8> = Vec::new();
        Datafile::write_log_to(&mut encoded, &datafile_entry).unwrap();

        let last = encoded.len();
        encoded[last - 1] = 0u8;

        assert!(matches!(
            DatafileLog::decode(&encoded),
            Err(DatafileError::CrcMismatch)
        ));
    }

    #[test]
    fn datafile_entry_decode_corrupt_sizes() {
        let mut bytes = vec![0u8; DATAFILE_HEADER_SIZE_BYTES];

        bytes[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            DatafileLog::decode(&bytes),
            Err(DatafileError::UndersizedDatafileKey { .. })
        ));

        bytes[12..16].copy_from_slice(&0u32.to_be_bytes());
        bytes[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            DatafileLog::decode(&bytes),
            Err(DatafileError::UndersizedDatafileValue { .. })
        ));
    }

    #[test]
    fn size_err() {
        let datafile_entry =
            DatafileLog::new(String::from("hello world").as_bytes(), &2u64.to_be_bytes()).unwrap();

        assert!(datafile_entry.size().is_ok());
    }

    #[test]
    fn datafile_write_log() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut df = Datafile::new(temp_dir.path().to_path_buf(), 1).unwrap();

        let datafile_entry =
            DatafileLog::new("hello world".as_bytes(), &2u32.to_be_bytes()).unwrap();
        let entry_size = datafile_entry.size().unwrap();

        let offset = df.write_log(&datafile_entry).unwrap();
        assert_eq!(offset, 0);
        assert_eq!(df.offset, entry_size as u64);

        let mut expected = Vec::new();
        Datafile::write_log_to(&mut expected, &datafile_entry).unwrap();
        let read_bytes = df.read_bytes(offset, entry_size).unwrap();
        assert_eq!(&*read_bytes, &expected[..]);
    }

    #[test]
    fn datafile_move_writer_offset() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut df = Datafile::new(temp_dir.path().to_path_buf(), 1).unwrap();

        let datafile_entry =
            DatafileLog::new("hello world".as_bytes(), &2u32.to_be_bytes()).unwrap();
        let entry_size = datafile_entry.size().unwrap() as u64;

        assert_eq!(df.offset, 0);
        let offset = df.write_log(&datafile_entry).unwrap();
        assert_eq!(offset, 0);
        assert_eq!(df.offset, entry_size);
    }

    #[test]
    fn datafile_non_writer_write_err() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut df = Datafile::new(temp_dir.path().to_path_buf(), 1).unwrap();
        df.demote_to_reader();

        let datafile_entry =
            DatafileLog::new("hello world".as_bytes(), &2u32.to_be_bytes()).unwrap();

        assert!(matches!(
            df.write_log(&datafile_entry),
            Err(DatafileError::WriteAttemptToReaderDatafile)
        ));
    }

    #[test]
    fn datafile_write_err_full() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut df = Datafile::new(temp_dir.path().to_path_buf(), 1).unwrap();
        df.offset = DATAFILE_MAX_SIZE_BYTES as u64;

        let datafile_entry =
            DatafileLog::new("hello world".as_bytes(), &2u32.to_be_bytes()).unwrap();

        assert!(matches!(
            df.write_log(&datafile_entry),
            Err(DatafileError::DatafileFull)
        ));
    }

    #[test]
    fn move_writer_offset_err_ovf() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut df = Datafile::new(temp_dir.path().to_path_buf(), 1).unwrap();
        df.offset = u64::MAX;

        assert!(matches!(
            df.move_writer_offset(1),
            Err(DatafileError::WriterFileOffsetOverflow)
        ));
    }
}
