mod utils;

use std::thread;
use utils::DatabaseBenchmarkRunner;

use bitcask::{
    options::{BitcaskOptions, DATAFILE_HEADER_SIZE_BYTES, DatafileMaxSize, GB, KB, SyncFrequency},
    test_utils::*,
};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};

fn integration_write(c: &mut Criterion) {
    let thread_count = 8usize;
    let operation_counts = vec![1_000_000];
    let ksz = 4usize;
    let vsz = 20 * KB;

    let options = BitcaskOptions::default()
        .datafile_max_size(DatafileMaxSize::SoftCappedBytes(GB))
        .merge_dead_space_min_bytes(GB as u64)
        .sync_frequency(SyncFrequency::Never);

    let rsz = DATAFILE_HEADER_SIZE_BYTES + ksz + vsz;
    let mut bench =
        DatabaseBenchmarkRunner::new("integration_write", ksz, vsz, 10, &operation_counts, c);

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

criterion_group!(benches, integration_write);
criterion_main!(benches);
