use std::{env, io, fs, process};
use std::path::PathBuf;

fn main() {
    let mut args = env::args().skip(1);
    let (input, operation) = match args.next().as_ref().map(String::as_str) {
        Some("-") => (Input::Stdin, Operation::Encode),
        Some("-d") => {
            match args.next().as_ref().map(|s| s.as_str()) {
                Some("-") => (Input::Stdin, Operation::Decode),
                Some(file) => (Input::File(file.into()), Operation::Decode),
                None => explain(),
            }
        },
        Some("-e") => {
            match args.next().as_ref().map(|s| s.as_str()) {
                Some("-") => (Input::Stdin, Operation::Encode),
                Some(file) => (Input::File(file.into()), Operation::Encode),
                None => explain(),
            }
        },
        Some(file) => (Input::File(file.into()), Operation::Encode),
        None => explain(),
    };

    let min_code = 8;
    let out = io::stdout();
    let out = out.lock();

    let result = match (input, operation) {
        (Input::File(file), Operation::Encode) => {
            lzw::encode(
                fs::File::open(file).unwrap(),
                lzw::MsbWriter::new(out),
                min_code)
        },
        (Input::Stdin, Operation::Encode) => {
            let stdin = io::stdin();
            lzw::encode(
                stdin.lock(),
                lzw::MsbWriter::new(out),
                min_code)
        },
        (Input::File(file), Operation::Decode) => (|| {
            let data = fs::read(file)?;
            let mut decoder = lzw::relzw::Decoder::new(lzw::relzw::ByteOrder::Msb, 8);
            let mut outbuf = vec![0; 1 << 12];
            let mut outmark = 0;
            let mut data = data.as_slice();
            loop {
                let result = decoder.decode_bytes(data, &mut outbuf[outmark..]);
                let done = result.status.map_err(|err| io::Error::new(
                        io::ErrorKind::InvalidData, &*format!("{:?}", err)
                    ))?;
                data = &data[result.consumed_in..];
                outmark += result.consumed_out;
                if let lzw::relzw::LzwStatus::Done = done {
                    break;
                }
                if let lzw::relzw::LzwStatus::NoProgress = done {
                    if outmark == outbuf.len() {
                        let next_len = outbuf.len() * 2;
                        outbuf.resize(next_len, 0u8);
                    } else {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof, ""
                        ));
                    }
                }
            }
            outbuf.truncate(outmark);
            io::copy(&mut outbuf.as_slice(), &mut {out})?;
            Ok(())
        })(),
        _ => unimplemented!("decoding is not implemented"),
    };

    result.expect("Operation Failed: ");
}

enum Input {
    File(PathBuf),
    Stdin,
}

enum Operation {
    Encode,
    Decode,
}

fn explain<T>() -> T {
    println!("Usage: lzw [-e|-d] <file>\n\
        Arguments:\n\
        -e\t operation encode (default)\n\
        -d\t operation decode\n\
        <file>\tfilepath or '-' for stdin");
    process::exit(1);
}
