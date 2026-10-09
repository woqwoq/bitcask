use std::{path::PathBuf, process};

use crate::{engine::BitcaskInstance, options::BitcaskOptions};

mod datafile;
mod engine;
mod error;
mod keydir;
mod options;
mod record;

fn main() {
    match BitcaskInstance::open(PathBuf::from("db"), BitcaskOptions::default()) {
        Ok(mut db) => {
            for i in 0..2000u32 {
                db.put(&i.to_be_bytes(), format!("{i}").as_bytes()).unwrap();
            }

            for i in 0..2000u32 {
                if db.get(&i.to_be_bytes()).unwrap().unwrap()
                    != Box::from(format!("{i}").as_bytes())
                {
                    println!("Miss: {}", i);
                }
            }

            for i in 1800..2000u32 {
                db.remove(&i.to_be_bytes()).unwrap();
            }

            println!("Writer:\n{}\n Readers:\n", db.writer);
            for (_, df) in db.readers {
                println!("{df}\n");
            }
        }
        Err(e) => {
            eprintln!("{e:?}");
            process::exit(1);
        }
    }
}
