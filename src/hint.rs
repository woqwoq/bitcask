use std::{
    fs::{File, OpenOptions},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    os::unix::fs::FileExt,
    path::Path,
};

use crc32fast::Hasher;

use crate::{
    datafile::Datafile,
    error::HintfileError,
    keydir::KeyDir,
    options::{READER_BUFFER_SIZE, WRITER_BUFFER_SIZE},
    record::DatafileRecord,
};

pub(crate) const HINTFILE_RECORD_HEADER_SIZE_BYTES: usize = 28;

const HINTFILE_HEADER_CRC_SIZE_BYTES: usize = 4;
const HINTFILE_HEADER_TIMESTAMP_SIZE_BYTES: usize = 8;
const HINTFILE_HEADER_KEY_SIZE_SIZE_BYTES: usize = 4;
const HINTFILE_HEADER_VALUE_SIZE_SIZE_BYTES: usize = 4;
const HINTFILE_HEADER_VALUE_OFFSET_SIZE_BYTES: usize = 8;

#[derive(Debug, PartialEq, Clone)]
pub(crate) struct HintfileRecordHeader {
    pub(crate) crc: u32,          // CRC32 of all fields below
    pub(crate) timestamp: u64,    // UNIX Timestamp of the original entry
    pub(crate) key_size: u32,     // Size of the key in bytes
    pub(crate) value_size: u32,   // Size of the value in bytes
    pub(crate) value_offset: u64, // Offset of the value in the source datafile
}

impl HintfileRecordHeader {
    pub(crate) fn from_datafile_record(record: &DatafileRecord, value_offset: u64) -> Self {
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

    pub(crate) fn encode(&self) -> [u8; HINTFILE_RECORD_HEADER_SIZE_BYTES] {
        let mut arr: [u8; HINTFILE_RECORD_HEADER_SIZE_BYTES] =
            [0; HINTFILE_RECORD_HEADER_SIZE_BYTES];

        arr[0..4].copy_from_slice(&self.crc.to_be_bytes());
        arr[4..12].copy_from_slice(&self.timestamp.to_be_bytes());
        arr[12..16].copy_from_slice(&self.key_size.to_be_bytes());
        arr[16..20].copy_from_slice(&self.value_size.to_be_bytes());
        arr[20..28].copy_from_slice(&self.value_offset.to_be_bytes());

        arr
    }

    pub(crate) fn decode(buf: &[u8]) -> Result<Self> {
        if HINTFILE_RECORD_HEADER_SIZE_BYTES > buf.len() {
            return Err(HintfileError::UndersizedHintfileHeader(buf.len()));
        }

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

    pub(crate) fn crc(&self) -> Hasher {
        let mut hasher = Hasher::new();

        hasher.update(&self.timestamp.to_be_bytes());
        hasher.update(&self.key_size.to_be_bytes());
        hasher.update(&self.value_size.to_be_bytes());
        hasher.update(&self.value_offset.to_be_bytes());

        hasher
    }

    pub(crate) fn size(&self) -> Result<usize> {
        let size = u64::from(self.key_size)
            .checked_add(HINTFILE_RECORD_HEADER_SIZE_BYTES as u64)
            .ok_or(HintfileError::OversizedHintfileRecord)?;

        usize::try_from(size).map_err(|_| HintfileError::OversizedHintfileRecord)
    }
}

#[allow(dead_code)]
#[derive(Debug, PartialEq, Clone)]
pub(crate) struct HintfileRecord {
    pub(crate) header: HintfileRecordHeader, // Header
    pub(crate) key: Box<[u8]>,               // The actual key
}

#[allow(dead_code)]
impl HintfileRecord {
    pub(crate) fn from_datafile_record(df_record: &DatafileRecord, value_offset: u64) -> Self {
        Self {
            header: HintfileRecordHeader::from_datafile_record(df_record, value_offset),
            key: df_record.key.clone(),
        }
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let hintfile_size = bytes.len() as u64;
        let header = HintfileRecordHeader::decode(bytes)?;

        let expected_datafile_size_with_key =
            HINTFILE_RECORD_HEADER_SIZE_BYTES as u64 + u64::from(header.key_size);

        if expected_datafile_size_with_key > hintfile_size {
            return Err(HintfileError::UndersizedHintfileKey {
                expected: expected_datafile_size_with_key as usize,
                actual: hintfile_size as usize,
            });
        }

        let key = &bytes[HINTFILE_RECORD_HEADER_SIZE_BYTES
            ..HINTFILE_RECORD_HEADER_SIZE_BYTES + header.key_size as usize];

        Ok(Self {
            header,
            key: Box::from(key),
        })
    }
}

type Result<R> = core::result::Result<R, HintfileError>;

pub(crate) struct Hintfile {
    file: BufWriter<File>,
    pub(crate) id: u32,
}

#[hotpath::measure_all]
impl Hintfile {
    pub(crate) fn new_writer(dir: &Path, id: u32, hintfile_extension: &str) -> Result<Self> {
        let filename = Datafile::generate_filename(dir, id, hintfile_extension);

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(filename)
            .map_err(HintfileError::Open)?;

        Ok(Self {
            file: BufWriter::with_capacity(WRITER_BUFFER_SIZE, file),
            id,
        })
    }

    pub(crate) fn replay(&self, keydir: &mut KeyDir) -> Result<usize> {
        let fsz = self.file_size()?;
        let mut start_offset = 0;
        let mut count: usize = 0;

        let mut reader = BufReader::with_capacity(READER_BUFFER_SIZE, self.file.get_ref());
        reader
            .seek(SeekFrom::Start(0))
            .map_err(HintfileError::Read)?;

        while start_offset < fsz {
            let header = HintfileRecordHeader::decode(&Self::read_exact_bytes(
                &mut reader,
                HINTFILE_RECORD_HEADER_SIZE_BYTES,
            )?)?;

            if header.key_size as u64 >= fsz {
                return Err(HintfileError::PayloadSizeGreaterThanHintfile {
                    file_size: fsz,
                    declared: header.key_size as usize,
                });
            }

            let mut hasher = header.crc();

            let key = Self::read_exact_bytes(&mut reader, header.key_size as usize)?;

            hasher.update(&key);

            if hasher.finalize() != header.crc {
                return Err(HintfileError::CrcMismatch);
            }

            keydir.put(
                key,
                self.id,
                header.value_size,
                header.value_offset,
                header.timestamp,
            );
            count += 1;

            start_offset += header.size()? as u64;
        }

        Ok(count)
    }

    pub(crate) fn open_if_exists(
        dir: &Path,
        id: u32,
        hintfile_extension: &str,
    ) -> Result<Option<Self>> {
        let filename = Datafile::generate_filename(dir, id, hintfile_extension);

        if !filename.exists() {
            return Ok(None);
        }

        let file = OpenOptions::new()
            .read(true)
            .write(false)
            .create(false)
            .open(filename)
            .map_err(HintfileError::Open)?;

        Ok(Some(Self {
            file: BufWriter::with_capacity(WRITER_BUFFER_SIZE, file),
            id,
        }))
    }

    #[allow(dead_code)]
    fn read_exact_bytes_at(&self, offset: u64, count: usize) -> Result<Box<[u8]>> {
        let mut buf = vec![0u8; count];

        self.file
            .get_ref()
            .read_exact_at(&mut buf, offset)
            .map_err(HintfileError::Read)?;

        Ok(Box::from(buf))
    }

    fn read_exact_bytes<R: Read>(reader: &mut R, count: usize) -> Result<Box<[u8]>> {
        let mut buf = vec![0u8; count];

        reader.read_exact(&mut buf).map_err(HintfileError::Read)?;

        Ok(Box::from(buf))
    }

    pub(crate) fn append_hint_record(
        &mut self,
        df_record: &DatafileRecord,
        value_offset: u64,
    ) -> Result<()> {
        // Instead of building HintfileRecord and copying the key, we write it disassembled into
        // header and key from the beginning.
        Self::write_hint_record_to(
            &mut self.file,
            HintfileRecordHeader::from_datafile_record(df_record, value_offset),
            &df_record.key,
        )?;

        Ok(())
    }

    fn write_hint_record_to<W: Write>(
        writer: &mut W,
        header: HintfileRecordHeader,
        key: &[u8],
    ) -> Result<()> {
        writer
            .write_all(&header.encode())
            .map_err(HintfileError::Write)?;
        writer.write_all(key).map_err(HintfileError::Write)?;

        Ok(())
    }

    pub(crate) fn file_size(&self) -> Result<u64> {
        Ok(self
            .file
            .get_ref()
            .metadata()
            .map_err(HintfileError::Read)?
            .len())
    }

    pub(crate) fn sync(&mut self) -> Result<()> {
        self.file.get_mut().sync_data().map_err(HintfileError::Sync)
    }

    pub(crate) fn flush(&mut self) -> Result<()> {
        self.file.flush().map_err(HintfileError::Flush)?;
        Ok(())
    }
}

#[allow(dead_code)]
pub(crate) struct HintfileIterator<'a> {
    hintfile: &'a Hintfile,
    fsz: u64,
    offset: u64,
}

#[allow(dead_code)]
impl<'a> HintfileIterator<'a> {
    pub(crate) fn new(hintfile: &'a Hintfile) -> Result<Self> {
        let fsz = hintfile.file_size()?;
        Ok(Self {
            hintfile,
            fsz,
            offset: 0,
        })
    }

    pub(crate) fn next_record(&mut self) -> Result<Option<HintfileRecord>> {
        if self.offset >= self.fsz {
            return Ok(None);
        }

        let header = HintfileRecordHeader::decode(
            &self
                .hintfile
                .read_exact_bytes_at(self.offset, HINTFILE_RECORD_HEADER_SIZE_BYTES)?,
        )?;

        let key = self.hintfile.read_exact_bytes_at(
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
            hint::{HINTFILE_RECORD_HEADER_SIZE_BYTES, HintfileRecord, HintfileRecordHeader},
            record::DatafileRecord,
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

            let hintfile_record = HintfileRecord::from_datafile_record(&df_record, value_offset);

            assert_eq!(hintfile_record.header, hintfile_header);
            assert_eq!(hintfile_record.key.to_vec(), Vec::from(key));
        }
    }

    mod hintfile {
        use crate::{
            hint::{HINTFILE_RECORD_HEADER_SIZE_BYTES, Hintfile, HintfileRecord},
            keydir::KeyDir,
            record::DatafileRecord,
        };
        const HINTFILE_EXTENSION: &str = "hint";

        #[test]
        fn new() {
            let temp_dir = tempfile::tempdir().unwrap();

            {
                let hintfile =
                    Hintfile::new_writer(temp_dir.path(), 1, HINTFILE_EXTENSION).unwrap();

                assert_eq!(hintfile.id, 1);
                assert_eq!(hintfile.file_size().unwrap(), 0);
            }

            assert!(Hintfile::new_writer(temp_dir.path(), 1, HINTFILE_EXTENSION).is_ok());
        }

        #[test]
        fn append_read() {
            let temp_dir = tempfile::tempdir().unwrap();
            const RECORD_SIZE_BYTES: usize = HINTFILE_RECORD_HEADER_SIZE_BYTES + 4;
            let mut records: Vec<DatafileRecord> = Vec::new();

            {
                let mut hintfile =
                    Hintfile::new_writer(temp_dir.path(), 1, HINTFILE_EXTENSION).unwrap();

                let mut offset = 0u64;

                for i in 0..100u32 {
                    let record = DatafileRecord::new(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
                    let ksz = record.header.key_size;
                    records.push(record.clone());

                    assert!(hintfile.append_hint_record(&record, offset).is_ok());
                    offset += HINTFILE_RECORD_HEADER_SIZE_BYTES as u64 + ksz as u64;
                }

                hintfile.flush().unwrap();
            }

            let hintfile = Hintfile::open_if_exists(temp_dir.path(), 1, HINTFILE_EXTENSION)
                .unwrap()
                .unwrap();

            let mut offset = 0u64;
            for expected in records {
                let bytes = hintfile
                    .read_exact_bytes_at(offset, RECORD_SIZE_BYTES)
                    .unwrap();
                let record = HintfileRecord::decode(&bytes).unwrap();

                assert_eq!(
                    record,
                    HintfileRecord::from_datafile_record(&expected, offset)
                );

                offset += HINTFILE_RECORD_HEADER_SIZE_BYTES as u64 + record.header.key_size as u64;
            }
        }

        #[test]
        fn replay() {
            let temp_dir = tempfile::tempdir().unwrap();
            let mut records: Vec<HintfileRecord> = Vec::new();
            let mut keydir = KeyDir::default();

            assert!(
                Hintfile::open_if_exists(temp_dir.path(), 2, HINTFILE_EXTENSION)
                    .unwrap()
                    .is_none()
            );

            {
                let mut hintfile =
                    Hintfile::new_writer(temp_dir.path(), 1, HINTFILE_EXTENSION).unwrap();

                let mut offset = 0u64;

                for i in 0..100u32 {
                    let record = DatafileRecord::new(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
                    let ksz = record.header.key_size;

                    assert!(hintfile.append_hint_record(&record, offset).is_ok());
                    records.push(HintfileRecord::from_datafile_record(&record, offset));
                    offset += HINTFILE_RECORD_HEADER_SIZE_BYTES as u64 + ksz as u64;
                }
            }

            let hintfile = Hintfile::open_if_exists(temp_dir.path(), 1, HINTFILE_EXTENSION)
                .unwrap()
                .unwrap();

            assert!(hintfile.replay(&mut keydir).is_ok());

            for record in records {
                assert!(keydir.contains(&record.key));
                let kde = keydir.get(&record.key).unwrap();

                assert_eq!(kde.file_id, hintfile.id);
                assert_eq!(kde.value_size, record.header.value_size);
                assert_eq!(kde.value_offset, record.header.value_offset);
                assert_eq!(kde.timestamp, record.header.timestamp);
            }
        }
    }
}
