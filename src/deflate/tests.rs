//! Round trips through [`Encoder`], [`Writer`](super::Writer) and
//! [`crate::compress`], checked with sirocco's decoder and miniz_oxide.
//! `crates/compare` checks against zlib and others.

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use proptest::prelude::*;

use super::*;
use crate::Format;

const FORMATS: [Format; 3] = [Format::Deflate, Format::Zlib, Format::Gzip];

/// Decompresses with sirocco, checking the stream ends at the end.
fn decompress(format: Format, data: &[u8]) -> Vec<u8> {
    crate::decompress(format, data, 1 << 30).unwrap()
}

/// Decompresses with miniz_oxide.
fn decompress_miniz(format: Format, data: &[u8]) -> Vec<u8> {
    use miniz_oxide::inflate::{decompress_to_vec, decompress_to_vec_zlib};
    match format {
        Format::Deflate => decompress_to_vec(data).unwrap(),
        Format::Zlib => decompress_to_vec_zlib(data).unwrap(),
        // miniz_oxide doesn't read gzip; check the body.
        _ => decompress_to_vec(&data[10..data.len() - 8]).unwrap(),
    }
}

/// Text-like data with repeats near and far, runs, and noise.
fn sample(len: usize, seed: u64) -> Vec<u8> {
    let words: [&[u8]; 8] = [
        b"the ",
        b"deflate ",
        b"zlib ",
        b"match ",
        b"of ",
        b"\n",
        b"a",
        b"stream ",
    ];
    let mut rng = seed | 1;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let r = next();
        match r % 16 {
            0 if out.len() > 100 => {
                let back = (r >> 8) as usize % out.len().min(40_000) + 1;
                let n = (r >> 32) as usize % 300;
                let start = out.len() - back;
                for i in 0..n {
                    out.push(out[start + i]);
                }
            }
            1 => out.extend(core::iter::repeat_n(
                (r >> 8) as u8,
                (r >> 16) as usize % 500,
            )),
            2 => out.extend((0..(r >> 8) % 64).map(|_| next() as u8)),
            _ => out.extend_from_slice(words[(r >> 8) as usize % words.len()]),
        }
    }
    out.truncate(len);
    out
}

#[test]
fn one_shot_round_trips() {
    let inputs = [
        Vec::new(),
        b"a".to_vec(),
        b"hello hello hello".to_vec(),
        vec![b'x'; 100_000],
        sample(1000, 1),
        sample(300_000, 2),
        (0..200_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect(),
    ];
    for data in &inputs {
        for level in 0..=9 {
            for format in FORMATS {
                let c = crate::compress(format, Level::new(level), data);
                let what = format!("level {level} {format:?} {} bytes", data.len());
                assert!(c.len() <= compress_bound(format, data.len()), "{what}");
                assert_eq!(&decompress(format, &c), data, "{what}");
                assert_eq!(&decompress_miniz(format, &c), data, "{what}");
            }
        }
    }
}

#[test]
fn headers_match_zlib() {
    for (level, flg) in [
        (0, 0x01),
        (1, 0x01),
        (2, 0x5E),
        (5, 0x5E),
        (6, 0x9C),
        (7, 0xDA),
        (9, 0xDA),
    ] {
        let c = crate::compress(Format::Zlib, Level::new(level), b"x");
        assert_eq!(c[..2], [0x78, flg], "level {level}");
    }
    let c = crate::compress(Format::Gzip, Level::new(9), b"");
    assert_eq!(c[..10], [0x1F, 0x8B, 8, 0, 0, 0, 0, 0, 2, 255]);
    // An empty stream: a static block with only an end-of-block, as zlib.
    assert_eq!(
        crate::compress(Format::Deflate, Level::DEFAULT, b""),
        [3, 0]
    );
    // Level 0: an empty stored block.
    assert_eq!(
        crate::compress(Format::Deflate, Level::NONE, b""),
        [1, 0, 0, 0xFF, 0xFF]
    );
}

#[test]
fn incompressible_input_is_stored() {
    let data: Vec<u8> = (0..100_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    for level in 0..=9 {
        let c = crate::compress(Format::Deflate, Level::new(level), &data);
        assert!(c.len() <= data.len() + 5 * 3, "level {level}: {}", c.len());
    }
}

/// One step of a streaming test: add input, then compress with a flush
/// into output pieces of a size.
#[derive(Clone, Debug)]
struct Step {
    add: usize,
    flush: Flush,
    out_len: usize,
}

fn flush_strategy() -> impl Strategy<Value = Flush> {
    prop_oneof![
        8 => Just(Flush::None),
        1 => Just(Flush::Sync),
        1 => Just(Flush::Full),
    ]
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    prop::collection::vec(
        (
            0usize..70_000,
            flush_strategy(),
            prop_oneof![1usize..20, 20usize..5000, Just(1 << 20)],
        )
            .prop_map(|(add, flush, out_len)| Step {
                add,
                flush,
                out_len,
            }),
        1..12,
    )
}

/// Compresses `data` in `steps` through a fixed `buf_len`-byte buffer,
/// checking that each sync or full flush makes all of the input so far
/// decompressible. Returns the stream.
fn stream(format: Format, level: Level, data: &[u8], steps: &[Step], buf_len: usize) -> Vec<u8> {
    let mut e = Encoder::new(format, level);
    let mut buf = vec![0; buf_len];
    let (mut len, mut fed, mut out) = (0, 0, Vec::new());
    let mut piece = vec![0; 1 << 20];
    let mut compress = |e: &mut Encoder, buf: &[u8], out: &mut Vec<u8>, flush, out_len: usize| loop {
        let p = e.compress(buf, &mut piece[..out_len], flush);
        assert!(p.output.len() <= out_len);
        out.extend_from_slice(p.output);
        if p.done || !p.full {
            break p.done;
        }
    };
    for s in steps.iter().cycle() {
        let mut add = s.add.min(data.len() - fed);
        while add > 0 {
            if len == buf_len {
                // A flush-free compress, so make_room can free the most.
                compress(&mut e, &buf[..len], &mut out, Flush::None, s.out_len);
                len -= e.make_room(&mut buf[..len]);
                assert!(len < buf_len, "make_room freed nothing");
            }
            let n = add.min(buf_len - len);
            buf[len..len + n].copy_from_slice(&data[fed..fed + n]);
            (len, fed, add) = (len + n, fed + n, add - n);
        }
        compress(&mut e, &buf[..len], &mut out, s.flush, s.out_len);
        if s.flush != Flush::None {
            assert_eq!(e.total_in(), fed as u64);
            if format == Format::Deflate {
                // Everything so far decompresses, though the stream goes on.
                let mut d = crate::inflate::Decoder::new(format);
                let mut dbuf = vec![0; fed + 1];
                let p = d.decompress(&out, &mut dbuf, false).unwrap();
                assert_eq!(p.consumed, out.len());
                assert_eq!(p.output, &data[..fed]);
                assert_eq!(out[out.len() - 4..], [0, 0, 0xFF, 0xFF]);
            }
        }
        if fed == data.len() {
            break;
        }
    }
    let last = steps.last().unwrap().out_len;
    assert!(compress(&mut e, &buf[..len], &mut out, Flush::Finish, last));
    assert_eq!(e.total_in(), data.len() as u64);
    assert_eq!(e.total_out(), out.len() as u64);
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        option_env!("SIROCCO_PROPTEST_CASES").and_then(|s| s.parse().ok()).unwrap_or(48)
    ))]

    #[test]
    fn streams_round_trip(
        len in 0usize..400_000,
        seed in any::<u64>(),
        level in 0u8..=9,
        format_i in 0usize..3,
        steps in steps(),
        big_buf in any::<bool>(),
    ) {
        let data = sample(len, seed);
        let format = FORMATS[format_i];
        let buf_len = if big_buf { 1 << 20 } else { MIN_BUFFER_LEN };
        let c = stream(format, Level::new(level), &data, &steps, buf_len);
        prop_assert_eq!(&decompress(format, &c), &data);
        prop_assert_eq!(&decompress_miniz(format, &c), &data);
    }
}

#[test]
fn full_flush_cuts_history() {
    let data = sample(200_000, 7);
    for level in 1..=9 {
        let mut e = Encoder::new(Format::Deflate, Level::new(level));
        let mut out = vec![0; 300_000];
        let half = 100_000;
        let p = e.compress(&data[..half], &mut out, Flush::Full);
        let cut = p.output.len();
        let p = e.compress(&data, &mut out[cut..], Flush::Finish);
        assert!(p.done);
        let end = cut + p.output.len();
        // The second half decompresses alone.
        assert_eq!(decompress(Format::Deflate, &out[cut..end]), &data[half..]);
    }
}

#[test]
fn repeated_sync_flushes_add_nothing() {
    let mut e = Encoder::new(Format::Deflate, Level::DEFAULT);
    let mut out = [0; 64];
    let p = e.compress(b"abc", &mut out, Flush::Sync);
    let n = p.output.len();
    assert_eq!(out[n - 4..n], [0, 0, 0xFF, 0xFF]);
    let p = e.compress(b"abc", &mut out, Flush::Sync);
    assert_eq!(p.output.len(), 0);
    let p = e.compress(b"abc", &mut out, Flush::Full);
    assert_eq!(p.output.len(), 0);
}

#[test]
fn one_byte_outputs() {
    let data = sample(50_000, 3);
    for level in [0, 1, 4, 6, 9] {
        for format in FORMATS {
            let mut e = Encoder::new(format, Level::new(level));
            let (mut out, mut byte) = (Vec::new(), [0]);
            loop {
                let p = e.compress(&data, &mut byte, Flush::Finish);
                out.extend_from_slice(p.output);
                if p.done {
                    break;
                }
                assert!(p.full);
            }
            assert_eq!(out, crate::compress(format, Level::new(level), &data));
        }
    }
}

#[test]
fn reuse_after_done() {
    let mut e = Encoder::new(Format::Gzip, Level::DEFAULT);
    let mut out = vec![0; 1 << 16];
    for i in 0..3 {
        let data = sample(10_000 + i * 999, i as u64);
        let p = e.compress(&data, &mut out, Flush::Finish);
        assert!(p.done);
        assert_eq!(decompress(Format::Gzip, p.output), data);
    }
}

#[cfg(feature = "std")]
#[test]
fn writer_round_trips() {
    use std::io::Write;

    let data = sample(1_000_000, 11);
    for level in [0, 1, 6, 9] {
        for format in FORMATS {
            let mut w = Writer::new(format, Level::new(level), Vec::new());
            for chunk in data.chunks(77_777) {
                w.write_all(chunk).unwrap();
            }
            w.flush().unwrap();
            let c = w.finish().unwrap();
            assert_eq!(decompress(format, &c), data);
        }
    }
}

/// Guardrail: the encoder's size inline, without its tables.
#[test]
fn encoder_size() {
    let size = size_of::<Encoder>();
    assert!(size <= 6 << 10, "Encoder is {size} bytes");
}
