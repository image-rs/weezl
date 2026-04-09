//! Benchmark comparing Classic vs Streaming decode strategies.
//! Run with: `cargo bench --bench strategy_compare`
//!
//! Synthetic data generators fitted to real-world LZW workloads via
//! K-means clustering (k=5) on 49 real TIFF files from 5 corpora
//! (TIFF conformance, QOI screenshots, gb82-sc screenshots, CLIC
//! photos with/without predictor), followed by Nelder-Mead parameter
//! optimization per cluster centroid.
//!
//! Each archetype's generator parameters (palette_size, run_mean,
//! nearby_prob, nearby_range) were fitted to minimize the distance
//! between the generated data's feature vector [entropy, log(run),
//! repeat_frac, mean_abs_delta] and the cluster centroid, plus the
//! log-ratio of LZW compression ratios.
//!
//! No external files needed. See wuffs-bench/examples/fit_archetypes.rs
//! on the wuffs-parity investigation branch for the fitting code.

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

// ---------------------------------------------------------------------------
// Parameterized generator — one function covers all archetypes.
//
// Parameters fitted via Nelder-Mead to each K-means cluster centroid.
// The generator produces bytes via a run-based process:
//   1. Emit `run_len` copies of the current value (geometric distribution)
//   2. Switch: with probability `nearby_prob`, jump by ±nearby_range;
//      otherwise, pick a random value from 0..palette_size.
//   3. Repeat.
//
// This simple process is enough to reproduce the entropy, run-length
// distribution, repeat fraction, and compression ratio of each cluster.
// ---------------------------------------------------------------------------

struct GenParams {
    palette_size: u16,
    run_mean: f64,
    nearby_prob: f64,
    nearby_range: u8,
}

/// xorshift32 PRNG — deterministic, no deps.
struct Rng(u32);
impl Rng {
    fn new(seed: u32) -> Self {
        Self(seed | 1)
    }
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }
}

fn generate(params: &GenParams, len: usize, seed: u32) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let pal = params.palette_size.clamp(1, 256) as u32;
    let run_mean = params.run_mean.max(1.0);
    let p = 1.0 / run_mean;

    let mut out = Vec::with_capacity(len);
    let mut val = (rng.next() % pal) as u8;

    while out.len() < len {
        // Geometric run length
        let mut run = 1usize;
        while (rng.next() as f64 / u32::MAX as f64) > p && run < len {
            run += 1;
        }
        for _ in 0..run.min(len - out.len()) {
            out.push(val);
        }
        // Switch
        if (rng.next() as f64 / u32::MAX as f64) < params.nearby_prob {
            let range = params.nearby_range.max(1) as i16;
            let delta = (rng.next() % (2 * range as u32 + 1)) as i16 - range;
            val = (val as i16 + delta).clamp(0, (pal as i16 - 1).min(255)) as u8;
        } else {
            val = (rng.next() % pal) as u8;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Fitted archetypes — parameters from Nelder-Mead optimization.
//
// Cluster │ Archetype          │ Files │ Target                        │ Fitted
// ────────┼────────────────────┼───────┼───────────────────────────────┼────────────
//  0      │ Flat UI / palette  │ 14    │ H=1.07 run=28 rep=91% r=25x  │ r=26x ±5%
//  1      │ Rich screenshot    │  8    │ H=2.58 run=3  rep=66% r=4.1x │ r=4.1x ±0%
//  2      │ Photo + predictor  │ 11    │ H=4.15 run=2  rep=36% r=1.9x │ r=1.9x ±0%
//  4      │ Photo raw / random │ 13    │ H=6.71 run=1  rep=8%  r=1.1x │ r=0.9x ±15%
//
// Cluster 3 (3 unusual files: cmyk, tiny hpredict, issue_69) merged into
// cluster 2 — too few files for a meaningful archetype.
// ---------------------------------------------------------------------------

/// Cluster 0: Flat UI / palette — terminals, settings, simple web pages.
/// 14 real files. H≈1.1, run≈28, rep≈91%, ratio≈25×.
const FLAT_UI: GenParams = GenParams {
    palette_size: 2,
    run_mean: 14.6,
    nearby_prob: 0.805,
    nearby_range: 7,
};

/// Cluster 1: Rich screenshot — complex web pages, IDEs, dark themes.
/// 8 real files. H≈2.6, run≈3, rep≈66%, ratio≈4.1×.
const RICH_SCREENSHOT: GenParams = GenParams {
    palette_size: 6,
    run_mean: 2.4,
    nearby_prob: 0.485,
    nearby_range: 24,
};

/// Cluster 2: Photo with predictor — TIFF photos after horizontal
/// differencing, complex web pages with gradients. 11 real files.
/// H≈4.2, run≈1.6, rep≈36%, ratio≈1.9×.
const PHOTO_PREDICTED: GenParams = GenParams {
    palette_size: 16,
    run_mean: 1.4,
    nearby_prob: 0.376,
    nearby_range: 19,
};

/// Cluster 4: Photo raw / near-random — uncompressed photos, high entropy.
/// LZW typically expands these. 13 real files. H≈6.7, run≈1, rep≈8%, ratio≈1.1×.
const PHOTO_RAW: GenParams = GenParams {
    palette_size: 140,
    run_mean: 1.0,
    nearby_prob: 0.490,
    nearby_range: 128,
};

// ---------------------------------------------------------------------------
// Bench harness
// ---------------------------------------------------------------------------

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
        // MSB + TIFF — image-tiff configuration (4 fitted archetypes + solid)
        make_workload(
            "flat-ui",
            &generate(&FLAT_UI, size, seed),
            BitOrder::Msb,
            true,
        ),
        make_workload(
            "rich-screenshot",
            &generate(&RICH_SCREENSHOT, size, seed),
            BitOrder::Msb,
            true,
        ),
        make_workload(
            "photo-predicted",
            &generate(&PHOTO_PREDICTED, size, seed),
            BitOrder::Msb,
            true,
        ),
        make_workload(
            "photo-raw",
            &generate(&PHOTO_RAW, size, seed),
            BitOrder::Msb,
            true,
        ),
        make_workload("solid-kwkwk", &vec![42u8; size], BitOrder::Msb, true),
        // LSB — GIF configuration (subset of archetypes)
        make_workload(
            "flat-ui",
            &generate(&FLAT_UI, size, seed),
            BitOrder::Lsb,
            false,
        ),
        make_workload(
            "rich-screenshot",
            &generate(&RICH_SCREENSHOT, size, seed),
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
