use std::{env, io, ffi, fs, process};
use std::path::PathBuf;

extern crate weezl;
use weezl::{encode as enlzw, decode as delzw, BitOrder};

fn main() {
    let args = env::args_os().skip(1);
    let flags = Flags::from_args(args);

    let out = io::stdout();
    let out = out.lock();

    let input = flags.file.unwrap_or_else(explain);
    let operation = flags.operation.unwrap_or_else(explain);
    let min_code = if flags.min_code < 2 || flags.min_code > 12 {
        return explain();
    } else { flags.min_code };
    let bit_order = flags.bit_order;

    let result = match (input, operation) {
        (Input::File(file), Operation::Encode) => (|| {
            let data = fs::File::open(file)?;
            let file = io::BufReader::with_capacity(1 << 26, data);

            let mut encoder = enlzw::Encoder::new(bit_order, min_code);
            encoder.into_stream(out).encode_all(file).status
        })(),
        (Input::Stdin, Operation::Encode) => {
            let input = io::BufReader::with_capacity(1 << 26, io::stdin());
            let mut encoder = enlzw::Encoder::new(bit_order, min_code);
            encoder.into_stream(out).encode_all(input).status
        },
        (Input::File(file), Operation::Decode) => (|| {
            let data = fs::File::open(file)?;
            let file = io::BufReader::with_capacity(1 << 26, data);

            let mut decoder = delzw::Decoder::new(bit_order, min_code);
            decoder.into_stream(out).decode_all(file).status
        })(),
        (Input::Stdin, Operation::Decode) => {
            let input = io::BufReader::with_capacity(1 << 26, io::stdin());
            let mut decoder = delzw::Decoder::new(bit_order, min_code);
            decoder.into_stream(out).decode_all(input).status
        }
    };

    result.expect("Operation Failed: ");
}

struct Flags {
    file: Option<Input>,
    operation: Option<Operation>,
    min_code: u8,
    bit_order: BitOrder,
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

impl Default for Flags {
    fn default() -> Flags {
        Flags {
            file: None,
            operation: None,
            min_code: 8,
            bit_order: BitOrder::Msb,
        }
    }
}

impl Flags {
    fn from_args(mut args: impl Iterator<Item=ffi::OsString>) -> Self {
        let mut flags = Flags::default();
        let (input, operation) = match args.next().as_ref().and_then(|s| s.to_str()) {
            Some("-") => (Input::Stdin, Operation::Encode),
            Some("-d") => {
                match args.next().as_ref() {
                    Some(arg) if arg.to_str() == Some("-") => (Input::Stdin, Operation::Decode),
                    Some(file) => (Input::File(file.into()), Operation::Decode),
                    None => explain(),
                }
            },
            Some("-e") => {
                match args.next().as_ref() {
                    Some(arg) if arg.to_str() == Some("-") => (Input::Stdin, Operation::Encode),
                    Some(file) => (Input::File(file.into()), Operation::Encode),
                    None => explain(),
                }
            },
            Some(file) => (Input::File(file.into()), Operation::Encode),
            None => explain(),
        };
        flags.file = Some(input);
        flags.operation = Some(operation);
        flags
    }
}
