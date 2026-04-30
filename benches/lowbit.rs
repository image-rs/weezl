//! Benchmarks for the lowbit (min_code_size 0/1) support.

extern crate criterion;
extern crate weezl;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use weezl::{decode::Decoder, encode::Encoder, BitOrder, LzwStatus};

/// Isolate decoder construction + first decode for a given min_code_size.
fn bench_init(c: &mut Criterion) {
    let mut group = c.benchmark_group("lowbit-init");
    let at_8 = Encoder::new(BitOrder::Msb, 8).encode(&[0u8]).unwrap();
    let at_1 = Encoder::new(BitOrder::Msb, 1).encode(&[0u8]).unwrap();
    let at_0 = Encoder::new(BitOrder::Msb, 0).encode(&[0u8]).unwrap();
    let mut outbuf = [0u8; 16];

    group.bench_function(BenchmarkId::new("msb", 8), |b| {
        b.iter(|| {
            let mut d = Decoder::new(BitOrder::Msb, 8);
            let _ = d.decode_bytes(&at_8, &mut outbuf);
            black_box(&outbuf);
        })
    });
    group.bench_function(BenchmarkId::new("msb", 1), |b| {
        b.iter(|| {
            let mut d = Decoder::new(BitOrder::Msb, 1);
            let _ = d.decode_bytes(&at_1, &mut outbuf);
            black_box(&outbuf);
        })
    });
    group.bench_function(BenchmarkId::new("msb", 0), |b| {
        b.iter(|| {
            let mut d = Decoder::new(BitOrder::Msb, 0);
            let _ = d.decode_bytes(&at_0, &mut outbuf);
            black_box(&outbuf);
        })
    });
    group.finish();
}

/// Throughput at min_code_size = 8 on a 4 KiB synthetic stream.
fn bench_throughput_size_8(c: &mut Criterion) {
    let plaintext: Vec<u8> = (0..4096).map(|i| (i * 251 + 7) as u8).collect();
    let encoded = Encoder::new(BitOrder::Msb, 8).encode(&plaintext).unwrap();
    let mut outbuf = vec![0u8; 8192];

    let mut group = c.benchmark_group("lowbit-throughput");
    group.throughput(Throughput::Bytes(plaintext.len() as u64));
    group.bench_with_input(
        BenchmarkId::new("msb/8/ramp", plaintext.len()),
        &encoded,
        |b, enc| {
            b.iter(|| {
                let mut decoder = Decoder::new(BitOrder::Msb, 8);
                let mut data = enc.as_slice();
                let mut written = 0;
                loop {
                    let r = decoder.decode_bytes(data, outbuf.as_mut_slice());
                    data = &data[r.consumed_in..];
                    written += r.consumed_out;
                    black_box(&outbuf[..r.consumed_out]);
                    match r.status.expect("decode") {
                        LzwStatus::Done => break,
                        LzwStatus::NoProgress => panic!("stalled"),
                        _ => {}
                    }
                }
                written
            })
        },
    );
    group.finish();
}

/// Throughput at min_code_size = 1 on a 16 KiB alternating bitstream.
fn bench_throughput_size_1(c: &mut Criterion) {
    let plaintext: Vec<u8> = (0..16_384).map(|i| (i & 1) as u8).collect();
    let encoded = Encoder::new(BitOrder::Msb, 1).encode(&plaintext).unwrap();
    let mut outbuf = vec![0u8; 32_768];

    let mut group = c.benchmark_group("lowbit-throughput");
    group.throughput(Throughput::Bytes(plaintext.len() as u64));
    group.bench_with_input(
        BenchmarkId::new("msb/1/alt16k", plaintext.len()),
        &encoded,
        |b, enc| {
            b.iter(|| {
                let mut decoder = Decoder::new(BitOrder::Msb, 1);
                let mut data = enc.as_slice();
                let mut written = 0;
                let outbuf = outbuf.as_mut_slice();
                loop {
                    let r = decoder.decode_bytes(data, outbuf);
                    data = &data[r.consumed_in..];
                    written += r.consumed_out;
                    black_box(&outbuf[..r.consumed_out]);
                    match r.status.expect("decode") {
                        LzwStatus::Done => break,
                        LzwStatus::NoProgress => panic!("stalled"),
                        _ => {}
                    }
                }
                written
            })
        },
    );
    group.finish();
}

criterion_group!(
    benches,
    bench_init,
    bench_throughput_size_8,
    bench_throughput_size_1
);
criterion_main!(benches);
