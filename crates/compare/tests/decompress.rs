//! Differential tests: sirocco's decompressor must agree with libdeflate's
//! on valid streams (output and consumed input) and on corrupt ones (which
//! error is returned).

use compare::{reference_compress, sirocco_decompress, Format, Outcome, ReferenceDecompressor};
use proptest::prelude::*;

fn check_same(format: Format, input: &[u8], out_len: usize) -> Outcome {
    let mut ours = sirocco::Decompressor::new(sirocco::Format::Deflate);
    let mut theirs = ReferenceDecompressor::new();
    let a = sirocco_decompress(&mut ours, format, input, out_len);
    let b = theirs.decompress(format, input, out_len);
    assert_eq!(
        a,
        b,
        "{format:?}, {} input bytes, out_len {out_len}",
        input.len()
    );
    a
}

#[test]
fn corpus_round_trips_at_every_level() {
    let mut d = sirocco::Decompressor::new(sirocco::Format::Deflate);
    for s in compare::corpus() {
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
fn truncated_streams_match_libdeflate() {
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
    fn skewed_literals_match_libdeflate(
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
    fn output_end_near_matches_libdeflate(
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
        let mut d = sirocco::Decompressor::new(sirocco::Format::Deflate);
        prop_assert_eq!(
            sirocco_decompress(&mut d, format, &c, data.len()),
            Outcome::Ok(c.len(), data)
        );
    }

    /// Corrupts a valid stream and checks both decompressors agree on the
    /// outcome, which exercises error paths far more than random bytes do.
    #[test]
    fn corrupted_streams_match_libdeflate(
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
    fn random_bytes_match_libdeflate(
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
                Err(sirocco::DecompressError::InsufficientSpace)
            );
        }
    }
}
