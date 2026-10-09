use std::{
    fmt::Display,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};

use crate::{
    error::DatafileError,
    keydir::KeyDir,
    options::{DatafileMaxSize, SyncMode},
    record::{DatafileRecord, DatafileRecordHeader},
};

pub const DATAFILE_HEADER_SIZE_BYTES: usize = 21;
pub const DATAFILE_HEADER_RECORD_TYPE_SIZE_BYTES: usize = 1;
pub const DATAFILE_HEADER_CRC_SIZE_BYTES: usize = 4;
pub const DATAFILE_HEADER_TIMESTAMP_SIZE_BYTES: usize = 8;
pub const DATAFILE_HEADER_KSZ_SIZE_BYTES: usize = 4;
pub const DATAFILE_HEADER_VSZ_SIZE_BYTES: usize = 4;

pub const DATAFILE_RECORD_TYPE_NORMAL: u8 = 0;
pub const DATAFILE_RECORD_TYPE_TOMBSTONE: u8 = 1;

pub const DATAFILE_VALUE_STREAM_CHUNK_SIZE_BYTES: usize = 4096;

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
            .append(is_writer)
            .create(false)
            .open(filename)?;

        Ok(Datafile {
            file,
            id,
            offset,
            is_writer,
        })
    }

    pub fn open_datafile_as_writer(
        dir: &Path,
        id: u32,
        offset: u64,
        datafile_extension: &str,
    ) -> Result<Datafile, DatafileError> {
        Self::open_datafile(dir, id, datafile_extension, offset, true, true)
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

#[cfg(test)]
mod tests {
    use std::io::Seek;

    use crate::{
        datafile::{Datafile, DatafileError, DatafileRecord},
        keydir::KeyDir,
        options::BitcaskOptions,
    };

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
        fn open_datafile_as_writer_missing_file_err() {
            let temp_dir = tempfile::tempdir().unwrap();
            let options = BitcaskOptions::default();
            let path = Datafile::generate_filename(temp_dir.path(), 1, &options.datafile_extension)
                .unwrap();

            let result = Datafile::open_datafile_as_writer(
                temp_dir.path(),
                1,
                0,
                &options.datafile_extension,
            );

            assert!(matches!(
                result,
                Err(DatafileError::IO(e)) if e.kind() == std::io::ErrorKind::NotFound
            ));
            assert!(!path.exists());
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
