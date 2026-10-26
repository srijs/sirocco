//! Differential tests: sirocco's decompressor must agree with zlib-rs's
//! on valid streams (output and consumed input) and on corrupt ones (which
//! error is returned).

use compare::{
    assert_agree, reference_compress, sirocco_decompress, Format, Outcome, ReferenceDecompressor,
    SiroccoDecoders,
};
use proptest::prelude::*;

fn check_same(format: Format, input: &[u8], out_len: usize) -> Outcome {
    let mut ours = SiroccoDecoders::new();
    let mut theirs = ReferenceDecompressor::new();
    let a = sirocco_decompress(&mut ours, format, input, out_len);
    let b = theirs.decompress(format, input, out_len);
    let what = format!("{format:?}, {} input bytes, out_len {out_len}", input.len());
    assert_agree(&a, &b, &what);
    a
}

#[test]
fn corpus_round_trips_at_every_level() {
    let mut d = SiroccoDecoders::new();
    for s in compare::test_corpus() {
        for level in 0..=12 {
            for format in Format::ALL {
                let c = reference_compress(format, level, &s.data);
                let what = format!("{} level {level} {format:?}", s.name);

                // Exact-size output buffer.
                let r = sirocco_decompress(&mut d, format, &c, s.data.len());
                assert_eq!(r, Outcome::Ok(c.len(), s.data.clone()), "{what}");

                // Larger buffer, trailing bytes after the stream.
                let mut padded = c.clone();
                padded.extend_from_slice(&[0xA5; 37]);
                let r = sirocco_decompress(&mut d, format, &padded, s.data.len() + 1000);
                assert_eq!(r, Outcome::Ok(c.len(), s.data.clone()), "{what} padded");

                // One byte too small.
                if !s.data.is_empty() {
                    let r = sirocco_decompress(&mut d, format, &c, s.data.len() - 1);
                    assert_eq!(r, Outcome::InsufficientSpace, "{what} short");
                }
            }
        }
    }
}

#[test]
fn truncated_streams_match_zlib() {
    let data = &compare::synthetic_corpus()[2].data; // text, 4 KiB
    for level in [0, 1, 6, 12] {
        for format in Format::ALL {
            let c = reference_compress(format, level, data);
            for len in 0..c.len() {
                check_same(format, &c[..len], data.len());
            }
        }
    }
}

#[test]
fn empty_input() {
    for format in Format::ALL {
        for level in [0, 1, 12] {
            let c = reference_compress(format, level, b"");
            assert_eq!(check_same(format, &c, 0), Outcome::Ok(c.len(), vec![]));
        }
    }
}

/// Data with a mix of literals, short-offset matches and long runs, so that
/// all of the match copy paths get used.
fn mixed_data() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop_oneof![
            prop::collection::vec(any::<u8>(), 0..64),
            (any::<u8>(), 1usize..600).prop_map(|(b, n)| vec![b; n]),
            (prop::collection::vec(any::<u8>(), 1..9), 1usize..80).prop_map(|(p, n)| p.repeat(n)),
            prop::collection::vec(prop::sample::select(b"abcde".to_vec()), 0..300),
        ],
        0..40,
    )
    .prop_map(|chunks| chunks.concat())
}

/// Literal-heavy data over a skewed alphabet, whose short literal codewords
/// get decoded two at a time.
fn skewed_literals() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop_oneof![
            8 => Just(b'e'),
            4 => Just(b' '),
            2 => prop::sample::select(b"tao".to_vec()),
            1 => any::<u8>(),
        ],
        0..5000,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Output buffers around the exact size, so that the end of the output
    /// can fall between the two literals of a double entry.
    #[test]
    fn skewed_literals_match_zlib(
        data in skewed_literals(),
        level in 0i32..=12,
        f in 0usize..3,
        out_delta in -40isize..4,
    ) {
        let format = Format::ALL[f];
        let c = reference_compress(format, level, &data);
        let out_len = data.len().saturating_add_signed(out_delta);
        let r = check_same(format, &c, out_len);
        if out_len >= data.len() {
            prop_assert_eq!(r, Outcome::Ok(c.len(), data));
        }
    }

    /// Streams followed by more bytes (as in a git pack), so that the input
    /// is never what stops the fastloop, into output buffers around the
    /// exact size: the end of the output falls inside or just after matches
    /// that the tail fastloop has to stop before or copy without overrun.
    #[test]
    fn output_end_near_matches_zlib(
        data in mixed_data(),
        level in 0i32..=12,
        f in 0usize..3,
        out_delta in -320isize..4,
        pad in 0usize..64,
    ) {
        let format = Format::ALL[f];
        let mut c = reference_compress(format, level, &data);
        let len = c.len();
        c.resize(len + pad, 0xA5);
        let out_len = data.len().saturating_add_signed(out_delta);
        let r = check_same(format, &c, out_len);
        if out_len >= data.len() {
            prop_assert_eq!(r, Outcome::Ok(len, data));
        }
    }

    #[test]
    fn round_trips(data in mixed_data(), level in 0i32..=12, f in 0usize..3) {
        let format = Format::ALL[f];
        let c = reference_compress(format, level, &data);
        let mut d = SiroccoDecoders::new();
        prop_assert_eq!(
            sirocco_decompress(&mut d, format, &c, data.len()),
            Outcome::Ok(c.len(), data)
        );
    }

    /// Corrupts a valid stream and checks both decompressors agree on the
    /// outcome, which exercises error paths far more than random bytes do.
    #[test]
    fn corrupted_streams_match_zlib(
        data in mixed_data(),
        level in 0i32..=12,
        f in 0usize..3,
        flips in prop::collection::vec((any::<prop::sample::Index>(), 0u8..8), 1..4),
        extra_out in 0usize..300,
    ) {
        let format = Format::ALL[f];
        let mut c = reference_compress(format, level, &data);
        for (i, bit) in flips {
            let i = i.index(c.len());
            c[i] ^= 1 << bit;
        }
        check_same(format, &c, data.len() + extra_out);
    }

    #[test]
    fn random_bytes_match_zlib(
        input in prop::collection::vec(any::<u8>(), 0..2000),
        out_len in 0usize..70_000,
        f in 0usize..3,
    ) {
        check_same(Format::ALL[f], &input, out_len);
    }

    /// The `Vec` path grows its buffer to fit, stops at the limit, and reads
    /// every member of a multi-member gzip file.
    #[test]
    fn vec_decompress_round_trips(
        parts in prop::collection::vec(mixed_data(), 1..4),
        level in 0i32..=12,
        f in 0usize..3,
    ) {
        let format = Format::ALL[f];
        // Only gzip allows more than one stream back to back.
        let parts = if format == Format::Gzip { &parts[..] } else { &parts[..1] };
        let c: Vec<u8> = parts.iter().flat_map(|p| reference_compress(format, level, p)).collect();
        let want = parts.concat();
        prop_assert_eq!(sirocco::decompress(format.ff(), &c, want.len()), Ok(want.clone()));
        if !want.is_empty() {
            prop_assert_eq!(
                sirocco::decompress(format.ff(), &c, want.len() - 1),
                Err(sirocco::inflate::Error::InsufficientSpace)
            );
        }
    }
}

/// Corrupts fixed Huffman streams, whose codes have codewords for the
/// symbols RFC 1951 reserves, which sirocco must reject as zlib does. Bit
/// flips reach them rarely, so this runs many more cases than the
/// proptests, deterministically.
#[test]
fn corrupted_fixed_streams_match_zlib() {
    let mut rng = compare::Rng(0x9e37_79b9_7f4a_7c15);
    let (mut ours, mut theirs) = (SiroccoDecoders::new(), ReferenceDecompressor::new());
    for _ in 0..5_000 {
        let len = 1 + (rng.next() % 3000) as usize;
        let data: Vec<u8> = (0..len)
            .map(|i| {
                if rng.next().is_multiple_of(4) {
                    rng.next() as u8
                } else {
                    (i % 7) as u8
                }
            })
            .collect();
        let c = compare::fixed_compress(&data);
        for _ in 0..4 {
            let mut c = c.clone();
            for _ in 0..=rng.next() % 3 {
                let bit = rng.next() as usize % (8 * c.len());
                c[bit / 8] ^= 1 << (bit % 8);
            }
            c.extend_from_slice(&[0; 16]);
            let (out_len, f) = (len + 16, Format::Deflate);
            assert_agree(
                &sirocco_decompress(&mut ours, f, &c, out_len),
                &theirs.decompress(f, &c, out_len),
                &format!("{} input bytes", c.len()),
            );
        }
    }
}
