#![no_main]

//! Compresses arbitrary bytes with sirocco at every level and format in one
//! call, and checks that the stream fits the bound and that zlib, zlib-rs,
//! libdeflate, miniz_oxide and sirocco all decompress it back.

use compare::deflate::{check_round_trip, sirocco_compress};
use compare::Format;
use libfuzzer_sys::fuzz_target;
use sirocco::deflate::compress_bound;

fuzz_target!(|data: &[u8]| {
    let Some((&ctl, data)) = data.split_first() else {
        return;
    };
    let level = ctl % 10;
    let format = Format::ALL[usize::from(ctl / 10) % 3];
    let c = sirocco_compress(format, level, data);
    assert!(c.len() <= compress_bound(format.ff(), data.len()));
    check_round_trip(format, &c, data, &format!("level {level} {format:?}"));
});
