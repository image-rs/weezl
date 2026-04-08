//! **DO NOT MERGE — this test is deliberately expected to fail.**
//!
//! Part of the clamp-observation demo (see the non-behavioral
//! instrumentation in `src/decode.rs` and the `clamp_stats` module in
//! `src/lib.rs`). This file lives in its own `tests/` binary so its
//! atomic counter state is process-isolated from the sibling
//! `clamp_observation_good_data.rs` suite.
//!
//! **Purpose:** show that the hypothetical pre-#61
//! `min(len, entry.prev)` clamp was not *completely* dead — a carefully
//! corrupted stream, which the decoder still happily runs through
//! `reconstruct()`, causes `raw_prev > initial_code` to become true on
//! some chain-walk step. The instrumentation counts those observations.
//!
//! **Expected outcome:** fail on the corrupt-input assertion at the
//! bottom of the single test function. The valid-data phase that runs
//! first (collecting the min invariant) should still pass its local
//! assertion — if *that* breaks, something else is wrong.
//!
//! Test structure runs in two phases, in order:
//!
//!   1. Collect the min invariant: round-trip a set of well-formed LZW
//!      streams and assert the clamp counter stays at zero. If this
//!      phase fires the clamp, the demo is invalidated — the subsequent
//!      corrupt-input observation would no longer specifically
//!      demonstrate corrupt-input behavior.
//!
//!   2. Feed a 48-byte ramp payload that encodes to 158 LZW bytes,
//!      flip the single bit `byte[3] bit 3`, and decode. The decoder
//!      reaches `reconstruct()` and the instrumentation records a
//!      non-zero clamp count, causing the final assertion to fail.

use weezl::clamp_stats;
use weezl::decode::Decoder;
use weezl::encode::Encoder;
use weezl::BitOrder;

#[test]
fn clamp_fires_on_corrupt_input_after_min_invariant_collected() {
    clamp_stats::reset();

    // --- Phase 1: collect the min invariant on well-formed data -----
    let ramp48: Vec<u8> = (0..48u32).map(|i| (i * 131 + 7) as u8).collect();
    let valid_inputs: &[(&str, &[u8])] = &[
        ("hello_world", b"Hello, world"),
        ("zero_4k", &[0u8; 4096]),
        ("alphabet", b"abcdefghijklmnopqrstuvwxyz"),
        ("ramp48", &ramp48),
    ];
    for &order in &[BitOrder::Lsb, BitOrder::Msb] {
        for &min_code_size in &[8u8, 9, 12] {
            for (label, data) in valid_inputs {
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
        "min invariant phase: clamp fired {} times on well-formed input — \
         the subsequent corrupt-input demonstration is invalid",
        clamp_stats::clamp_count()
    );

    // --- Phase 2: corrupt the encoding of `ramp48` and observe ------
    //
    // Re-encode the 48-byte ramp at Lsb / min_code_size = 12, flip the
    // single bit byte[3] bit 3, decode. This is the minimal single-bit
    // mutation found empirically (see `examples/minimize.rs`) that
    // drives the chain walk into a state where `raw_prev > initial_code`
    // on a visited entry.
    clamp_stats::reset();
    let mut corrupt = Encoder::new(BitOrder::Lsb, 12)
        .encode(&ramp48)
        .expect("encode ramp48");
    corrupt[3] ^= 0x08;
    let _ = Decoder::new(BitOrder::Lsb, 12).decode(&corrupt);

    let fired = clamp_stats::clamp_count();
    assert_eq!(
        fired, 0,
        "corrupt input fired the pre-#61 clamp {} time(s) — this is the \
         *intended* failure of this demo test, showing that the old \
         `min(len, entry.prev)` clamp would have altered its argument on \
         exactly this kind of input. #61 replaced that clamp with an \
         unconditional `& MASK` which produces different (still non-panicking) \
         output bytes in this case.",
        fired
    );
}
