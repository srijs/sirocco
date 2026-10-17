#![no_main]

//! Compresses arbitrary bytes with libdeflate at every level and checks that
//! sirocco decompresses them back.

use compare::{sirocco_decompress, reference_compress, Format, Outcome};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&level, data)) = data.split_first() else {
        return;
    };
    let level = i32::from(level % 13);
    let mut d = sirocco::Decompressor::new(sirocco::Format::Deflate);
    for format in Format::ALL {
        let c = reference_compress(format, level, data);
        assert_eq!(
            sirocco_decompress(&mut d, format, &c, data.len()),
            Outcome::Ok(c.len(), data.to_vec()),
            "{format:?} level {level}"
        );
    }
});
