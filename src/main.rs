use std::{env, path::PathBuf, process};

use bitcask::{
    db::KeyValueDatabase,
    options::{BitcaskOptions, DatafileMaxSize, MB},
    BitcaskInstance,
};

#[hotpath::main]
fn main() {
    let args: Vec<String> = env::args().collect();

    let mut options = BitcaskOptions::default()
        .merge_dead_space_threshold_percent(49)
        .merge_dead_space_min_bytes((48 * MB) as u64)
        .expected_keydir_max_size(10_000_000)
        .datafile_max_size(DatafileMaxSize::SoftCappedBytes(64 * MB));

    let mut entry_count: usize = 10_000_000;

    if let Some(arg) = args.get(1)
        && arg == "--count"
        && let Some(n) = args.get(2)
        && let Ok(n) = n.parse::<usize>()
    {
        options = options.expected_keydir_max_size(n);
        entry_count = n;
    }

    match BitcaskInstance::open(&PathBuf::from("db"), options) {
        Ok(mut db) => {
            for i in 0..entry_count as u128 {
                db.put(&i.to_be_bytes(), format!("{i}").as_bytes()).unwrap();
            }
            for i in 0..entry_count as u128 {
                db.put(&i.to_be_bytes(), format!("{i}").as_bytes()).unwrap();
            }
        }
        Err(e) => {
            eprintln!("{e:?}");
            process::exit(1);
        }
    }
}
