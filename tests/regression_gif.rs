use weezl::{decode, BitOrder};

/// See:https://github.com/image-rs/image-gif/issues/97
#[test]
fn regression_gif_97() {
    // The data stream contains three symbols, in bits: 10, 00, (0)11
    // That is the interpretation must be: clear, (0), end
    // It is ambiguous if the end symbol is coded as 2 or 3 bits but in order to be able to code a
    // next code we assume a size switch. However, the first data symbol _must_ be coded as 2 bits
    // otherwise it would code 100 which invalidly codes the next-code before any derivation.
    // Similarly, if we always assumed 3 bits then it would be (011) (110), coding the invalid
    // symbol 110 that is even greater than the next-code.
    let data = &[0x32];

    let mut decoder = decode::Configuration::new(BitOrder::Lsb, 1)
        .build();

    let mut output = vec![];
    let result = decoder.into_vec(&mut output).decode_all(data);
    assert!(matches!(result.status, Ok(weezl::LzwStatus::Ok)));
    assert_eq!(result.consumed_in, 1);
    assert_eq!(output, &[0]);
}
