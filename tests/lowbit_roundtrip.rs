#![cfg(feature = "alloc")]
//! Round-trip and edge-case tests for `min_code_size ∈ {0, 1}`.

use weezl::decode::Decoder;
use weezl::encode::Encoder;
use weezl::BitOrder;

/// Exhaustively round-trip every 1-, 2-, 4-, and 8-byte payload that can
/// be represented at `min_code_size = 0` (single-symbol alphabet `{0}`).
#[test]
fn roundtrip_min_size_0_exhaustive() {
    for &order in &[BitOrder::Lsb, BitOrder::Msb] {
        for len in 1usize..=8 {
            let data = vec![0u8; len];
            let encoded = Encoder::new(order, 0)
                .encode(&data)
                .unwrap_or_else(|e| panic!("encode {:?} len={}: {:?}", order, len, e));
            let decoded = Decoder::new(order, 0)
                .decode(&encoded)
                .unwrap_or_else(|e| panic!("decode {:?} len={}: {:?}", order, len, e));
            assert_eq!(decoded, data, "{:?} len={}", order, len);
        }
    }
}

/// Exhaustively round-trip every 2-byte payload at `min_code_size = 1`
/// (alphabet `{0, 1}`), then a set of larger randomized-ish payloads.
#[test]
fn roundtrip_min_size_1_small_and_random() {
    for &order in &[BitOrder::Lsb, BitOrder::Msb] {
        for a in 0u8..=1 {
            for b in 0u8..=1 {
                let data = vec![a, b];
                let encoded = Encoder::new(order, 1).encode(&data).unwrap();
                let decoded = Decoder::new(order, 1).decode(&encoded).unwrap();
                assert_eq!(decoded, data, "{:?} {:?}", order, data);
            }
        }

        // Longer alternating and structured payloads.
        let patterns: &[(&str, Vec<u8>)] = &[
            ("alt_4k", (0..4096).map(|i| (i & 1) as u8).collect()),
            ("zero_4k", vec![0u8; 4096]),
            ("one_4k", vec![1u8; 4096]),
            ("lfsr_16k", {
                let mut v = Vec::with_capacity(16_384);
                let mut s: u16 = 0xACE1;
                for _ in 0..16_384 {
                    let bit = ((s >> 0) ^ (s >> 2) ^ (s >> 3) ^ (s >> 5)) & 1;
                    s = (s >> 1) | (bit << 15);
                    v.push((s & 1) as u8);
                }
                v
            }),
        ];
        for (label, data) in patterns {
            let encoded = Encoder::new(order, 1).encode(data).unwrap();
            let decoded = Decoder::new(order, 1).decode(&encoded).unwrap();
            assert_eq!(
                decoded,
                *data,
                "{:?} {} ({} bytes)",
                order,
                label,
                data.len()
            );
        }
    }
}

/// Sanity-check: bytes outside the alphabet still cause `InvalidCode`
/// on the encoder side, exactly as before.
#[test]
fn min_size_0_rejects_nonzero_byte() {
    for &order in &[BitOrder::Lsb, BitOrder::Msb] {
        assert!(Encoder::new(order, 0).encode(&[1]).is_err());
        assert!(Encoder::new(order, 0).encode(&[0, 1]).is_err());
    }
}

#[test]
fn min_size_1_rejects_byte_2_and_up() {
    for &order in &[BitOrder::Lsb, BitOrder::Msb] {
        assert!(Encoder::new(order, 1).encode(&[2]).is_err());
        assert!(Encoder::new(order, 1).encode(&[0, 1, 2]).is_err());
    }
}

/// Direct bound check on the init-time bump: for every
/// `min_code_size` the decoder must finish construction in at most
/// `MAX_CODESIZE - (min_code_size + 1)` conceptual bumps. We can't poke
/// internal state, so instead we assert construction terminates and
/// produces a decoder that returns *some* result (Ok or Err) on empty
/// input, for every min size, without panicking or looping. This is
/// the "can the bump loop be exploited" guardrail.
#[test]
fn bump_loop_converges_for_every_min_size() {
    for size in 0u8..=12 {
        for &order in &[BitOrder::Lsb, BitOrder::Msb] {
            let _ = Decoder::new(order, size).decode(&[]);
        }
    }
}
