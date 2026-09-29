use std::{collections::HashMap, fs::File, path::PathBuf};

use lru::LruCache;

// We are using explicit types to have platform-agnostic behaviors on both 32 and 64 bit CPUs
pub struct KeyDirEntry {
    file_id: u32,      // ID (e.g. 00000001.data) of the file that contains the given value
    value_size: u32,   // Size of the value in bytes
    value_offset: u64, // Absolute byyte offset to the location of the value in the  datafile
    timestamp: u64,    // UNIX Timestamp when the entry was created
}

pub struct KeyDir {
    dir: HashMap<Box<[u8]>, KeyDirEntry>, // We are using Box<[u8]> instead of a vector to save 8
                                          // bytes per key on the `capacity` field that vector would store
}

const DATAFILE_HEADER_SIZE_BYTES: u8 = 20;
pub struct DatafileHeader {
    crc: u32,        // CRC32 of all fields below
    timestamp: u64,  // UNIX Timestamp when the entry was created
    key_size: u32,   // Size of the key in bytes
    value_size: u32, // Size of the value in bytes
}

pub struct DatafileEntry {
    header: DatafileHeader,
    key: Box<[u8]>,
    value: Box<[u8]>,
}

pub struct BitcaskInstance {
    keydir: KeyDir,                    // In-memory keydir map
    active_file: File,                 // Writer file handle
    active_file_id: u32,               // Writer file id
    active_file_offset: u64,           // Writer file offset
    dir_path: PathBuf,                 // For opening files through LRU
    reader_files: LruCache<u32, File>, // Use LRU cache to track recently used files
}
