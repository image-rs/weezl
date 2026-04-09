//! Fuzz the `min_code_size ∈ {0, 1, 2}` decode paths. The first byte of
//! the fuzz input picks the min size (mod 3) and bit order; the rest is
//! fed to `Decoder::decode`. The property under test is simply "decoder
//! does not panic or loop indefinitely" — the init-time bump loop is
//! bounded by `MAX_CODESIZE` iterations and takes no input, so no
//! fuzz-controlled data should be able to make it unbounded.
#![no_main]
use libfuzzer_sys::fuzz_target;
use weezl::{decode::Decoder, BitOrder};

fuzz_target!(|raw: &[u8]| {
    if raw.is_empty() {
        return;
    }
    let cfg = raw[0];
    let min_size = cfg % 3; // 0, 1, or 2
    let order = if (cfg >> 2) & 1 == 0 {
        BitOrder::Lsb
    } else {
        BitOrder::Msb
    };
    let payload = &raw[1..];
    let mut decoder = Decoder::new(order, min_size);
    let _ = decoder.decode(payload);
});
