#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|raw_data: &[u8]| {
    let mut decoder = weezl::decode::Decoder::new(weezl::BitOrder::Lsb, 0);
    let _ = decoder.into_stream(std::io::sink())
        .decode_all(raw_data);
});
