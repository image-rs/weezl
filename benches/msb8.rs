extern crate criterion;
extern crate lzw;

use std::fs;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use lzw::relzw::{ByteOrder,Decoder,LzwStatus};

pub fn criterion_benchmark(c: &mut Criterion, file: &str) {
    let data = fs::read(file)
        .expect("Benchmark input not found");
    let mut group = c.benchmark_group("msb-8");
    let id = BenchmarkId::new(file, data.len());
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_with_input(id, &data, |b, data| b.iter(|| {
        let mut decoder = Decoder::new(ByteOrder::Msb, 8);
        let mut outbuf = vec![0; 1 << 12];
        let mut data = data.as_slice();
        loop {
            let result = decoder.decode_bytes(data, &mut outbuf[..]);
            let done = result.status.expect("Error");
            data = &data[result.consumed_in..];
            black_box(&outbuf[..result.consumed_out]);
            if let LzwStatus::Done = done {
                break;
            }
            if let LzwStatus::NoProgress = done {
                panic!("Need to make progress");
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
