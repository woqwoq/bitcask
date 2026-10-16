use bitcask::{
    engine::BitcaskInstance,
    keydir::KeyDir,
    options::{BitcaskOptions, DatafileMaxSize, SyncFrequency},
    record::DATAFILE_HEADER_SIZE_BYTES,
};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rand::distr::{Alphanumeric, SampleString};
use tempfile::{TempDir, tempdir};

const KB: usize = 1024;
const MB: usize = KB * KB;
const GB: usize = KB * MB;

fn generate_sized_string(n: usize) -> String {
    Alphanumeric.sample_string(&mut rand::rng(), n)
}

fn provision_db_in_temp(options: BitcaskOptions) -> (TempDir, BitcaskInstance) {
    let temp_dir = tempdir().unwrap();
    let path = temp_dir.path().to_path_buf();

    (temp_dir, BitcaskInstance::open(path, options).unwrap())
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

fn fill_database_from_vec(db: &mut BitcaskInstance, kv: DatabaseKVPairs) {
    for (k, v) in kv {
        db.put(&k, &v).unwrap()
    }
}

pub fn prepare_random_fill_keydir(n: usize) -> KeyDir {
    let mut keydir = KeyDir::default();
    for i in 0..n {
        keydir.put(
            Box::from(i.to_be_bytes()),
            rand::random(),
            rand::random(),
            rand::random(),
            rand::random(),
        );
    }

    keydir
}

pub fn insert_n_keydir_vals(keydir: &mut KeyDir, n: usize) {
    for _ in 0..n {
        keydir.put(
            Box::from(rand::random::<u32>().to_be_bytes()),
            rand::random(),
            rand::random(),
            rand::random(),
            rand::random(),
        );
    }
}

pub fn get_n_keydir_vals(keydir: &KeyDir, n: usize) {
    for i in 0..n {
        keydir.get(&i.to_be_bytes()).unwrap();
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
                    |((_, mut db), kv)| fill_database_from_vec(&mut db, kv),
                    BatchSize::SmallInput,
                );
            },
        );
    }
}

fn keydir_insertion(c: &mut Criterion) {
    let sample_sizes = [10_000usize, 50_000, 100_000];

    let mut group = c.benchmark_group("keydir_insertion");

    for size in sample_sizes {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &s| {
            b.iter_batched(
                || prepare_random_fill_keydir(0),
                |mut kd| insert_n_keydir_vals(&mut kd, s),
                BatchSize::SmallInput,
            );
        });
    }
}

fn keydir_get(c: &mut Criterion) {
    let sample_sizes = [10_000usize, 50_000, 100_000];

    let mut group = c.benchmark_group("keydir_get");

    for size in sample_sizes {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &s| {
            b.iter_batched(
                || prepare_random_fill_keydir(s),
                |kd| get_n_keydir_vals(&kd, s),
                BatchSize::SmallInput,
            );
        });
    }
}

fn db_put_no_sync(c: &mut Criterion) {
    let operation_counts = vec![10_000usize, 50_000, 100_000];
    let ksz = 4usize;
    let vsz = 512usize;

    let options = BitcaskOptions::default()
        .datafile_max_size(DatafileMaxSize::SoftCappedBytes(GB))
        .merge_dead_space_min_bytes(GB as u64)
        .sync_frequency(SyncFrequency::Never);

    db_put_delegate(
        options,
        "db_put_no_sync",
        100,
        operation_counts,
        ksz,
        vsz,
        c,
    );
}

criterion_group!(benches, keydir_insertion, keydir_get, db_put_no_sync);
criterion_main!(benches);
