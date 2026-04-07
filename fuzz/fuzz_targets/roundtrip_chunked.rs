#![no_main]
use libfuzzer_sys::fuzz_target;
use weezl::{BitOrder, decode, encode};

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    // Use first byte to select parameters
    let control = data[0];
    let payload = &data[1..];

    let order = if control & 1 == 0 {
        BitOrder::Msb
    } else {
        BitOrder::Lsb
    };
    let tiff = control & 2 != 0;
    let size = ((control >> 2) % 11) + 2; // 2..=12

    // Clamp payload to valid range
    let clamped: Vec<u8> = if size >= 8 {
        payload.to_vec()
    } else {
        let m = (1u16 << size) as u16;
        payload.iter().map(|&b| (u16::from(b) % m) as u8).collect()
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

    // Decode with classic table (default)
    let classic = if tiff {
        decode::Decoder::with_tiff_size_switch(order, size)
    } else {
        decode::Decoder::new(order, size)
    }
    .decode(&encoded)
    .expect("classic decode failed on valid encoded data");

    // Decode with chunked table
    let mut config = if tiff {
        decode::Configuration::with_tiff_size_switch(order, size)
    } else {
        decode::Configuration::new(order, size)
    };
    config = config.with_table_strategy(decode::TableStrategy::Chunked);
    let chunked = config
        .build()
        .decode(&encoded)
        .expect("chunked decode failed on valid encoded data");

    assert_eq!(clamped, classic, "classic roundtrip mismatch (size={size})");
    assert_eq!(clamped, chunked, "chunked roundtrip mismatch (size={size})");
    assert_eq!(classic, chunked, "classic vs chunked differ (size={size})");
});
