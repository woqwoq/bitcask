use bitcask::{
    db::KeyValueDatabase,
    options::{BitcaskOptions, DATAFILE_HEADER_SIZE_BYTES, DatafileMaxSize, GB, KB, SyncFrequency},
    BitcaskInstance,
};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rand::{
    distr::{Alphanumeric, SampleString},
    seq::SliceRandom,
};
use tempfile::{TempDir, tempdir};

const DATA_TO_READS_RATIO: usize = 1;

const RANDOM_READ_PERCENTAGE: usize = 50; // 50% Each

#[derive(Clone)]
enum OperationType {
    Read,
    Write,
}

#[derive(Clone)]
struct Operation {
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

struct DatabaseBenchmarkRunner<'c> {
    benchmark_name: String,
    ksz: usize,
    vsz: usize,
    repeats: usize,
    inputs: Vec<usize>,
    c: &'c mut Criterion,
}

impl<'c> DatabaseBenchmarkRunner<'c> {
    pub fn new(
        benchmark_name: &str,
        ksz: usize,
        vsz: usize,
        repeats: usize,
        sample_sizes: &[usize],
        c: &'c mut Criterion,
    ) -> Self {
        Self {
            benchmark_name: benchmark_name.to_string(),
            ksz,
            vsz,
            repeats,
            inputs: sample_sizes.to_vec(),
            c,
        }
    }

    pub fn run<T, P, R>(&mut self, name: &str, throughput: T, prep: P, run: R)
    where
        T: Fn(usize) -> Throughput,
        P: Fn(usize) -> (TempDir, BitcaskInstance, DatabaseOperations),
        R: Fn(
            usize,
            TempDir,
            BitcaskInstance,
            DatabaseOperations,
        ) -> (TempDir, BitcaskInstance, DatabaseOperations),
    {
        let mut group = self.c.benchmark_group(&self.benchmark_name);

        for &size in self.inputs.iter() {
            group.sample_size(self.repeats);
            group.throughput(throughput(size));
            group.bench_with_input(
                BenchmarkId::new(format!("{name}_k{}B_v{}B", self.ksz, self.vsz), size),
                &size,
                |b, &s| {
                    b.iter_batched(
                        || prep(s),
                        |(dir, db, ops)| run(s, dir, db, ops),
                        BatchSize::SmallInput,
                    );
                },
            );
        }
    }
}

fn generate_sized_string(n: usize) -> String {
    Alphanumeric.sample_string(&mut rand::rng(), n)
}

fn provision_db_in_temp(options: BitcaskOptions) -> (TempDir, BitcaskInstance) {
    let temp_dir = tempdir().unwrap();
    let path = temp_dir.path().to_path_buf();

    (temp_dir, BitcaskInstance::open(&path, options).unwrap())
}

fn fill_db(db: &mut BitcaskInstance, ops: &DatabaseOperations) {
    for op in ops {
        db.put(&op.key, &op.value).unwrap();
    }
}

type DatabaseOperations = Vec<Operation>;
fn provision_db_data(
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

fn merge_and_shuffle(
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

fn perform_operations(db: &mut BitcaskInstance, ops: &DatabaseOperations, n: usize) {
    for op in ops.iter().take(n) {
        match op.op_type {
            OperationType::Read => {
                db.get(&op.key).unwrap().unwrap();
            }
            OperationType::Write => db.put(&op.key, &op.value).unwrap(),
        };
    }
}

fn write_bench(c: &mut Criterion) {
    let operation_counts = vec![10_000usize, 50_000, 100_000, 1_000_000];
    let ksz = 4usize;
    let vsz = KB;

    let options = BitcaskOptions::default()
        .datafile_max_size(DatafileMaxSize::SoftCappedBytes(GB))
        .merge_dead_space_min_bytes(GB as u64)
        .sync_frequency(SyncFrequency::Never);

    let rsz = DATAFILE_HEADER_SIZE_BYTES + ksz + vsz;
    let mut bench = DatabaseBenchmarkRunner::new("write", ksz, vsz, 10, &operation_counts, c);

    let throughput = |size| Throughput::Bytes((size * rsz) as u64);
    let prep = |s| {
        let (dir, db) = provision_db_in_temp(options.clone());
        let ops = provision_db_data(s, ksz, vsz, OperationType::Write);
        (dir, db, ops)
    };
    let run = |s, dir, mut db, ops| {
        perform_operations(&mut db, &ops, s);
        (dir, db, ops)
    };

    bench.run("write", throughput, prep, run);
}

fn read_bench(c: &mut Criterion) {
    let operation_counts = vec![10_000usize, 50_000, 100_000, 500_000];
    let ksz = 4usize;
    let vsz = KB;

    let options = BitcaskOptions::default()
        .datafile_max_size(DatafileMaxSize::SoftCappedBytes(GB))
        .merge_dead_space_min_bytes(GB as u64)
        .sync_frequency(SyncFrequency::Never);

    let mut bench = DatabaseBenchmarkRunner::new("read", ksz, vsz, 10, &operation_counts, c);

    let throughput = |size| Throughput::Bytes((size * vsz) as u64);
    let prep = |s| {
        let (dir, mut db) = provision_db_in_temp(options.clone());
        let ops = provision_db_data(s * DATA_TO_READS_RATIO, ksz, vsz, OperationType::Read);

        fill_db(&mut db, &ops);

        (dir, db, ops)
    };

    let prep_random = |size| {
        let (dir, db, mut ops) = prep(size);
        let mut rng = rand::rng();
        ops.shuffle(&mut rng);
        (dir, db, ops)
    };

    let run = |s, dir, mut db, ops| {
        perform_operations(&mut db, &ops, s);
        (dir, db, ops)
    };

    bench.run("sequential", throughput, prep, run);
    bench.run("random", throughput, prep_random, run);
}

// Try with small datafile size to avoid flushing the writer
fn read_write_bench(c: &mut Criterion) {
    let operation_counts = vec![10_000usize, 50_000, 100_000, 500_000];
    let ksz = 4usize;
    let vsz = KB;
    let rsz = DATAFILE_HEADER_SIZE_BYTES + ksz + vsz;

    let options = BitcaskOptions::default()
        .datafile_max_size(DatafileMaxSize::SoftCappedBytes(GB))
        .merge_dead_space_min_bytes(GB as u64)
        .sync_frequency(SyncFrequency::Never);

    let mut bench = DatabaseBenchmarkRunner::new("read_write", ksz, vsz, 10, &operation_counts, c);

    let throughput = |size| {
        let read_count = (size * RANDOM_READ_PERCENTAGE) / 100;
        let write_count = size - read_count;

        Throughput::Bytes((read_count * vsz + write_count * rsz) as u64)
    };

    let prep = |size| {
        let read_count = (size * RANDOM_READ_PERCENTAGE) / 100;
        let write_count = size - read_count;

        let (dir, mut db) = provision_db_in_temp(options.clone());
        let read_kv = provision_db_data(read_count, ksz, vsz, OperationType::Read);
        let write_kv = provision_db_data(write_count, ksz, vsz, OperationType::Write);

        fill_db(&mut db, &read_kv);

        let ops = merge_and_shuffle(read_kv, write_kv);

        (dir, db, ops)
    };

    let run = |s, dir, mut db, ops| {
        perform_operations(&mut db, &ops, s);
        (dir, db, ops)
    };

    bench.run("random", throughput, prep, run);
}

criterion_group!(benches, write_bench, read_bench, read_write_bench);
criterion_main!(benches);
