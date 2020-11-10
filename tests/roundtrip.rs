use weezl::{BitOrder, encode, decode};
use std::{env, fs};

#[test]
fn roundtrip_all() {
    let data = fs::read(env::args().next().unwrap()).unwrap();

    for bit_order in &[BitOrder::Msb, BitOrder::Lsb] {
        for bit_width in 1..8 {
            let data: Vec<_> = data.iter().copied().map(|b| b & ((1 << bit_width) - 1)).collect();

            assert_roundtrips(&*data, bit_width, *bit_order);
        }
    }
}

fn assert_roundtrips(
    data: &[u8],
    bit_width: u8,
    bit_order: BitOrder,
) {
    let mut encoder = encode::Encoder::with_tiff_size_switch(bit_order, bit_width);
    let mut buffer = Vec::with_capacity(2*data.len() + 40);
    let _ = encoder.into_stream(&mut buffer).encode_all(data);

    let mut decoder = decode::Decoder::with_tiff_size_switch(bit_order, bit_width);
    let mut compare = vec![];
    let result = decoder.into_stream(&mut compare).decode_all(buffer.as_slice());
    assert!(result.status.is_ok(), "{:?}", result.status);
    assert_eq!(data, &*compare);
}
