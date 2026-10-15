use bitcask::keydir::KeyDir;
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};

pub fn prepare_random_fill_keydir(n: usize) -> KeyDir {
    let mut keydir = KeyDir::new();
    for _ in 0..n {
        keydir.put(
            Box::from(rand::random::<u32>().to_be_bytes()),
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

fn keydir_insertion_insertion(c: &mut Criterion) {
    let sample_sizes = [10000, 30000, 50000, 100000];

    let mut group = c.benchmark_group("keydir_insertion_insertion");

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

criterion_group!(benches, keydir_insertion_insertion);
criterion_main!(benches);
