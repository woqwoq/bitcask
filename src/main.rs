use std::{path::PathBuf, process};

use crate::{
    engine::BitcaskInstance,
    options::{BitcaskOptions, DatafileMaxSize},
};

mod datafile;
mod engine;
mod error;
mod hint;
mod keydir;
mod options;
mod record;

fn main() {
    let options = BitcaskOptions::default()
        .merge_dead_space_threshold_percent(70)
        .merge_dead_space_min_bytes(50000)
        .datafile_max_size(DatafileMaxSize::SoftCappedBytes(64 * 1024 * 1024));

    const ENTRY_COUNT: usize = 40000;

    match BitcaskInstance::open(PathBuf::from("db"), options) {
        Ok(mut db) => {
            for i in 0..ENTRY_COUNT as u32 {
                db.put(&i.to_be_bytes(), format!("{i}").as_bytes()).unwrap();
            }
        }
        Err(e) => {
            eprintln!("{e:?}");
            process::exit(1);
        }
    }
}
