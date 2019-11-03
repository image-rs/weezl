extern crate criterion;
extern crate lzw;

use std::fs;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use lzw::{Decoder, MsbReader};

pub fn criterion_benchmark(c: &mut Criterion, file: &str) {
    let data = fs::read(file)
        .expect("Benchmark input not found");
    let mut group = c.benchmark_group("msb-8");
    let id = BenchmarkId::new(file, data.len());
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_with_input(id, &data, |b, data| b.iter(|| {
        let mut decoder = Decoder::new(MsbReader::new(), 8);
        let mut data = data.as_slice();
        while !data.is_empty() {
            match decoder.decode_bytes(data) {
                Ok((len, output)) => {
                    data = &data[len..];
                    black_box(output);
                },
                Err(err) => panic!("Error: {:?}", err),
            }
        }
    }));
}

pub fn bench_toml(c: &mut Criterion) {
    criterion_benchmark(c, "benches/Cargo-8-msb.lzw");
}

pub fn bench_binary(c: &mut Criterion) {
    criterion_benchmark(c, "benches/binary-8-msb.lzw");
}

pub fn bench_lib(c: &mut Criterion) {
    criterion_benchmark(c, "benches/lib-8-msb.lzw");
}

criterion_group!(benches, bench_toml, bench_binary, bench_lib);
criterion_main!(benches);
