//! Helpers for testing and benchmarking sirocco's compressor against zlib
//! and the other DEFLATE libraries.
//!
//! zlib here is madler's zlib (bundled by `libz-sys`), through `flate2`:
//! the reference both for compressed sizes, level by level, and for which
//! streams are valid.

use sirocco::deflate::{Encoder, Flush, Level};

use crate::{Format, Outcome, ReferenceDecompressor};

/// Compresses `data` with zlib at `level` (0-9).
#[must_use]
pub fn zlib_compress(format: Format, level: u32, data: &[u8]) -> Vec<u8> {
    let level = flate2::Compression::new(level);
    let mut c = match format {
        Format::Deflate => flate2::Compress::new(level, false),
        Format::Zlib => flate2::Compress::new(level, true),
        Format::Gzip => flate2::Compress::new_gzip(level, 15),
    };
    let mut out = Vec::with_capacity(data.len() + data.len() / 100 + 64);
    loop {
        let consumed = c.total_in() as usize;
        let st = c.compress_vec(&data[consumed..], &mut out, flate2::FlushCompress::Finish);
        if st.unwrap() == flate2::Status::StreamEnd {
            return out;
        }
        out.reserve(out.capacity());
    }
}

/// Decompresses `data`, which must be exactly one stream of at most
/// `max_len` bytes of output, with zlib.
pub fn zlib_decompress(format: Format, data: &[u8], max_len: usize) -> Result<Vec<u8>, String> {
    let mut d = match format {
        Format::Deflate => flate2::Decompress::new(false),
        Format::Zlib => flate2::Decompress::new(true),
        Format::Gzip => flate2::Decompress::new_gzip(15),
    };
    let mut out = Vec::with_capacity(max_len);
    let st = d
        .decompress_vec(data, &mut out, flate2::FlushDecompress::Finish)
        .map_err(|e| format!("zlib: {e}"))?;
    if st != flate2::Status::StreamEnd {
        return Err(format!(
            "zlib: {st:?} after {} of {} bytes",
            d.total_in(),
            data.len()
        ));
    }
    if d.total_in() as usize != data.len() {
        return Err(format!(
            "zlib: the stream ends at {} of {}",
            d.total_in(),
            data.len()
        ));
    }
    Ok(out)
}

/// Decompresses the start of a stream that goes on (as after a sync flush)
/// with zlib: all the output `data` gives.
pub fn zlib_decompress_prefix(
    format: Format,
    data: &[u8],
    max_len: usize,
) -> Result<Vec<u8>, String> {
    let mut d = match format {
        Format::Deflate => flate2::Decompress::new(false),
        Format::Zlib => flate2::Decompress::new(true),
        Format::Gzip => flate2::Decompress::new_gzip(15),
    };
    let mut out = Vec::with_capacity(max_len);
    d.decompress_vec(data, &mut out, flate2::FlushDecompress::Sync)
        .map_err(|e| format!("zlib: {e}"))?;
    if d.total_in() as usize != data.len() {
        return Err(format!(
            "zlib: stopped at {} of {}",
            d.total_in(),
            data.len()
        ));
    }
    Ok(out)
}

/// Compresses `data` with sirocco in one call.
#[must_use]
pub fn sirocco_compress(format: Format, level: u8, data: &[u8]) -> Vec<u8> {
    sirocco::compress(format.ff(), Level::new(level), data)
}

/// Checks that every decompressor turns `compressed` into `data`: zlib,
/// zlib-rs, libdeflate, miniz_oxide (not for gzip) and sirocco.
#[track_caller]
pub fn check_round_trip(
    format: Format,
    compressed: &[u8],
    data: &[u8],
    what: &dyn std::fmt::Display,
) {
    let len = data.len();
    match zlib_decompress(format, compressed, len) {
        Ok(out) => assert!(out == data, "zlib's output differs: {what}"),
        Err(e) => panic!("{e}: {what}"),
    }
    assert_eq!(
        ReferenceDecompressor::new().decompress(format, compressed, len),
        Outcome::Ok(compressed.len(), data.to_vec()),
        "zlib-rs: {what}"
    );
    let mut d = libdeflater::Decompressor::new();
    let mut out = vec![0; len];
    let n = match format {
        Format::Deflate => d.deflate_decompress(compressed, &mut out),
        Format::Zlib => d.zlib_decompress(compressed, &mut out),
        Format::Gzip => d.gzip_decompress(compressed, &mut out),
    };
    assert!(
        n.is_ok_and(|n| n == len) && out == data,
        "libdeflate: {what}"
    );
    let miniz = match format {
        Format::Deflate => Some(miniz_oxide::inflate::decompress_to_vec(compressed)),
        Format::Zlib => Some(miniz_oxide::inflate::decompress_to_vec_zlib(compressed)),
        Format::Gzip => None,
    };
    if let Some(m) = miniz {
        assert!(m.is_ok_and(|m| m == data), "miniz_oxide: {what}");
    }
    let s = sirocco::decompress(format.ff(), compressed, len);
    assert!(s.is_ok_and(|s| s == data), "sirocco: {what}");
}

/// One step of a streaming compression: add `add` bytes of input, then
/// compress with `flush`, into output pieces of `out_len` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub add: usize,
    pub flush: Flush,
    pub out_len: usize,
}

/// A streamed compression's output, and where its sync and full flushes
/// were: (input so far, output so far, full).
#[derive(Debug)]
pub struct Streamed {
    pub compressed: Vec<u8>,
    pub flushes: Vec<(usize, usize, bool)>,
}

/// Compresses `data` with sirocco through a `buf_len`-byte buffer (at
/// least `MIN_BUF_LEN`), following `steps` (cycled) until the input runs
/// out, then finishing.
#[must_use]
pub fn sirocco_compress_stream(
    format: Format,
    level: u8,
    data: &[u8],
    steps: &[Step],
    buf_len: usize,
) -> Streamed {
    let mut e = Encoder::new(format.ff(), Level::new(level));
    let mut buf = vec![0; buf_len];
    let mut piece = vec![0; steps.iter().map(|s| s.out_len).max().unwrap_or(1).max(1)];
    let (mut len, mut fed) = (0, 0);
    let mut s = Streamed {
        compressed: Vec::new(),
        flushes: Vec::new(),
    };
    let mut run = |e: &mut Encoder, buf: &[u8], out: &mut Vec<u8>, flush, out_len: usize| loop {
        let p = e.compress(buf, &mut piece[..out_len.max(1)], flush);
        out.extend_from_slice(p.output);
        if p.done || !p.full {
            return;
        }
    };
    for step in steps.iter().cycle() {
        let mut add = step.add.min(data.len() - fed);
        while add > 0 {
            if len == buf_len {
                run(
                    &mut e,
                    &buf[..len],
                    &mut s.compressed,
                    Flush::None,
                    step.out_len,
                );
                len -= e.make_room(&mut buf[..len]);
                assert!(len < buf_len, "make_room freed nothing");
            }
            let n = add.min(buf_len - len);
            buf[len..len + n].copy_from_slice(&data[fed..fed + n]);
            (len, fed, add) = (len + n, fed + n, add - n);
        }
        run(
            &mut e,
            &buf[..len],
            &mut s.compressed,
            step.flush,
            step.out_len,
        );
        if matches!(step.flush, Flush::Sync | Flush::Full) {
            assert_eq!(e.total_in(), fed as u64, "a flush leaves no input behind");
            let full = step.flush == Flush::Full;
            s.flushes.push((fed, s.compressed.len(), full));
        }
        if fed == data.len() || steps.iter().all(|s| s.add == 0) {
            break;
        }
    }
    let last = steps.last().map_or(1 << 16, |s| s.out_len);
    run(&mut e, &buf[..len], &mut s.compressed, Flush::Finish, last);
    assert_eq!(e.total_in(), data.len() as u64);
    assert_eq!(e.total_out(), s.compressed.len() as u64);
    s
}

/// Checks a streamed compression of `data`: the whole stream with
/// [`check_round_trip`], and with zlib that each sync or full flush ends
/// with `00 00 FF FF` and makes all of the input so far decompressible, and
/// that raw DEFLATE after a full flush decompresses alone.
#[track_caller]
pub fn check_stream(format: Format, s: &Streamed, data: &[u8], what: &dyn std::fmt::Display) {
    check_round_trip(format, &s.compressed, data, what);
    for &(fed, at, full) in &s.flushes {
        let c = &s.compressed[..at];
        assert!(
            c.ends_with(&[0, 0, 0xFF, 0xFF]),
            "no flush marker at {at}: {what}"
        );
        match zlib_decompress_prefix(format, c, data.len()) {
            Ok(out) => assert!(
                out == data[..fed],
                "zlib's output to a flush differs: {what}"
            ),
            Err(e) => panic!("{e} to a flush: {what}"),
        }
        if full && format == Format::Deflate {
            let rest = zlib_decompress(format, &s.compressed[at..], data.len());
            assert!(
                rest.is_ok_and(|r| r == data[fed..]),
                "after a full flush at {at}: {what}"
            );
        }
    }
}
