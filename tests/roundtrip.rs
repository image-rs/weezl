use weezl::{BitOrder, encode, decode};
use std::{env, fs};

#[test]
fn roundtrip_all() {
    let file = env::args().next().unwrap();
    dbg!(&file);
    let data = fs::read("Cargo.lock").unwrap();

    for bit_order in &[BitOrder::Lsb, BitOrder::Msb] {
        for bit_width in (2..8).rev() {
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
    eprintln!("Roundtrip test {:?} {}", bit_order, bit_width);
    let mut encoder = encode::Encoder::with_tiff_size_switch(bit_order, bit_width);
    let mut buffer = Vec::with_capacity(2*data.len() + 40);
    let _ = encoder.into_stream(&mut buffer).encode_all(data);
    fs::write("/tmp/encoded", buffer.as_slice()).unwrap();

    let mut decoder = decode::Decoder::with_tiff_size_switch(bit_order, bit_width);
    let mut compare = vec![];
    let result = decoder.into_stream(&mut compare).decode_all(buffer.as_slice());
    fs::write("/tmp/decoded0", data).unwrap();
    fs::write("/tmp/decoded1", compare.as_slice()).unwrap();
    assert!(result.status.is_ok(), "{:?}, {}, {:?}", bit_order, bit_width, result.status);
    assert!(data == &*compare, "{:?}, {}", bit_order, bit_width);
}
