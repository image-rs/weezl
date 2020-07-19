//! Decompresses the input from stdin and writes the result to stdout.

use std::io::{self, BufWriter};

fn main() {
    match (|| -> io::Result<()> {
        let mut decoder = weezl::relzw::Decoder::new(weezl::ByteOrder::Msb, 8);
        let stdout = io::stdout();
        let stdout = BufWriter::new(stdout.lock());
        let stdin = io::stdin();
        let stdin = stdin.lock();
        decoder.decode_all(stdin, stdout).status?;
        Ok(())
    })() {
        Ok(()) => (),
        Err(err) => eprintln!("{}", err),
    }
}
