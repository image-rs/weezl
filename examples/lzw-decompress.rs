//! Decompresses the input from stdin and writes the result to stdout.

use std::io::{self, BufWriter};

fn main() {
    match (|| -> io::Result<()> {
        let mut decoder = weezl::relzw::Decoder::new(weezl::BitOrder::Msb, 8);
        let stdout = io::stdout();
        let stdout = BufWriter::new(stdout.lock());
        let stdin = io::stdin();
        let stdin = stdin.lock();
        decoder.into_stream(stdout).decode_all(stdin).status?;
        Ok(())
    })() {
        Ok(()) => (),
        Err(err) => eprintln!("{}", err),
    }
}
