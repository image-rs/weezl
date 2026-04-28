//! Regression test for an out-of-bounds panic found via single-bit-flip
//! mutation fuzzing of valid LZW streams at `min_code_size = 12`.
//!
//! Before the fixed-array decode table landed
//! (https://github.com/image-rs/weezl/pull/61), a single bit flip in the
//! encoded output of `b"Hello, world"` could steer `Table::derive_burst`
//! into indexing its `depths` vector one past the end, panicking with:
//!
//!     index out of bounds: the len is 4098 but the index is 4098
//!
//! The `& MASK` indexing pattern introduced by #61 replaces the OOB with
//! a wrapping read, so corrupt input now produces wrong output or a clean
//! `LzwError::InvalidCode`, but must never panic.
//!
//! Test order is deliberate:
//!
//!   1. First collect the "min invariant": round-trip a set of valid
//!      inputs and assert byte-identical output. This is the observable
//!      consequence of the reconstruct chain walk's `entry.prev < len`
//!      invariant — if any case mismatches, the decoder is broken on
//!      valid data and the subsequent corrupt-input check would be
//!      meaningless.
//!
//!   2. Only then feed the bit-flipped stream to the decoder. Both `Ok`
//!      and `Err` are acceptable; simply returning from `decode()`
//!      (rather than unwinding) is the property under test.

use weezl::decode::Decoder;
use weezl::encode::Encoder;
use weezl::BitOrder;

#[test]
fn corrupt_input_does_not_panic_in_derive_burst() {
    // --- Phase 1: min invariant — valid round-trips ---------------------
    //
    // A compact but deliberately diverse set: the exact literal whose
    // encoding the fuzz flip targets, a zero run, an all-0xff run, a
    // repeating alphabet, and a small byte ramp. Both bit orders, and a
    // spread of code sizes including 12 (where the fuzz finding lived).
    let inputs: &[(&str, &[u8])] = &[
        ("hello_world", b"Hello, world"),
        ("zero_run_4k", &[0u8; 4096]),
        ("ff_run_4k", &[0xffu8; 4096]),
        ("alphabet", b"abcdefghijklmnopqrstuvwxyz"),
        ("ramp_256", &{
            let mut r = [0u8; 256];
            for (i, b) in r.iter_mut().enumerate() {
                *b = i as u8;
            }
            r
        }),
    ];

    for &order in &[BitOrder::Lsb, BitOrder::Msb] {
        for &min_code_size in &[8u8, 9, 12] {
            for (label, data) in inputs {
                let encoded = Encoder::new(order, min_code_size)
                    .encode(data)
                    .unwrap_or_else(|e| {
                        panic!("encode {} {:?}/{}: {:?}", label, order, min_code_size, e)
                    });
                let decoded = Decoder::new(order, min_code_size)
                    .decode(&encoded)
                    .unwrap_or_else(|e| {
                        panic!("decode {} {:?}/{}: {:?}", label, order, min_code_size, e)
                    });
                assert_eq!(
                    decoded, *data,
                    "round-trip mismatch for {} at {:?}/{}",
                    label, order, min_code_size
                );
            }
        }
    }

    // --- Phase 2: corrupt input must not panic --------------------------
    //
    // Re-encode the literal the fuzz harness started from, then flip a
    // single bit. The flip is byte index 3, bit 3 — the minimal mutation
    // that historically panicked `derive_burst`. Both `Ok` and `Err` are
    // acceptable outcomes; reaching this line without unwinding is the
    // property under test.
    let encoded = Encoder::new(BitOrder::Lsb, 12)
        .encode(b"Hello, world")
        .expect("encode baseline");
    assert!(
        encoded.len() > 3,
        "encoded baseline too short to mutate at byte 3"
    );
    let mut corrupt = encoded.clone();
    corrupt[3] ^= 0x08;

    let _ = Decoder::new(BitOrder::Lsb, 12).decode(&corrupt);
}
