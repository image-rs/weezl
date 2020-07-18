#![no_main]
use libfuzzer_sys::fuzz_target;
use weezl::{enlzw, relzw};

fuzz_target!(|data: &[u8]| {

    let mut encoder = enlzw::Encoder::new(relzw::ByteOrder::Msb, 8);
    encoder.finish();
    let mut buffer = vec![0; 2*data.len() + 40];

    let mut input = data;
    let mut output = buffer.as_mut_slice();
    let mut length = 0;

    loop {
        let result = encoder.encode_bytes(input, output);
        input = &input[result.consumed_in..];
        length += result.consumed_out;
        output = &mut output[result.consumed_out..];

        if let Err(_) = result.status {
            break;
        }

        if let Ok(relzw::LzwStatus::NoProgress) = result.status {
            break;
        }

        if let Ok(relzw::LzwStatus::Done) = result.status {
            break;
        }
    }

    buffer.truncate(length);

    let mut decoder = relzw::Decoder::new(relzw::ByteOrder::Msb, 8);
    let mut compare = vec![];
    let result = decoder.decode_all(buffer.as_slice(), &mut compare);
    // dbg!(buffer.as_slice());
    // dbg!(&result.status);
    assert!(result.status.is_ok(), "{:?}", result.status);
});
