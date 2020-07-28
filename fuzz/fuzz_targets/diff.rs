#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|raw_data: &[u8]| {
    let mut data = vec![0b10];
    data.extend_from_slice(raw_data);
    let data = data.as_slice();
    const CUT_OFF: usize = 1 << 20;

    let mut detailed_ref: Option<_> = None;
    let reference = (|| {
        let mut decoder = lzw::Decoder::new(lzw::LsbReader::new(), 2);
        let mut data = data;
        let mut output = vec![];
        while !data.is_empty() && output.len() < CUT_OFF {
            match decoder.decode_bytes(data) {
                Ok((0, _)) => break,
                Ok((len, decoded)) => {
                    // eprintln!("Ref {:?}", decoded);
                    data = &data[len..];
                    output.extend_from_slice(decoded);
                }
                Err(err) => {
                    detailed_ref = Some(err);
                    return Err(());
                }
            }

            if decoder.has_ended() {
                break;
            }
        }
        output.truncate(CUT_OFF);
        Ok(output)
    })();

    let mut detailed_err = None;
    let new = (|| {
        let mut decoder = weezl::decode::Decoder::new(weezl::BitOrder::Lsb, 2);
        let mut output = Vec::with_capacity(CUT_OFF);
        let err = decoder.into_stream(&mut output).decode(data);
        if let Err(err) = err.status {
            detailed_err = Some(err);
            return Err(());
        }
        output.truncate(CUT_OFF);
        Ok(output)
    })();

    assert_eq!(reference, new, "{:?} vs {:?}", detailed_ref, detailed_err);
});
