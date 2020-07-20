use std::{env, io, fs, process};
use std::path::PathBuf;

extern crate weezl;
use weezl::{enlzw, relzw, BitOrder};

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
        (Input::File(file), Operation::Encode) => (|| {
            let data = fs::File::open(file)?;
            let file = io::BufReader::with_capacity(1 << 26, data);

            let mut encoder = enlzw::Encoder::new(BitOrder::Msb, 8);
            encoder.into_stream(out).encode_all(file).status
        })(),
        (Input::Stdin, Operation::Encode) => {
            let input = io::BufReader::with_capacity(1 << 26, io::stdin());
            let mut encoder = enlzw::Encoder::new(BitOrder::Msb, 8);
            encoder.into_stream(out).encode_all(input).status
        },
        (Input::File(file), Operation::Decode) => (|| {
            let data = fs::File::open(file)?;
            let file = io::BufReader::with_capacity(1 << 26, data);

            let mut decoder = relzw::Decoder::new(BitOrder::Msb, 8);
            decoder.into_stream(out).decode_all(file).status
        })(),
        (Input::Stdin, Operation::Decode) => {
            let input = io::BufReader::with_capacity(1 << 26, io::stdin());
            let mut decoder = relzw::Decoder::new(BitOrder::Msb, 8);
            decoder.into_stream(out).decode_all(input).status
        }
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
