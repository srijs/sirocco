#![no_main]

//! Compresses arbitrary bytes with sirocco in pieces, with flushes, into
//! output pieces of any size, and checks the stream and each flush point
//! with zlib (and the whole stream with the other decompressors).

use compare::deflate::{check_stream, sirocco_compress_stream, Step};
use compare::Format;
use libfuzzer_sys::fuzz_target;
use sirocco::deflate::Flush;

fuzz_target!(|data: &[u8]| {
    // The first 12 bytes pick the level, format, buffer and steps; the input
    // is the rest, repeated (with a changing byte between) up to 64 times, so
    // that it is large enough to fill the buffer and slide the window.
    if data.len() < 12 {
        return;
    }
    let (ctl, rest) = data.split_at(12);
    let level = ctl[0] % 10;
    let format = Format::ALL[usize::from(ctl[1]) % 3];
    let buf_len = if ctl[2] & 1 == 0 { sirocco::deflate::MIN_BUFFER_LEN } else { 1 << 20 };
    let reps = usize::from(ctl[3] % 64) + 1;
    let mut input = Vec::with_capacity((rest.len() + 1) * reps);
    for i in 0..reps {
        input.extend_from_slice(rest);
        input.push(i as u8);
    }
    let steps: Vec<Step> = ctl[4..]
        .chunks(2)
        .map(|c| {
            let add = (usize::from(c[0]) + 1).pow(2) * 2;
            let flush = match c[1] % 8 {
                0 => Flush::Sync,
                1 => Flush::Full,
                _ => Flush::None,
            };
            let out_len = [1, 7, 100, 4096, 1 << 20][usize::from(c[1] >> 3) % 5];
            Step { add, flush, out_len }
        })
        .collect();
    let s = sirocco_compress_stream(format, level, &input, &steps, buf_len);
    check_stream(format, &s, &input, &format!("level {level} {format:?} {steps:?}"));
});
