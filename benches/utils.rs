use bitcask::{BitcaskInstance, test_utils::DatabaseOperations};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use tempfile::TempDir;

pub struct DatabaseBenchmarkRunner<'c> {
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
