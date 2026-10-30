use std::thread;

use bitcask::{
    BitcaskInstance,
    db::KeyValueDatabase,
    options::{
        BitcaskOptions, DATAFILE_HEADER_SIZE_BYTES, DatafileMaxSize, GB, KB, MB, SyncFrequency,
    },
    test_utils::{
        DatabaseOperations, OperationType, fill_db, merge_and_shuffle, perform_operations,
        provision_db_data, provision_db_in_temp,
    },
};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rand::seq::SliceRandom;
use tempfile::TempDir;

const DATA_TO_READS_RATIO: usize = 1;

const RANDOM_READ_PERCENTAGE: usize = 50; // 50% Each

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
    let run = |s, dir, db, ops: DatabaseOperations| {
        perform_operations(&db, &ops, s);
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
        let (dir, db) = provision_db_in_temp(options.clone());
        let ops = provision_db_data(s * DATA_TO_READS_RATIO, ksz, vsz, OperationType::Read);

        fill_db(&db, &ops);

        (dir, db, ops)
    };

    let prep_random = |size| {
        let (dir, db, mut ops) = prep(size);
        let mut rng = rand::rng();
        ops.shuffle(&mut rng);
        (dir, db, ops)
    };

    let run = |s, dir, db, ops: DatabaseOperations| {
        perform_operations(&db, &ops, s);
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

        let (dir, db) = provision_db_in_temp(options.clone());
        let read_kv = provision_db_data(read_count, ksz, vsz, OperationType::Read);
        let write_kv = provision_db_data(write_count, ksz, vsz, OperationType::Write);

        fill_db(&db, &read_kv);

        let ops = merge_and_shuffle(read_kv, write_kv);

        (dir, db, ops)
    };

    let run = |s, dir, db, ops: DatabaseOperations| {
        perform_operations(&db, &ops, s);
        (dir, db, ops)
    };

    bench.run("random", throughput, prep, run);
}

fn merge_bench(c: &mut Criterion) {
    let operation_counts = vec![10_000usize, 50_000, 100_000, 500_000];
    let ksz = 4usize;
    let vsz = KB;
    let rsz = DATAFILE_HEADER_SIZE_BYTES + ksz + vsz;

    let options =
        BitcaskOptions::default().datafile_max_size(DatafileMaxSize::SoftCappedBytes(64 * MB));

    let mut bench = DatabaseBenchmarkRunner::new("merge", ksz, vsz, 10, &operation_counts, c);

    let throughput = |size| Throughput::Bytes((size * rsz) as u64);

    let prep = |size| {
        let (dir, db) = provision_db_in_temp(options.clone());
        let ops = provision_db_data(size, ksz, vsz, OperationType::Write);

        fill_db(&db, &ops);

        (dir, db, ops)
    };

    let run = |_, dir, db: BitcaskInstance, ops| {
        db.merge().unwrap();
        (dir, db, ops)
    };

    bench.run("synthetic", throughput, prep, run);
}

fn concurrent_read_bench(c: &mut Criterion) {
    let thread_count = 8usize;
    let operation_counts = vec![10_000usize, 50_000, 100_000, 500_000];
    let ksz = 4usize;
    let vsz = KB;

    // We use small datafile sizes so that threads don't need to wait for a writer lock during reads
    let options = BitcaskOptions::default()
        .datafile_max_size(DatafileMaxSize::SoftCappedBytes(5 * MB))
        .reader_lru_cache_size(128)
        .merge_dead_space_min_bytes(GB as u64)
        .sync_frequency(SyncFrequency::Never);

    let mut bench =
        DatabaseBenchmarkRunner::new("concurrent_read", ksz, vsz, 10, &operation_counts, c);

    let throughput = |size| Throughput::Bytes((size * vsz) as u64);
    let prep = |s| {
        let (dir, db) = provision_db_in_temp(options.clone());
        let ops = provision_db_data(s * DATA_TO_READS_RATIO, ksz, vsz, OperationType::Read);

        fill_db(&db, &ops);

        (dir, db, ops)
    };

    let prep_random = |size| {
        let (dir, db, mut ops) = prep(size);
        let mut rng = rand::rng();
        ops.shuffle(&mut rng);
        (dir, db, ops)
    };

    let run = |size, dir, db: BitcaskInstance, ops: DatabaseOperations| {
        thread::scope(|scope| {
            for chunk in ops.chunks(size / thread_count) {
                scope.spawn(|| {
                    perform_operations(&db, chunk, size);
                });
            }
        });

        (dir, db, ops)
    };

    bench.run("sequential", throughput, prep, run);
    bench.run("random", throughput, prep_random, run);
}

fn concurrent_write_bench(c: &mut Criterion) {
    let thread_count = 8usize;
    let operation_counts = vec![10_000usize, 50_000, 100_000, 1_000_000];
    let ksz = 4usize;
    let vsz = KB;

    let options = BitcaskOptions::default()
        .datafile_max_size(DatafileMaxSize::SoftCappedBytes(GB))
        .merge_dead_space_min_bytes(GB as u64)
        .sync_frequency(SyncFrequency::Never);

    let rsz = DATAFILE_HEADER_SIZE_BYTES + ksz + vsz;
    let mut bench =
        DatabaseBenchmarkRunner::new("concurrent_write", ksz, vsz, 10, &operation_counts, c);

    let throughput = |size| Throughput::Bytes((size * rsz) as u64);
    let prep = |s| {
        let (dir, db) = provision_db_in_temp(options.clone());
        let ops = provision_db_data(s, ksz, vsz, OperationType::Write);
        (dir, db, ops)
    };
    let run = |size, dir, db, ops: DatabaseOperations| {
        thread::scope(|scope| {
            for chunk in ops.chunks(size / thread_count) {
                scope.spawn(|| {
                    perform_operations(&db, chunk, size);
                });
            }
        });
        (dir, db, ops)
    };

    bench.run("write", throughput, prep, run);
}

criterion_group!(
    benches,
    write_bench,
    read_bench,
    read_write_bench,
    merge_bench,
    concurrent_read_bench,
    concurrent_write_bench,
);
criterion_main!(benches);
