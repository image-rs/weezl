#![no_main]
use libfuzzer_sys::fuzz_target;
use weezl::{decode, encode, BitOrder};

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    // First byte selects parameters
    let control = data[0];
    let payload = &data[1..];

    let order = if control & 1 == 0 {
        BitOrder::Msb
    } else {
        BitOrder::Lsb
    };
    let tiff = control & 2 != 0;
    let yield_on_full = control & 4 != 0;
    let size = ((control >> 3) % 11) + 2; // 2..=12 (encoder rejects < 2)

    // Clamp payload to valid alphabet
    let clamped: Vec<u8> = if size >= 8 {
        payload.to_vec()
    } else {
        let mask = (1u16 << size) - 1;
        payload.iter().map(|&b| (u16::from(b) & mask) as u8).collect()
    };

    // Encode
    let mut encoder = if tiff {
        encode::Encoder::with_tiff_size_switch(order, size)
    } else {
        encode::Encoder::new(order, size)
    };
    let encoded = match encoder.encode(&clamped) {
        Ok(e) => e,
        Err(_) => return,
    };

    // Build decoder configs
    let make_config = |strategy| {
        let c = if tiff {
            decode::Configuration::with_tiff_size_switch(order, size)
        } else {
            decode::Configuration::new(order, size)
        };
        c.with_yield_on_full_buffer(yield_on_full)
            .with_table_strategy(strategy)
    };

    // Decode with both strategies
    let classic = make_config(decode::TableStrategy::Classic)
        .build()
        .decode(&encoded);
    let streaming = make_config(decode::TableStrategy::Streaming)
        .build()
        .decode(&encoded);

    let classic = classic.expect("classic decode failed");
    let streaming = streaming.expect("streaming decode failed");

    assert_eq!(
        clamped, classic,
        "classic roundtrip (size={size} tiff={tiff} yield={yield_on_full})"
    );
    assert_eq!(
        classic, streaming,
        "classic vs streaming (size={size} tiff={tiff} yield={yield_on_full})"
    );
});
