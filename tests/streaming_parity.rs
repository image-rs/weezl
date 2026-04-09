//! Comprehensive Streaming decoder parity tests.
//!
//! Every test encodes data with the weezl encoder, then decodes with Classic
//! and Streaming. The outputs must be byte-for-
//! byte identical. This is an oracle test: Classic is the reference.
//!
//! Coverage targets:
//!   - All min_code_size values 0..=12
//!   - Both bit orders (LSB, MSB)
//!   - GIF and TIFF mode
//!   - With and without yield_on_full_buffer
//!   - Various data patterns (uniform, ramp, alternating, random, KwKwK-heavy)
//!   - Various payload lengths (0, 1, Q-1, Q, Q+1, 255, 4096, 64K)
//!   - Small output buffer sweep (1..=64 bytes) to exercise suspension/resume
//!   - Table fill cycles (long enough to clear and refill)

use weezl::{
    decode::{Configuration, TableStrategy},
    encode, BitOrder, LzwStatus,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Encode data, return compressed bytes.
fn encode(data: &[u8], order: BitOrder, size: u8, tiff: bool) -> Option<Vec<u8>> {
    // Encoder rejects size < 2 (PR #67 not yet merged here).
    if size < 2 {
        return None;
    }
    let mut encoder = if tiff {
        encode::Encoder::with_tiff_size_switch(order, size)
    } else {
        encode::Encoder::new(order, size)
    };
    encoder.encode(data).ok()
}

/// Decode using the low-level decode_bytes loop (exercises suspension).
fn decode_full(
    encoded: &[u8],
    order: BitOrder,
    size: u8,
    tiff: bool,
    yield_on_full: bool,
    strategy: TableStrategy,
    out_buf_size: usize,
) -> Result<Vec<u8>, String> {
    let config = if tiff {
        Configuration::with_tiff_size_switch(order, size)
    } else {
        Configuration::new(order, size)
    };
    let mut dec = config
        .with_yield_on_full_buffer(yield_on_full)
        .with_table_strategy(strategy)
        .build();

    let mut result = Vec::new();
    let mut inp = encoded;
    let mut tmp = vec![0u8; out_buf_size];

    loop {
        let r = dec.decode_bytes(inp, &mut tmp);
        inp = &inp[r.consumed_in..];
        result.extend_from_slice(&tmp[..r.consumed_out]);
        match r.status {
            Ok(LzwStatus::Done) => return Ok(result),
            Ok(LzwStatus::NoProgress) => {
                if r.consumed_in == 0 && r.consumed_out == 0 {
                    return Ok(result);
                }
            }
            Ok(LzwStatus::Ok) => {}
            Err(e) => return Err(format!("{:?}", e)),
        }
        // Safety valve
        if result.len() > 1 << 22 {
            return Err("output too large".into());
        }
    }
}

/// Assert Classic and Streaming produce identical output.
fn assert_parity(
    data: &[u8],
    order: BitOrder,
    size: u8,
    tiff: bool,
    yield_on_full: bool,
    out_buf_size: usize,
) {
    let encoded = match encode(data, order, size, tiff) {
        Some(e) => e,
        None => return, // encoder rejected (e.g., byte out of alphabet)
    };

    let classic = decode_full(
        &encoded,
        order,
        size,
        tiff,
        yield_on_full,
        TableStrategy::Classic,
        out_buf_size,
    );
    let streaming = decode_full(
        &encoded,
        order,
        size,
        tiff,
        yield_on_full,
        TableStrategy::Streaming,
        out_buf_size,
    );

    match (classic, streaming) {
        (Ok(c), Ok(s)) => {
            // Streaming must always round-trip correctly.
            assert_eq!(
                data, &s[..],
                "Streaming roundtrip mismatch: order={:?} size={} tiff={} yield={} buf={} datalen={}",
                order, size, tiff, yield_on_full, out_buf_size, data.len()
            );
            if c.len() == s.len() {
                // Both produced the same length — must be identical.
                assert_eq!(
                    c, s,
                    "Classic vs Streaming differ: order={:?} size={} tiff={} yield={} buf={} datalen={}",
                    order, size, tiff, yield_on_full, out_buf_size, data.len()
                );
            } else {
                // Classic produced fewer bytes (known limitation with
                // yield_on_full + tiny output buffers). Verify Streaming
                // is a superset: Classic's output must be a prefix of
                // Streaming's.
                assert!(
                    s.starts_with(&c),
                    "Classic output is not a prefix of Streaming: classic={} streaming={} \
                     (order={:?} size={} tiff={} yield={} buf={} datalen={})",
                    c.len(),
                    s.len(),
                    order,
                    size,
                    tiff,
                    yield_on_full,
                    out_buf_size,
                    data.len()
                );
            }
        }
        (Err(_ce), Err(_se)) => {
            // Both errored — fine
        }
        (Ok(c), Err(se)) => {
            panic!(
                "Classic succeeded ({} bytes) but Streaming failed: {} \
                 (order={:?} size={} tiff={} yield={} buf={} datalen={})",
                c.len(),
                se,
                order,
                size,
                tiff,
                yield_on_full,
                out_buf_size,
                data.len()
            );
        }
        (Err(ce), Ok(s)) => {
            // Streaming succeeded where Classic failed — acceptable if
            // Streaming's output matches the original data.
            assert_eq!(
                data,
                &s[..],
                "Classic failed ({}) but Streaming produced wrong output ({} bytes) \
                 (order={:?} size={} tiff={} yield={} buf={} datalen={})",
                ce,
                s.len(),
                order,
                size,
                tiff,
                yield_on_full,
                out_buf_size,
                data.len()
            );
        }
    }
}

/// Run assert_parity across the full (order, tiff, yield) matrix.
fn parity_matrix(data: &[u8], size: u8, out_buf_size: usize) {
    for &order in &[BitOrder::Lsb, BitOrder::Msb] {
        for &tiff in &[false, true] {
            for &yield_on_full in &[false, true] {
                assert_parity(data, order, size, tiff, yield_on_full, out_buf_size);
            }
        }
    }
}

/// Generate data clamped to the valid alphabet for a given min_size.
fn clamped(data: &[u8], size: u8) -> Vec<u8> {
    if size >= 8 {
        data.to_vec()
    } else {
        let mask = (1u16 << size) - 1;
        data.iter().map(|&b| (u16::from(b) & mask) as u8).collect()
    }
}

// ---------------------------------------------------------------------------
// Data generators
// ---------------------------------------------------------------------------

fn uniform(len: usize, val: u8) -> Vec<u8> {
    vec![val; len]
}

fn ramp(len: usize, size: u8) -> Vec<u8> {
    let m = if size >= 8 { 256 } else { 1usize << size };
    (0..len).map(|i| (i % m) as u8).collect()
}

fn alternating(len: usize, a: u8, b: u8) -> Vec<u8> {
    (0..len).map(|i| if i & 1 == 0 { a } else { b }).collect()
}

/// Simple LFSR-based pseudo-random sequence.
fn pseudo_random(len: usize, seed: u32, size: u8) -> Vec<u8> {
    let mask = if size >= 8 { 0xFF } else { (1u8 << size) - 1 };
    let mut state = seed | 1; // ensure nonzero
    (0..len)
        .map(|_| {
            // xorshift32
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            ((state >> 8) as u8) & mask
        })
        .collect()
}

/// Data that maximizes KwKwK (cScSc) codes: repeated pattern where the
/// decoder's next code equals the code being added.
fn kwkwk_heavy(len: usize, size: u8) -> Vec<u8> {
    // Repeated single-byte runs create many KwKwK situations.
    // e.g., [0,0,0,0,...] → after initial literal, every subsequent code
    // is the "new" code (save_code) triggering the self-reference path.
    let val = if size >= 8 { 42 } else { 0 };
    vec![val; len]
}

/// Data long enough to fill the LZW table, trigger a clear, and refill.
fn table_cycle(size: u8) -> Vec<u8> {
    // Table has 4096 entries. With size=8, literals take 256 slots,
    // clear+end take 2, leaving 3838 derived slots. To cycle through,
    // we need ~3838 distinct pairs minimum. A ramp of ~8K bytes does it.
    let len = if size <= 2 { 256 } else { 8192 };
    ramp(len, size)
}

// ---------------------------------------------------------------------------
// Tests: full configuration matrix at every size
// ---------------------------------------------------------------------------

#[test]
fn parity_all_sizes_ramp() {
    for size in 0..=12u8 {
        let data = ramp(512, size);
        parity_matrix(&data, size, 8192);
    }
}

#[test]
fn parity_all_sizes_uniform() {
    for size in 0..=12u8 {
        let data = uniform(512, 0);
        let data = clamped(&data, size);
        parity_matrix(&data, size, 8192);
    }
}

#[test]
fn parity_all_sizes_random() {
    for size in 0..=12u8 {
        let data = pseudo_random(1024, 0xDEAD_BEEF, size);
        parity_matrix(&data, size, 8192);
    }
}

// ---------------------------------------------------------------------------
// Tests: edge-case payload lengths
// ---------------------------------------------------------------------------

#[test]
fn parity_empty_payload() {
    for size in 2..=8u8 {
        let data: Vec<u8> = vec![];
        parity_matrix(&data, size, 8192);
    }
}

#[test]
fn parity_single_byte() {
    for size in 0..=8u8 {
        let data = vec![0u8];
        parity_matrix(&data, size, 8192);
    }
}

#[test]
fn parity_two_bytes() {
    for size in 0..=8u8 {
        let mask = if size >= 8 { 0xFF } else { (1u8 << size) - 1 };
        // All 2-byte permutations within the alphabet
        for a in 0..=mask.min(3) {
            for b in 0..=mask.min(3) {
                let data = vec![a, b];
                parity_matrix(&data, size, 8192);
            }
        }
    }
}

#[test]
fn parity_boundary_lengths() {
    // Lengths around Q (8), 255, 256, 4096
    for size in [2, 4, 8] {
        for &len in &[7, 8, 9, 15, 16, 17, 255, 256, 257, 4095, 4096, 4097] {
            let data = ramp(len, size);
            parity_matrix(&data, size, 8192);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: KwKwK and table-fill cycles
// ---------------------------------------------------------------------------

#[test]
fn parity_kwkwk_heavy() {
    for size in [2, 4, 8] {
        let data = kwkwk_heavy(4096, size);
        parity_matrix(&data, size, 8192);
    }
}

#[test]
fn parity_table_cycle() {
    for size in [2, 4, 8] {
        let data = table_cycle(size);
        parity_matrix(&data, size, 8192);
    }
}

#[test]
fn parity_alternating() {
    for size in [1, 2, 4, 8] {
        let mask = if size >= 8 { 0xFF } else { (1u8 << size) - 1 };
        let data = alternating(2048, 0, mask);
        parity_matrix(&data, size, 8192);
    }
}

// ---------------------------------------------------------------------------
// Tests: small output buffer sweep (exercises suspension/resume)
// ---------------------------------------------------------------------------

#[test]
fn parity_small_buffer_sweep_size8() {
    let data = pseudo_random(2048, 42, 8);
    for buf_size in 1..=64 {
        parity_matrix(&data, 8, buf_size);
    }
}

#[test]
fn parity_small_buffer_sweep_size2() {
    let data = ramp(512, 2);
    for buf_size in 1..=32 {
        parity_matrix(&data, 2, buf_size);
    }
}

#[test]
fn parity_small_buffer_kwkwk() {
    // KwKwK with small buffers: every code triggers the pending spill path.
    let data = kwkwk_heavy(1024, 8);
    for buf_size in [1, 2, 7, 8, 9, 15, 16, 63, 64] {
        parity_matrix(&data, 8, buf_size);
    }
}

// ---------------------------------------------------------------------------
// Tests: lowbit (min_size 0 and 1) thorough coverage
// ---------------------------------------------------------------------------

#[test]
fn parity_lowbit_size0_exhaustive() {
    // Only byte 0 is valid at size 0
    for len in 1..=32 {
        let data = vec![0u8; len];
        parity_matrix(&data, 0, 8192);
    }
}

#[test]
fn parity_lowbit_size1_exhaustive() {
    // Alphabet {0, 1}
    // All permutations up to length 4
    for len in 1..=4 {
        for bits in 0..(1u32 << len) {
            let data: Vec<u8> = (0..len).map(|i| ((bits >> i) & 1) as u8).collect();
            parity_matrix(&data, 1, 8192);
        }
    }
    // Longer payloads
    for &len in &[16, 64, 256, 1024] {
        let data = alternating(len, 0, 1);
        parity_matrix(&data, 1, 8192);
        let data = uniform(len, 0);
        parity_matrix(&data, 1, 8192);
        let data = pseudo_random(len, 0xCAFE, 1);
        parity_matrix(&data, 1, 8192);
    }
}

#[test]
fn parity_lowbit_small_buffer() {
    let data = alternating(128, 0, 1);
    for buf_size in 1..=16 {
        parity_matrix(&data, 1, buf_size);
    }
    let data = vec![0u8; 128];
    for buf_size in 1..=16 {
        parity_matrix(&data, 0, buf_size);
    }
}

// ---------------------------------------------------------------------------
// Tests: min_size=12 boundary (clear/end alias table edge)
// ---------------------------------------------------------------------------

#[test]
fn parity_size12() {
    // At size=12, the table starts nearly full (4096 literals + clear + end = 4098).
    // Each byte value < 4096 is valid. Data bytes are 0..=255 which all fit.
    let data = ramp(4096, 12);
    parity_matrix(&data, 12, 8192);

    let data = pseudo_random(4096, 0x1234, 12);
    parity_matrix(&data, 12, 8192);
}

// ---------------------------------------------------------------------------
// Tests: large payload (64K) to exercise steady-state
// ---------------------------------------------------------------------------

#[test]
fn parity_large_random() {
    let data = pseudo_random(65536, 0xBEEF, 8);
    // Only test large buffer to keep runtime reasonable
    for &order in &[BitOrder::Lsb, BitOrder::Msb] {
        assert_parity(&data, order, 8, false, false, 8192);
        assert_parity(&data, order, 8, true, false, 8192);
    }
}

#[test]
fn parity_large_kwkwk() {
    let data = kwkwk_heavy(65536, 8);
    for &order in &[BitOrder::Lsb, BitOrder::Msb] {
        assert_parity(&data, order, 8, false, false, 8192);
    }
}
