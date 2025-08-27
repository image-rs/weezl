#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|raw_data: &[u8]| {
    // No implicit restart in `lzw` so make sure there is one. Otherwise we get an instant
    // detection that is not helpful at all.
    let mut data = vec![0b1000_0000];
    data.extend_from_slice(raw_data);
    let data = data.as_slice();
    const CUT_OFF: usize = 1 << 20;

    let mut detailed_ref: Option<_> = None;
    let reference = (|| {
        let mut decoder = lzw::Decoder::new(lzw::LsbReader::new(), 7);
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
        let mut decoder = weezl::decode::Decoder::new(weezl::BitOrder::Lsb, 7);
        let mut output = Vec::with_capacity(CUT_OFF);
        let err = decoder.into_stream(&mut output).decode(data);
        if let Err(err) = err.status {
            detailed_err = Some(err);
            return Err(());
        }
        output.truncate(CUT_OFF);
        Ok(output)
    })();

    // Output my be omitted if the stream did not end properly in an end code.
    let ref_len = reference.as_ref().map_or(usize::MAX, |x| x.len());
    let new_len = new.as_ref().map_or(usize::MAX, |x| x.len());

    let reference = reference.map(|mut vec| {
        vec.truncate(ref_len.min(new_len));
        vec
    });

    let new = new.map(|mut vec| {
        vec.truncate(ref_len.min(new_len));
        vec
    });

    assert_eq!(reference, new, "{:?} vs {:?}", detailed_ref, detailed_err);
});
