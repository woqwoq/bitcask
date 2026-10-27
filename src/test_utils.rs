use rand::{
    distr::{Alphanumeric, SampleString},
    seq::SliceRandom,
};
use tempfile::{TempDir, tempdir};

use crate::{BitcaskInstance, options::BitcaskOptions};
#[cfg(any(test, feature = "test_utils"))]
#[derive(Clone)]
pub enum OperationType {
    Read,
    Write,
}

#[derive(Clone)]
pub struct Operation {
    op_type: OperationType,
    key: Box<[u8]>,
    value: Box<[u8]>,
}

impl Operation {
    pub fn new(key: &[u8], value: &[u8], op_type: OperationType) -> Self {
        Self {
            op_type,
            key: Box::from(key),
            value: Box::from(value),
        }
    }
}

pub fn generate_sized_string(n: usize) -> String {
    Alphanumeric.sample_string(&mut rand::rng(), n)
}

pub fn provision_db_in_temp(options: BitcaskOptions) -> (TempDir, BitcaskInstance) {
    let temp_dir = tempdir().unwrap();
    let path = temp_dir.path().to_path_buf();

    (temp_dir, BitcaskInstance::open(&path, options).unwrap())
}

pub fn fill_db(db: &BitcaskInstance, ops: &DatabaseOperations) {
    for op in ops {
        db.put(&op.key, &op.value).unwrap();
    }
}

pub type DatabaseOperations = Vec<Operation>;
pub fn provision_db_data(
    n: usize,
    ksz: usize,
    vsz: usize,
    op_type: OperationType,
) -> DatabaseOperations {
    let mut store = Vec::new();

    for _ in 0..n {
        store.push(Operation::new(
            generate_sized_string(ksz).as_bytes(),
            generate_sized_string(vsz).as_bytes(),
            op_type.clone(),
        ));
    }

    store
}

pub fn merge_and_shuffle(
    read_ops: DatabaseOperations,
    write_ops: DatabaseOperations,
) -> DatabaseOperations {
    let mut rng = rand::rng();
    let mut data = DatabaseOperations::new();

    data.extend(read_ops);
    data.extend(write_ops);

    data.shuffle(&mut rng);

    data
}

pub fn perform_operations(db: &mut BitcaskInstance, ops: &DatabaseOperations, n: usize) {
    for op in ops.iter().take(n) {
        match op.op_type {
            OperationType::Read => {
                db.get(&op.key).unwrap().unwrap();
            }
            OperationType::Write => db.put(&op.key, &op.value).unwrap(),
        };
    }
}
