#![no_main]

//! Feeds arbitrary bytes to sirocco's decoder in pieces, through a buffer
//! smaller than the output can be, and checks it agrees with zlib-rs.

use compare::{assert_agree, reference_decompress, sirocco_stream, Format};
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
    for format in Format::ALL {
        assert_agree(
            &sirocco_stream(format, input, &ins, buf_len),
            &reference_decompress(format, input, 1 << 22),
            &format!("{format:?}"),
        );
    }
});
