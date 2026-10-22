#![no_main]

//! Feeds arbitrary bytes to sirocco's decoder in pieces, through a buffer
//! smaller than the output can be, and checks it agrees with libdeflate and
//! zlib-rs (see `compare::assert_agree`).

use compare::{assert_agree, sirocco_stream, Format, ReferenceDecompressor};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The first four bytes pick the piece and buffer sizes.
    let (ins, buf_len, input) = match data {
        [a, b, c, d, rest @ ..] => {
            let ins = [usize::from(*a) + 1, usize::from(u16::from_le_bytes([*b, *a])) + 1];
            let buf_len = (64 << 10) + usize::from(u16::from_le_bytes([*c, *d]));
            (ins, buf_len, rest)
        }
        _ => return,
    };
    let mut theirs = ReferenceDecompressor::new();
    for format in Format::ALL {
        assert_agree(
            &sirocco_stream(format, input, &ins, buf_len),
            &theirs.decompress(format, input, 1 << 22),
            format,
            input,
            1 << 22,
            &format!("{format:?}"),
        );
    }
});
