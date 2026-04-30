use weezl::{decode::Configuration, encode, BitOrder, LzwStatus};

#[test]
fn fill_into_buffer_counts_as_progress() {
    let data = vec![42u8; 8];
    let encoded = encode::Encoder::new(BitOrder::Lsb, 8)
        .encode(&data)
        .unwrap();

    let mut dec = Configuration::new(BitOrder::Lsb, 8)
        .with_yield_on_full_buffer(true)
        .build();

    let mut result = Vec::new();
    let mut inp = encoded.as_slice();
    loop {
        let mut tmp = [0u8; 1]; // 1-byte output buffer
        let r = dec.decode_bytes(inp, &mut tmp);
        inp = &inp[r.consumed_in..];
        result.extend_from_slice(&tmp[..r.consumed_out]);
        match r.status {
            Ok(LzwStatus::Done) => break,
            Ok(LzwStatus::NoProgress) if r.consumed_in == 0 && r.consumed_out == 0 => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }

    assert_eq!(result.len(), 8); // FAILS: result.len() == 1
}
