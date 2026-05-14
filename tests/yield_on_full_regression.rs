#![cfg(feature = "alloc")]
//! Regression tests for yield_on_full_buffer correctness (issue #68).
//!
//! The decoder's yield_on_full mode is used for libtiff compatibility:
//! the caller provides a fixed-size output buffer and expects the decoder
//! to return when it's full, allowing incremental processing. When the
//! decoder yields mid-stream and resumes, the output must match a
//! straight decode without yield_on_full.
//!
//! These tests encode data → decode with yield_on_full using small
//! output buffers → compare to the original input. Any divergence is
//! a correctness bug.

use weezl::{decode::Configuration, encode, BitOrder, LzwStatus};

/// Decode with yield_on_full using a fixed-size output buffer,
/// collecting all output into a Vec.
fn decode_yield(encoded: &[u8], order: BitOrder, size: u8, tiff: bool, buf_size: usize) -> Vec<u8> {
    let config = if tiff {
        Configuration::with_tiff_size_switch(order, size)
    } else {
        Configuration::new(order, size)
    };
    let mut dec = config.with_yield_on_full_buffer(true).build();

    let mut result = Vec::new();
    let mut inp = encoded;
    let mut tmp = vec![0u8; buf_size];

    loop {
        let r = dec.decode_bytes(inp, &mut tmp);
        inp = &inp[r.consumed_in..];
        result.extend_from_slice(&tmp[..r.consumed_out]);
        match r.status {
            Ok(LzwStatus::Done) => return result,
            Ok(LzwStatus::NoProgress) => {
                if r.consumed_in == 0 && r.consumed_out == 0 {
                    return result;
                }
            }
            Ok(LzwStatus::Ok) => {}
            Err(e) => panic!("decode error: {:?}", e),
        }
        if result.len() > 1 << 22 {
            panic!("output too large");
        }
    }
}

/// Decode without yield_on_full (straight decode) as reference.
fn decode_straight(encoded: &[u8], order: BitOrder, size: u8, tiff: bool) -> Vec<u8> {
    let mut dec = if tiff {
        weezl::decode::Decoder::with_tiff_size_switch(order, size)
    } else {
        weezl::decode::Decoder::new(order, size)
    };
    dec.decode(encoded).expect("straight decode failed")
}

fn encode_data(data: &[u8], order: BitOrder, size: u8, tiff: bool) -> Vec<u8> {
    let mut enc = if tiff {
        encode::Encoder::with_tiff_size_switch(order, size)
    } else {
        encode::Encoder::new(order, size)
    };
    enc.encode(data).expect("encode failed")
}

/// Assert that yield_on_full decode matches straight decode matches
/// the original input.
fn assert_yield_roundtrip(data: &[u8], order: BitOrder, size: u8, tiff: bool, buf_size: usize) {
    let encoded = encode_data(data, order, size, tiff);
    let reference = decode_straight(&encoded, order, size, tiff);
    let yielded = decode_yield(&encoded, order, size, tiff, buf_size);

    assert_eq!(
        data,
        &reference[..],
        "straight decode roundtrip mismatch (size={size} tiff={tiff} buf={buf_size})"
    );
    assert_eq!(
        reference,
        yielded,
        "yield_on_full diverges from straight decode \
         (size={size} order={order:?} tiff={tiff} buf={buf_size})\n\
         reference len={}, yielded len={}",
        reference.len(),
        yielded.len()
    );
}

// Fuzz-minimized reproducer: size=7, MSB, non-TIFF, yield_on_full=true.
// Repeating 0x4A with 0x7F runs. The decoder loses bytes 99-108 when
// yielding — they contain stale 0x7F values instead of 0x4A.
#[test]
fn regression_yield_on_full_fuzz_crash() {
    let payload: &[u8] = &[
        126, 36, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74,
        74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 126, 44, 74, 74, 74, 74, 74, 74, 74, 74, 74,
        127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127,
        127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127,
        127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74,
        74, 74, 74, 74, 74, 74, 74, 74, 74, 126, 44,
    ];
    assert_yield_roundtrip(payload, BitOrder::Msb, 7, false, 8192);
}

#[test]
fn regression_yield_on_full_small_buffers() {
    let payload: &[u8] = &[
        126, 36, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74,
        74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74, 126, 44, 74, 74, 74, 74, 74, 74, 74, 74, 74,
        127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127,
        127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 127,
        127, 127, 127, 127, 127, 127, 127, 127, 127, 127, 74, 74, 74, 74, 74, 74, 74, 74, 74, 74,
        74, 74, 74, 74, 74, 74, 74, 74, 74, 126, 44,
    ];
    for buf_size in [1, 3, 7, 8, 9, 13, 15, 16, 17, 32, 64] {
        assert_yield_roundtrip(payload, BitOrder::Msb, 7, false, buf_size);
    }
}

#[test]
fn yield_on_full_sweep_sizes() {
    // The bug is not specific to one code size. Test yield_on_full
    // roundtrip across all code sizes with data that triggers long codes.
    for size in 2..=12u8 {
        let mask = if size >= 8 {
            0xFF
        } else {
            (1u16 << size) as u8 - 1
        };
        let data: Vec<u8> = (0u16..256).map(|i| (i as u8) & mask).collect();
        for buf_size in [1, 7, 8, 9, 16, 64, 256] {
            assert_yield_roundtrip(&data, BitOrder::Msb, size, false, buf_size);
            assert_yield_roundtrip(&data, BitOrder::Lsb, size, false, buf_size);
            assert_yield_roundtrip(&data, BitOrder::Msb, size, true, buf_size);
        }
    }
}
