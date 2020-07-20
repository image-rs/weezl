//! Compresses the input from stdin and writes the result to stdout.

use std::io::{self, BufWriter};

fn main() {
    match (|| -> io::Result<()> {
        let mut encoder = weezl::enlzw::Encoder::new(weezl::BitOrder::Msb, 8);
        let stdin = io::stdin();
        let stdin = stdin.lock();
        let stdout = io::stdout();
        let stdout = BufWriter::new(stdout.lock());
        encoder.into_stream(stdout).encode_all(stdin).status?;
        Ok(())
    })() {
        Ok(()) => (),
        Err(err) => eprintln!("{}", err),
    }
    
}
