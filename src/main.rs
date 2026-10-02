use crate::engine::BitcaskInstance;

mod datafile;
mod engine;
mod keydir;

fn main() {
    let (mut db, _) = BitcaskInstance::build_mock_bitcask_instance();

    db.put("h".as_bytes(), "1".as_bytes()).unwrap();
}
