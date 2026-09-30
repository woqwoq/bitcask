use std::collections::HashMap;

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
