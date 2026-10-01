use std::{
    fs::File,
    io::{Error, Write},
    path::PathBuf,
};

use lru::LruCache;

use crate::{
    datafile::{DATAFILE_HEADER_SIZE_BYTES, DatafileEntry, DatafileError},
    keydir::KeyDir,
};

pub enum BitcaskError {
    DatafileCreation(DatafileError),
    IOWrite(Error),

    WriterFileOffsetOverflow,
}

impl From<Error> for BitcaskError {
    fn from(value: Error) -> Self {
        Self::IOWrite(value)
    }
}

impl From<DatafileError> for BitcaskError {
    fn from(value: DatafileError) -> Self {
        Self::DatafileCreation(value)
    }
}

pub struct BitcaskInstance {
    keydir: KeyDir,                    // In-memory keydir map
    active_file: File,                 // Writer file handle
    active_file_id: u32,               // Writer file id
    active_file_offset: u64,           // Writer file offset
    dir_path: PathBuf,                 // For opening files through LRU
    reader_files: LruCache<u32, File>, // Use LRU cache to track recently used files
}

impl BitcaskInstance {
    pub fn open(dir_path: PathBuf) -> Result<Self, BitcaskError> {
        todo!()
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), BitcaskError> {
        let entry = DatafileEntry::new(key, value)?;
        let entry_size = entry.size()?;

        self.commit_datafile_write(&entry)?; // Write log first, so that it can be
        // replayed in case of a crash.

        self.commit_keydir_write(entry)?; // Append entry to the keydir
        self.move_writer_offset(entry_size)?; // Move the writer offset to suit the new entry

        Ok(())
    }

    fn commit_keydir_write(&mut self, datafile: DatafileEntry) -> Result<(), BitcaskError> {
        let ksz = datafile.header.key_size;

        self.keydir.put(
            datafile,
            self.active_file_id,
            DATAFILE_HEADER_SIZE_BYTES as u64 + ksz as u64 + self.active_file_offset,
        );

        Ok(())
    }

    fn move_writer_offset(&mut self, entry_size: usize) -> Result<(), BitcaskError> {
        self.active_file_offset = self
            .active_file_offset
            .checked_add(entry_size as u64)
            .ok_or(BitcaskError::WriterFileOffsetOverflow)?;
    }

    fn commit_datafile_write(&mut self, datafile: &DatafileEntry) -> Result<(), BitcaskError> {
        self.active_file
            .write_all(&datafile.encode()?)
            .map_err(BitcaskError::IOWrite)
    }
}
