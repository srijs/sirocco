#![no_main]

//! Decompresses arbitrary bytes with sirocco and zlib-rs in all three
//! formats and checks they agree on the outcome, the consumed input and the
//! output.

use compare::{assert_agree, sirocco_decompress, Format, ReferenceDecompressor, SiroccoDecoders};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The first two bytes pick the output buffer size, so both the
    // insufficient-space paths and the fastloop get exercised.
    let (out_len, input) = match data {
        [a, b, rest @ ..] => (usize::from(u16::from_le_bytes([*a, *b])) * 4, rest),
        _ => return,
    };
    let mut ours = SiroccoDecoders::new();
    let mut theirs = ReferenceDecompressor::new();
    for format in Format::ALL {
        assert_agree(
            &sirocco_decompress(&mut ours, format, input, out_len),
            &theirs.decompress(format, input, out_len),
            &format!("{format:?}"),
        );
    }
});
