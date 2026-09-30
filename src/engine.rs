use std::{fs::File, path::PathBuf};

use lru::LruCache;

use crate::{datafile::DatafileError, keydir::KeyDir};

pub enum BitcaskError {
    DatafileCreation(DatafileError),
}

pub struct BitcaskInstance {
    keydir: KeyDir,                    // In-memory keydir map
    active_file: File,                 // Writer file handle
    active_file_id: u32,               // Writer file id
    active_file_offset: u64,           // Writer file offset
    dir_path: PathBuf,                 // For opening files through LRU
    reader_files: LruCache<u32, File>, // Use LRU cache to track recently used files
}
