use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::FileExt,
    path::Path,
};

use crc32fast::Hasher;

use crate::{
    datafile::{Datafile, DatafileRecordIteratorEntry},
    error::HintfileError,
    record::DatafileRecord,
};

const HINTFILE_RECORD_HEADER_SIZE_BYTES: usize = 28;

const HINTFILE_HEADER_CRC_SIZE_BYTES: usize = 4;
const HINTFILE_HEADER_TIMESTAMP_SIZE_BYTES: usize = 8;
const HINTFILE_HEADER_KEY_SIZE_SIZE_BYTES: usize = 4;
const HINTFILE_HEADER_VALUE_SIZE_SIZE_BYTES: usize = 4;
const HINTFILE_HEADER_VALUE_OFFSET_SIZE_BYTES: usize = 8;

#[derive(Debug, PartialEq, Clone)]
pub struct HintfileRecordHeader {
    pub crc: u32,          // CRC32 of all fields below
    pub timestamp: u64,    // UNIX Timestamp of the original entry
    pub key_size: u32,     // Size of the key in bytes
    pub value_size: u32,   // Size of the value in bytes
    pub value_offset: u64, // Offset of the value in the source datafile
}

impl HintfileRecordHeader {
    pub fn from_datafile_record(record: &DatafileRecord, value_offset: u64) -> Self {
        let crc = Self::crc_for(
            record.header.timestamp,
            record.header.key_size,
            record.header.value_size,
            value_offset,
            &record.key,
        );

        Self {
            crc,
            timestamp: record.header.timestamp,
            key_size: record.header.key_size,
            value_size: record.header.value_size,
            value_offset,
        }
    }

    fn crc_for(
        timestamp: u64,
        key_size: u32,
        value_size: u32,
        value_offset: u64,
        key: &[u8],
    ) -> u32 {
        let mut hasher = Hasher::new();

        hasher.update(&timestamp.to_be_bytes());
        hasher.update(&key_size.to_be_bytes());
        hasher.update(&value_size.to_be_bytes());
        hasher.update(&value_offset.to_be_bytes());
        hasher.update(key);

        hasher.finalize()
    }

    pub fn encode(&self) -> [u8; HINTFILE_RECORD_HEADER_SIZE_BYTES] {
        let mut arr: [u8; HINTFILE_RECORD_HEADER_SIZE_BYTES] =
            [0; HINTFILE_RECORD_HEADER_SIZE_BYTES];

        arr[0..4].copy_from_slice(&self.crc.to_be_bytes());
        arr[4..12].copy_from_slice(&self.timestamp.to_be_bytes());
        arr[12..16].copy_from_slice(&self.key_size.to_be_bytes());
        arr[16..20].copy_from_slice(&self.value_size.to_be_bytes());
        arr[20..28].copy_from_slice(&self.value_offset.to_be_bytes());

        arr
    }

    pub fn decode(buf: &[u8]) -> Result<Self, HintfileError> {
        let crc: [u8; HINTFILE_HEADER_CRC_SIZE_BYTES] = buf[0..4].try_into()?;
        let timestamp: [u8; HINTFILE_HEADER_TIMESTAMP_SIZE_BYTES] = buf[4..12].try_into()?;
        let key_size: [u8; HINTFILE_HEADER_KEY_SIZE_SIZE_BYTES] = buf[12..16].try_into()?;
        let value_size: [u8; HINTFILE_HEADER_VALUE_SIZE_SIZE_BYTES] = buf[16..20].try_into()?;
        let value_offset: [u8; HINTFILE_HEADER_VALUE_OFFSET_SIZE_BYTES] = buf[20..28].try_into()?;

        Ok(Self {
            crc: u32::from_be_bytes(crc),
            timestamp: u64::from_be_bytes(timestamp),
            key_size: u32::from_be_bytes(key_size),
            value_size: u32::from_be_bytes(value_size),
            value_offset: u64::from_be_bytes(value_offset),
        })
    }

    pub fn crc(&self) -> Hasher {
        let mut hasher = Hasher::new();

        hasher.update(&self.timestamp.to_be_bytes());
        hasher.update(&self.key_size.to_be_bytes());
        hasher.update(&self.value_size.to_be_bytes());
        hasher.update(&self.value_offset.to_be_bytes());

        hasher
    }

    pub fn size(&self) -> Result<usize, HintfileError> {
        let size = u64::from(self.key_size)
            .checked_add(HINTFILE_RECORD_HEADER_SIZE_BYTES as u64)
            .ok_or(HintfileError::OversizedHintfileRecord)?;

        usize::try_from(size).map_err(|_| HintfileError::OversizedHintfileRecord)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct HintfileRecord {
    pub header: HintfileRecordHeader, // Header
    pub key: Box<[u8]>,               // The actual key
}

impl HintfileRecord {
    pub fn from_datafile_record(df_record: DatafileRecord, value_offset: u64) -> Self {
        Self {
            header: HintfileRecordHeader::from_datafile_record(&df_record, value_offset),
            key: df_record.key,
        }
    }
}

pub struct Hintfile {
    file: File,
    pub id: u32,
}

impl Hintfile {
    pub fn new(dir: &Path, id: u32, datafile_extension: &str) -> Result<Self, HintfileError> {
        let filename = Datafile::generate_filename(dir, id, datafile_extension);

        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(filename)?;

        Ok(Self { file, id })
    }
    pub fn read_bytes(&self, offset: u64, count: usize) -> Result<Box<[u8]>, HintfileError> {
        let mut buf = vec![0u8; count];

        self.file.read_exact_at(&mut buf, offset)?;

        Ok(Box::from(buf))
    }

    pub fn append_hint_record(
        &mut self,
        df_record: DatafileRecord,
        value_offset: u64,
    ) -> Result<(), HintfileError> {
        Self::write_hint_record_to(
            &mut self.file,
            &HintfileRecord::from_datafile_record(df_record, value_offset),
        )?;

        Ok(())
    }

    fn write_hint_record_to<W: Write>(
        writer: &mut W,
        record: &HintfileRecord,
    ) -> Result<(), HintfileError> {
        writer.write_all(&record.header.encode())?;
        writer.write_all(&record.key)?;

        Ok(())
    }

    pub fn file_size(&self) -> Result<u64, HintfileError> {
        Ok(self.file.metadata()?.len())
    }
}

pub struct HintfileIterator<'a> {
    hintfile: &'a Hintfile,
    fsz: u64,
    offset: u64,
}

impl<'a> HintfileIterator<'a> {
    pub fn new(hintfile: &'a Hintfile) -> Result<Self, HintfileError> {
        let fsz = hintfile.file_size()?;
        Ok(Self {
            hintfile,
            fsz,
            offset: 0,
        })
    }

    pub fn next_record(&mut self) -> Result<Option<HintfileRecord>, HintfileError> {
        if self.offset >= self.fsz {
            return Ok(None);
        }

        let header = HintfileRecordHeader::decode(
            &self
                .hintfile
                .read_bytes(self.offset, HINTFILE_RECORD_HEADER_SIZE_BYTES)?,
        )?;

        let key = self.hintfile.read_bytes(
            self.offset + HINTFILE_RECORD_HEADER_SIZE_BYTES as u64,
            header.key_size as usize,
        )?;

        self.offset += header.size()? as u64;

        Ok(Some(HintfileRecord { header, key }))
    }
}

#[cfg(test)]
mod test {
    mod record {
        use crate::{
            datafile::DatafileRecordIteratorEntry,
            hint::{HINTFILE_RECORD_HEADER_SIZE_BYTES, HintfileRecord, HintfileRecordHeader},
            record::{DatafileRecord, DatafileRecordHeader},
        };

        #[test]
        fn header_from_datafile_record_iterator_entry() {
            let key = b"1";
            let value = 1u32.to_be_bytes();

            let df_record = DatafileRecord::new(key, &value).unwrap();
            let value_offset = 142u64;

            let hintfile_header =
                HintfileRecordHeader::from_datafile_record(&df_record, value_offset);

            let expected_crc = HintfileRecordHeader::crc_for(
                df_record.header.timestamp,
                df_record.header.key_size,
                df_record.header.value_size,
                value_offset,
                key,
            );

            assert_eq!(hintfile_header.timestamp, df_record.header.timestamp);
            assert_eq!(hintfile_header.key_size, df_record.header.key_size);
            assert_eq!(hintfile_header.value_size, df_record.header.value_size);
            assert_eq!(hintfile_header.value_offset, value_offset);
            assert_eq!(hintfile_header.crc, expected_crc);
        }

        #[test]
        fn header_encode_decode() {
            let key = b"1";
            let value = 1u32.to_be_bytes();

            let df_record = DatafileRecord::new(key, &value).unwrap();
            let value_offset = 142u64;

            let hintfile_header =
                HintfileRecordHeader::from_datafile_record(&df_record, value_offset);

            let mut expected_encoding: Vec<u8> = Vec::new();
            expected_encoding.extend_from_slice(&hintfile_header.crc.to_be_bytes());
            expected_encoding.extend_from_slice(&hintfile_header.timestamp.to_be_bytes());
            expected_encoding.extend_from_slice(&hintfile_header.key_size.to_be_bytes());
            expected_encoding.extend_from_slice(&hintfile_header.value_size.to_be_bytes());
            expected_encoding.extend_from_slice(&hintfile_header.value_offset.to_be_bytes());

            assert_eq!(hintfile_header.encode(), expected_encoding.as_slice());
            assert_eq!(
                HintfileRecordHeader::decode(&expected_encoding).unwrap(),
                hintfile_header
            );
            assert_eq!(
                HintfileRecordHeader::decode(&hintfile_header.encode()).unwrap(),
                hintfile_header
            );
        }

        #[test]
        fn crc() {
            let key = b"1";
            let value = 1u32.to_be_bytes();

            let df_record = DatafileRecord::new(key, &value).unwrap();
            let value_offset = 142u64;

            let hintfile_header =
                HintfileRecordHeader::from_datafile_record(&df_record, value_offset);

            let mut hasher = hintfile_header.crc();
            hasher.update(key);

            assert_eq!(hasher.finalize(), hintfile_header.crc);
        }

        #[test]
        fn size() {
            let key = b"1";
            let value = 1u32.to_be_bytes();

            let df_record = DatafileRecord::new(key, &value).unwrap();
            let value_offset = 142u64;

            let hintfile_header =
                HintfileRecordHeader::from_datafile_record(&df_record, value_offset);

            assert_eq!(
                hintfile_header.size().unwrap(),
                HINTFILE_RECORD_HEADER_SIZE_BYTES + 1
            );
        }

        #[test]
        fn record_from_datafile_record_iterator_entry() {
            let key = b"1";
            let value = 1u32.to_be_bytes();

            let df_record = DatafileRecord::new(key, &value).unwrap();
            let value_offset = 142u64;

            let hintfile_header =
                HintfileRecordHeader::from_datafile_record(&df_record, value_offset);

            let hintfile_record = HintfileRecord::from_datafile_record(df_record, value_offset);

            assert_eq!(hintfile_record.header, hintfile_header);
            assert_eq!(hintfile_record.key.to_vec(), Vec::from(key));
        }
    }
}
