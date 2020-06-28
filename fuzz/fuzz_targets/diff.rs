#![no_main]
use weezl::{Decoder, MsbReader, relzw};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    const CUT_OFF: usize = 1 << 20;

    let mut detailed_ref = None;
    let reference = (|| {
        let mut decoder = Decoder::new(MsbReader::new(), 8);
        let mut data = data;
        let mut output = vec![];
        while !data.is_empty() && output.len() < CUT_OFF {
            match decoder.decode_bytes(data) {
                Ok((0, _)) => break,
                Ok((len, decoded)) => {
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
        let mut decoder = weezl::relzw::Decoder::new(weezl::relzw::ByteOrder::Msb, 8);
        let mut data = data;
        let mut output = vec![0; CUT_OFF];
        let mut out = output.as_mut_slice();
        while !(data.is_empty() && out.is_empty()) {
            let result = decoder.decode_bytes(data, out);
            data = &data[result.consumed_in..];
            out = &mut out[result.consumed_out..];

            if let Err(err) = result.status {
                detailed_err = Some(err);
                return Err(());
            }

            if let Ok(relzw::LzwStatus::Done) = result.status {
                break;
            }

            if let Ok(relzw::LzwStatus::NoProgress) = result.status {
                break;
            }
        }
        let tail = out.len();
        let len = output.len() - tail;
        output.truncate(len);
        Ok(output)
    })();

    assert_eq!(reference, new, "{:?} vs {:?}", detailed_ref, detailed_err);
});
