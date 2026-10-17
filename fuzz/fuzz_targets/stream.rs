#![no_main]

//! Feeds arbitrary bytes to sirocco's streaming decompressor in pieces and
//! checks it agrees with libdeflate's one-shot decompressor.

use compare::{sirocco_stream, Format, ReferenceDecompressor};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The first four bytes pick the input and output piece sizes.
    let (ins, outs, input) = match data {
        [a, b, c, d, rest @ ..] => {
            let ins = [usize::from(*a) + 1, usize::from(u16::from_le_bytes([*b, *a])) + 1];
            let outs = [usize::from(*c) + 1, usize::from(u16::from_le_bytes([*c, *d])) + 1];
            (ins, outs, rest)
        }
        _ => return,
    };
    let mut theirs = ReferenceDecompressor::new();
    for format in Format::ALL {
        assert_eq!(
            sirocco_stream(format, input, &ins, &outs),
            theirs.decompress(format, input, 1 << 22),
            "{format:?}"
        );
    }
});
