#![forbid(unsafe_code)]
use std::path::PathBuf;
use std::{env, ffi, fs, io, process};

use weezl::{decode as delzw, encode as enlzw, BitOrder};

fn main() -> CodingResult {
    CodingResult::catch_panic(|| {
        let flags = Flags::from_args(env::args_os()).unwrap_or_else(|ParamError| explain());
        run_coding(flags)
    })
}

fn run_coding(flags: Flags) -> Result<(), io::Error> {
    let mut files = flags.files;
    let input = files.pop().unwrap_or_else(explain);
    if !files.is_empty() {
        return explain();
    }
    let operation = flags.operation.unwrap_or_else(explain);
    let min_code = if flags.min_code < 2 || flags.min_code > 12 {
        return explain();
    } else {
        flags.min_code
    };
    let bit_order = flags.bit_order;

    let (encoder, decoder): (fn(BitOrder, u8) -> _, fn(BitOrder, u8) -> _) = match flags.style {
        Style::Gif => (enlzw::Encoder::new, delzw::Decoder::new),
        Style::Tiff => (
            enlzw::Encoder::with_tiff_size_switch,
            delzw::Decoder::with_tiff_size_switch,
        ),
    };

    let out = io::stdout();
    let out = out.lock();

    match (input, operation) {
        (Input::File(file), Operation::Encode) => {
            let data = fs::File::open(file)?;
            let file = io::BufReader::with_capacity(1 << 26, data);

            let mut encoder = encoder(bit_order, min_code);
            encoder.into_stream(out).encode_all(file).status
        }
        (Input::Stdin, Operation::Encode) => {
            let input = io::BufReader::with_capacity(1 << 26, io::stdin());
            let mut encoder = encoder(bit_order, min_code);
            encoder.into_stream(out).encode_all(input).status
        }
        (Input::File(file), Operation::Decode) => {
            let data = fs::File::open(file)?;
            let file = io::BufReader::with_capacity(1 << 26, data);

            let mut decoder = decoder(bit_order, min_code);
            decoder.into_stream(out).decode_all(file).status
        }
        (Input::Stdin, Operation::Decode) => {
            let input = io::BufReader::with_capacity(1 << 26, io::stdin());
            let mut decoder = decoder(bit_order, min_code);
            decoder.into_stream(out).decode_all(input).status
        }
    }
}

struct Flags {
    files: Vec<Input>,
    operation: Option<Operation>,
    min_code: u8,
    bit_order: BitOrder,
    style: Style,
}

struct ParamError;

#[derive(Debug)]
enum Input {
    File(PathBuf),
    Stdin,
}

#[derive(Debug)]
enum Operation {
    Encode,
    Decode,
}

#[derive(Debug)]
enum Style {
    Gif,
    Tiff,
}

fn explain<T>() -> T {
    println!(
        "Usage: lzw [-e|-d] <file>\n\
        Arguments:\n\
        -e\t operation encode (default)\n\
        -d\t operation decode\n\
        <file>\tfilepath or '-' for stdin"
    );
    process::exit(1);
}

impl Default for Flags {
    fn default() -> Flags {
        Flags {
            files: vec![],
            operation: None,
            min_code: 8,
            bit_order: BitOrder::Msb,
            style: Style::Gif,
        }
    }
}

fn command() -> clap::Command<'static> {
    clap::Command::new("weezl")
        .author("Andreas Molzer")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Interact with lzw binary data")
        .arg(
            clap::Arg::new("decode")
                .short('d')
                .long("--decode")
                .takes_value(false),
        )
        .arg(
            clap::Arg::new("encode")
                .short('e')
                .long("--encode")
                .takes_value(false),
        )
        .group(
            clap::ArgGroup::new("operation")
                .args(&["decode", "encode"])
                .multiple(false)
                .required(true),
        )
        .arg(
            clap::Arg::new("min_code")
                .short('w')
                .long("--word-bits")
                .takes_value(true)
                .value_parser(2..12),
        )
        .arg(
            clap::Arg::new("bit_order")
                .short('b')
                .long("--bit-order")
                .takes_value(true)
                .value_parser(["l", "m", "least", "most"]),
        )
        .arg(
            clap::Arg::new("style")
                .short('s')
                .long("--style")
                .default_value("gif")
                .value_parser(["gif", "tiff"]),
        )
        .arg(
            clap::Arg::new("file")
                .default_value("-")
                .value_parser(clap::builder::ValueParser::path_buf()),
        )
}

impl Flags {
    fn from_args(mut args: impl Iterator<Item = ffi::OsString>) -> Result<Self, ParamError> {
        let mut flags = Flags::default();
        let matches = command().get_matches_from(args.by_ref());

        if matches.contains_id("decode") {
            flags.operation = Some(Operation::Decode);
        } else if matches.contains_id("encode") {
            flags.operation = Some(Operation::Encode);
        }

        if let Some(&m) = matches.get_one::<u8>("min_code") {
            flags.min_code = m;
        }

        match matches.get_one::<String>("bit_order").map(String::as_str) {
            Some("m") => flags.bit_order = BitOrder::Msb,
            Some("l") => flags.bit_order = BitOrder::Lsb,
            Some("most") => flags.bit_order = BitOrder::Msb,
            Some("least") => flags.bit_order = BitOrder::Lsb,
            Some(_) => unreachable!("unparsed bit order"),
            _ => {}
        }

        match matches.get_one::<String>("style").map(String::as_str) {
            Some("gif") => flags.style = Style::Gif,
            Some("tiff") => flags.style = Style::Tiff,
            Some(_) => unreachable!("unparsed style"),
            _ => {}
        }

        match matches.get_one::<PathBuf>("file") {
            None => flags.files = vec![Input::Stdin],
            Some(p) if *p == PathBuf::from("-") => flags.files = vec![Input::Stdin],
            Some(p) => flags.files = vec![Input::File(p.clone())],
        }

        Ok(flags)
    }
}

enum CodingResult {
    Ok,
    Err(io::Error),
    Panic,
}

impl CodingResult {
    fn catch_panic(op: fn() -> Result<(), io::Error>) -> Self {
        std::panic::catch_unwind(|| match op() {
            Ok(()) => CodingResult::Ok,
            Err(err) => CodingResult::Err(err),
        })
        .unwrap_or(CodingResult::Panic)
    }
}

impl std::process::Termination for CodingResult {
    fn report(self) -> std::process::ExitCode {
        match self {
            CodingResult::Ok => std::process::ExitCode::SUCCESS,
            CodingResult::Err(err) => {
                eprintln!("{}", err);
                std::process::ExitCode::FAILURE
            }
            CodingResult::Panic => {
                eprintln!(
                    "The process failed irrecoverably! This should never happen and is a bug."
                );
                eprintln!("If you know what this means, please report it to:");
                eprintln!("	<{}>", env!("CARGO_PKG_REPOSITORY"));
                std::process::ExitCode::from(128)
            }
        }
    }
}
