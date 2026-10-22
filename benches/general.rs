use bitcask::{
    db::KeyValueDatabase,
    engine::BitcaskInstance,
    options::{BitcaskOptions, DATAFILE_HEADER_SIZE_BYTES, DatafileMaxSize, GB, KB, SyncFrequency},
};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rand::{
    distr::{Alphanumeric, SampleString},
    seq::SliceRandom,
};
use tempfile::{TempDir, tempdir};

const DATA_TO_READS_RATIO: usize = 1;

enum ReadType {
    Sequential,
    Random,
}

fn generate_sized_string(n: usize) -> String {
    Alphanumeric.sample_string(&mut rand::rng(), n)
}

fn provision_db_in_temp(options: BitcaskOptions) -> (TempDir, BitcaskInstance) {
    let temp_dir = tempdir().unwrap();
    let path = temp_dir.path().to_path_buf();

    (temp_dir, BitcaskInstance::open(&path, options).unwrap())
}

type DatabaseKVPairs = Vec<(Box<[u8]>, Box<[u8]>)>;
fn provision_db_data(n: usize, ksz: usize, vsz: usize) -> DatabaseKVPairs {
    let mut store = Vec::new();

    for _ in 0..n {
        store.push((
            Box::from(generate_sized_string(ksz).as_bytes()),
            Box::from(generate_sized_string(vsz).as_bytes()),
        ));
    }

    store
}

fn provision_db_data_sized(n: usize, size_bytes: usize, kkv_ratio: f32) -> DatabaseKVPairs {
    let ksz = (size_bytes as f32 * kkv_ratio) as usize;
    let vsz = size_bytes - ksz;

    provision_db_data(n, ksz, vsz)
}

fn fill_database_from_vec(db: &mut BitcaskInstance, kv: &DatabaseKVPairs) {
    for (k, v) in kv {
        db.put(k, v).unwrap()
    }
}

fn read_database_sequential(db: &BitcaskInstance, kv: &DatabaseKVPairs, n: usize) {
    for (k, _) in kv.iter().take(n) {
        db.get(k).unwrap().unwrap();
    }
}

fn db_put_delegate(
    options: BitcaskOptions,
    name: &str,
    sample_size: usize,
    operation_counts: Vec<usize>,
    ksz: usize,
    vsz: usize,
    c: &mut Criterion,
) {
    let record_size = DATAFILE_HEADER_SIZE_BYTES + ksz + vsz;

    let mut group = c.benchmark_group("db_put");

    for size in operation_counts {
        group.sample_size(sample_size);
        group.throughput(Throughput::Bytes((record_size * size) as u64));
        group.bench_with_input(
            BenchmarkId::new(format!("{name}_k{ksz}B_v{vsz}B"), size),
            &size,
            |b, &s| {
                b.iter_batched(
                    || {
                        (
                            provision_db_in_temp(options.clone()),
                            provision_db_data(s, ksz, vsz),
                        )
                    },
                    |((_, mut db), kv)| fill_database_from_vec(&mut db, &kv),
                    BatchSize::SmallInput,
                );
            },
        );
    }
}

fn db_get_delegate(
    options: &BitcaskOptions,
    read_type: ReadType,
    name: &str,
    sample_size: usize,
    operation_counts: &Vec<usize>,
    ksz: usize,
    vsz: usize,
    c: &mut Criterion,
) {
    let record_size = DATAFILE_HEADER_SIZE_BYTES + ksz + vsz;

    let mut group = c.benchmark_group("db_get");

    for size in operation_counts {
        group.sample_size(sample_size);
        group.throughput(Throughput::Bytes((record_size * size) as u64));
        group.bench_with_input(
            BenchmarkId::new(format!("{name}_k{ksz}B_v{vsz}B"), size),
            &size,
            |b, &&s| {
                b.iter_batched(
                    || {
                        let (dir, mut db) = provision_db_in_temp(options.clone());
                        let mut kv = provision_db_data(s * DATA_TO_READS_RATIO, ksz, vsz);

                        fill_database_from_vec(&mut db, &kv);

                        if matches!(read_type, ReadType::Random) {
                            let mut rng = rand::rng();
                            kv.shuffle(&mut rng);
                        }
                        (dir, db, kv)
                    },
                    |(_, db, kv)| read_database_sequential(&db, &kv, s),
                    BatchSize::SmallInput,
                );
            },
        );
    }
}

fn db_put(c: &mut Criterion) {
    let operation_counts = vec![10_000usize, 50_000, 100_000, 1_000_000];
    let ksz = 4usize;
    let vsz = KB;

    let options = BitcaskOptions::default()
        .datafile_max_size(DatafileMaxSize::SoftCappedBytes(GB))
        .merge_dead_space_min_bytes(GB as u64)
        .sync_frequency(SyncFrequency::Never);

    db_put_delegate(options, "db_put", 10, operation_counts, ksz, vsz, c);
}

fn db_get(c: &mut Criterion) {
    let operation_counts = vec![10_000usize, 50_000, 100_000, 500_000];
    let ksz = 4usize;
    let vsz = KB;

    let options = BitcaskOptions::default()
        .datafile_max_size(DatafileMaxSize::SoftCappedBytes(GB))
        .merge_dead_space_min_bytes(GB as u64)
        .sync_frequency(SyncFrequency::Never);

    db_get_delegate(
        &options,
        ReadType::Sequential,
        "db_get_sequential",
        10,
        &operation_counts,
        ksz,
        vsz,
        c,
    );
    db_get_delegate(
        &options,
        ReadType::Random,
        "db_get_random",
        10,
        &operation_counts,
        ksz,
        vsz,
        c,
    );
}

criterion_group!(benches, db_put, db_get);
criterion_main!(benches);
