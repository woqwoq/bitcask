use std::{path::PathBuf, process};

use bitcask::{
    engine::BitcaskInstance,
    options::{BitcaskOptions, DatafileMaxSize, MB},
};

fn main() {
    let options = BitcaskOptions::default()
        .merge_dead_space_threshold_percent(49)
        .merge_dead_space_min_bytes((48 * MB) as u64)
        .expected_keydir_max_size(10_000_000)
        .datafile_max_size(DatafileMaxSize::SoftCappedBytes(64 * MB));

    const ENTRY_COUNT: usize = 10_000_000;

    match BitcaskInstance::open(PathBuf::from("db"), options) {
        Ok(mut db) => {
            for i in 0..ENTRY_COUNT as u128 {
                db.put(&i.to_be_bytes(), format!("{i}").as_bytes()).unwrap();
            }
        }
        Err(e) => {
            eprintln!("{e:?}");
            process::exit(1);
        }
    }
}
