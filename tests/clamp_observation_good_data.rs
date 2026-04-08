//! **DO NOT MERGE — demonstration only.**
//!
//! Part of the clamp-observation demo (see the non-behavioral
//! instrumentation in `src/decode.rs` and the `clamp_stats` module in
//! `src/lib.rs`). This test file lives in its own `tests/` binary so its
//! atomic counter state is process-isolated from the sibling
//! `clamp_observation_corrupt_input.rs` suite, which is intentionally
//! expected to fail.
//!
//! **Purpose:** verify that the hypothetical pre-#61
//! `min(len, entry.prev)` clamp is truly dead code on well-formed LZW
//! input. The instrumentation in `reconstruct` increments
//! `clamp_stats::clamp_count()` every time it observes
//! `raw_prev > initial_code` on a visited entry — the exact condition
//! the old clamp would have corrected. On every valid stream this test
//! round-trips, that counter must stay at zero.
//!
//! **Expected outcome:** pass.

use weezl::clamp_stats;
use weezl::decode::Decoder;
use weezl::encode::Encoder;
use weezl::BitOrder;

#[test]
fn clamp_never_fires_on_valid_roundtrip_corpus() {
    clamp_stats::reset();

    // A deliberately diverse set: short literal, long zero run, all-0xff
    // run, alphabet, 48-byte multiplicative ramp (the exact payload whose
    // bit-flipped encoding we use in the sibling corrupt-input test), and
    // a ~64 KB structured stream to stress dictionary saturation and any
    // implicit clear cycles.
    let mut long_structured = Vec::with_capacity(65_536);
    for i in 0..65_536u32 {
        long_structured.push(((i.wrapping_mul(2654435761)) >> 24) as u8);
    }
    let ramp48: Vec<u8> = (0..48u32).map(|i| (i * 131 + 7) as u8).collect();
    let ramp256: Vec<u8> = (0..=255u8).collect();
    let inputs: &[(&str, &[u8])] = &[
        ("hello_world", b"Hello, world"),
        ("zero_4k", &[0u8; 4096]),
        ("ff_4k", &[0xffu8; 4096]),
        ("alphabet", b"abcdefghijklmnopqrstuvwxyz"),
        ("ramp48", &ramp48),
        ("ramp256", &ramp256),
        ("long_structured", &long_structured),
    ];

    for &order in &[BitOrder::Lsb, BitOrder::Msb] {
        for &min_code_size in &[8u8, 9, 10, 11, 12] {
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

    assert_eq!(
        clamp_stats::clamp_count(),
        0,
        "hypothetical pre-#61 min(len, entry.prev) clamp fired on well-formed LZW input — \
         {} times across the roundtrip corpus. #61's claim that the clamp is dead on valid \
         data would then be wrong.",
        clamp_stats::clamp_count()
    );
}
