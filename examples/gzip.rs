//! Compresses standard input to standard output as gzip, at the level given
//! (default 6):
//!
//! ```sh
//! cargo run --release --example gzip -- 9 < file > file.gz
//! ```

use std::io::{self, Read, Write};

use sirocco::deflate::{Level, Writer};
use sirocco::Format;

fn main() -> io::Result<()> {
    let level = match std::env::args().nth(1) {
        Some(l) => Level::new(
            l.parse()
                .map_err(|_| io::Error::other("the level is 0 to 9"))?,
        ),
        None => Level::DEFAULT,
    };
    let mut w = Writer::new(Format::Gzip, level, io::stdout().lock());
    let mut buf = vec![0; 1 << 16];
    let mut input = io::stdin().lock();
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
    }
    w.finish()?.flush()
}
