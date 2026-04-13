//! Benchmark comparing Classic vs Streaming decode strategies.
//!
//! Run with: `cargo bench --bench strategy_compare`
//!
//! Workloads span the full LZW decode space: scanned documents (email,
//! blank, dense, monochrome-old), continuous-tone photos (grayscale,
//! color), solid-color KwKwK stress, and palette-indexed flat UI.
//! Generators and archetype constants live in `benches/generators.rs`.

#[path = "generators.rs"]
mod generators;

use std::sync::Arc;
use weezl::{
    decode::{Configuration, TableStrategy},
    encode::Encoder,
    BitOrder, LzwStatus,
};
use zenbench::prelude::*;

fn decode_all(
    encoded: &[u8],
    out: &mut [u8],
    order: BitOrder,
    tiff: bool,
    strategy: TableStrategy,
) -> usize {
    let config = if tiff {
        Configuration::with_tiff_size_switch(order, 8)
    } else {
        Configuration::new(order, 8)
    };
    let mut dec = config.with_table_strategy(strategy).build();
    let mut inp = encoded;
    let mut cursor = out;
    let mut written = 0;
    loop {
        let r = dec.decode_bytes(inp, cursor);
        inp = &inp[r.consumed_in..];
        written += r.consumed_out;
        cursor = &mut std::mem::take(&mut cursor)[r.consumed_out..];
        match r.status {
            Ok(LzwStatus::Done | LzwStatus::NoProgress) => return written,
            Ok(LzwStatus::Ok) => {
                if inp.is_empty() && cursor.is_empty() {
                    return written;
                }
            }
            Err(_) => return written,
        }
    }
}

struct Workload {
    name: &'static str,
    encoded: Arc<Vec<u8>>,
    decoded_size: usize,
    order: BitOrder,
    tiff: bool,
}

fn make_workload(name: &'static str, data: &[u8], order: BitOrder, tiff: bool) -> Workload {
    let encoded = if tiff {
        Encoder::with_tiff_size_switch(order, 8)
            .encode(data)
            .unwrap()
    } else {
        Encoder::new(order, 8).encode(data).unwrap()
    };
    let mut scratch = vec![0u8; data.len() + 4096];
    let decoded_size = decode_all(&encoded, &mut scratch, order, tiff, TableStrategy::Classic);
    Workload {
        name,
        encoded: Arc::new(encoded),
        decoded_size,
        order,
        tiff,
    }
}

fn bench_workload(g: &mut BenchGroup, w: &Workload) {
    g.throughput(Throughput::Bytes(w.decoded_size as u64));
    g.config().min_sample_ns(10_000_000);
    g.config().max_rounds(500);
    g.config().min_rounds(100);
    let out_cap = w.decoded_size + 4096;

    for &(label, strategy) in &[
        ("classic", TableStrategy::Classic),
        ("streaming", TableStrategy::Streaming),
    ] {
        let enc = Arc::clone(&w.encoded);
        let order = w.order;
        let tiff = w.tiff;
        g.bench(label, move |b| {
            let enc = Arc::clone(&enc);
            let mut out = vec![0u8; out_cap];
            b.iter(move || {
                let n = decode_all(&enc, &mut out, order, tiff, strategy);
                black_box(&out[..n]);
                n
            });
        });
    }
}

fn bench_strategies(suite: &mut Suite) {
    let size = 256 * 1024;
    let seed = 0xDEADBEEF;

    let workloads = vec![
        // MSB + TIFF — scanned document archetypes
        make_workload(
            "scanned-email",
            &generators::generate(&generators::SCANNED_EMAIL, size, seed),
            BitOrder::Msb,
            true,
        ),
        make_workload(
            "scanned-blank",
            &generators::generate(&generators::SCANNED_BLANK, size, seed),
            BitOrder::Msb,
            true,
        ),
        make_workload(
            "scanned-dense",
            &generators::generate(&generators::SCANNED_DENSE, size, seed),
            BitOrder::Msb,
            true,
        ),
        make_workload(
            "scanned-mono-old",
            &generators::generate(&generators::SCANNED_MONOCHROME_OLD, size, seed),
            BitOrder::Msb,
            true,
        ),
        make_workload("solid-kwkwk", &vec![42u8; size], BitOrder::Msb, true),
        // Photo workloads — continuous-tone imagery
        make_workload(
            "photo-gray",
            &generators::generate_photo(&generators::PHOTO_GRAY, size, seed),
            BitOrder::Msb,
            true,
        ),
        make_workload(
            "photo-color",
            &generators::generate_photo(&generators::PHOTO_COLOR, size, seed),
            BitOrder::Msb,
            true,
        ),
        // LSB — GIF configuration
        make_workload(
            "scanned-email",
            &generators::generate(&generators::SCANNED_EMAIL, size, seed),
            BitOrder::Lsb,
            false,
        ),
        make_workload(
            "flat-ui",
            &generators::generate_flat_ui(size, seed),
            BitOrder::Lsb,
            false,
        ),
    ];

    for w in &workloads {
        let mode = if w.tiff { "tiff" } else { "gif" };
        suite.group(format!("{}/{}", mode, w.name), |g| bench_workload(g, w));
    }
}

zenbench::main!(bench_strategies);
