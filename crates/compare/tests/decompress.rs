//! Differential tests: sirocco's decompressor must agree with libdeflate's
//! on valid streams (output and consumed input) and on corrupt ones (which
//! error is returned), except that sirocco rejects the symbols RFC 1951
//! reserves, as zlib does, where libdeflate decodes them.

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
    assert_agree(&a, &b, format, input, out_len, &what);
    a
}

#[test]
fn corpus_round_trips_at_every_level() {
    let mut d = SiroccoDecoders::new();
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
        let mut d = SiroccoDecoders::new();
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
                Err(sirocco::inflate::Error::InsufficientSpace)
            );
        }
    }
}

/// A raw DEFLATE stream of one static Huffman block: `a`, 96 matches of
/// litlen symbol 285 (length 258) at offset 1, then a match of litlen
/// symbol 257 (length 3) at offset symbol `offset_sym` with 13 zero extra
/// bits, and end of block. Symbol 29 reaches back 24577 bytes, to the `a`.
fn static_block_with_offset_symbol(offset_sym: u32) -> Vec<u8> {
    let (mut out, mut bits, mut n) = (Vec::new(), 0u64, 0);
    // Writes `len` bits of `value`, LSB first, or MSB first as Huffman
    // codewords go.
    let mut put = |value: u32, len: u32, msb_first: bool| {
        let value = if msb_first {
            value.reverse_bits() >> (32 - len)
        } else {
            value
        };
        bits |= u64::from(value) << n;
        n += len;
        while n >= 8 {
            out.push(bits as u8);
            bits >>= 8;
            n -= 8;
        }
    };
    put(0b011, 3, false); // final block, static Huffman
    put(0x30 + u32::from(b'a'), 8, true);
    for _ in 0..96 {
        put(0xc5, 8, true); // litlen 285
        put(0, 5, true); // offset symbol 0
    }
    put(1, 7, true); // litlen 257
    put(offset_sym, 5, true);
    put(0, 13, false);
    put(0, 7, true); // end of block
    put(0, 7, false); // the rest of the last byte
    out
}

/// Where sirocco and libdeflate part ways: offset symbol 30 (litlen symbols
/// 286 and 287, and offset symbol 31, are alike). libdeflate decodes it as
/// 29; sirocco rejects it, as zlib and miniz_oxide do.
#[test]
fn reserved_offset_symbol_is_rejected_like_zlib() {
    let want = [&b"a"[..], &[b'a'; 96 * 258 + 3]].concat();
    let valid = static_block_with_offset_symbol(29);
    assert_eq!(
        check_same(Format::Deflate, &valid, 1 << 16),
        Outcome::Ok(valid.len(), want)
    );
    assert_eq!(compare::walk_deflate(Format::Deflate, &valid), Some(false));
    assert!(!compare::rejected_for_reserved_symbol(
        Format::Deflate,
        &valid,
        1 << 16
    ));

    // Padded, as in a git pack: at the very end of the input, an error is
    // truncation (the input may have been cut short).
    let mut c = static_block_with_offset_symbol(30);
    c.extend_from_slice(&[0; 8]);
    let mut ours = SiroccoDecoders::new();
    assert_eq!(
        sirocco_decompress(&mut ours, Format::Deflate, &c, 1 << 16),
        Outcome::BadData
    );
    assert!(matches!(
        ReferenceDecompressor::new().decompress(Format::Deflate, &c, 1 << 16),
        Outcome::Ok(..)
    ));
    assert!(compare::rejected_for_reserved_symbol(
        Format::Deflate,
        &c,
        1 << 16
    ));
    // Not if zlib fails only past the output libdeflate had room for.
    assert!(!compare::rejected_for_reserved_symbol(
        Format::Deflate,
        &c,
        1000
    ));
    assert!(miniz_oxide::inflate::decompress_to_vec(&c).is_err());
    check_same(Format::Deflate, &c, 1 << 16);
}

/// zlib gives the same error for a codeword that an incomplete code lacks,
/// here the one match of a dynamic block with an empty offset code, which
/// libdeflate and sirocco decode (as offset symbol 0). That is no reserved
/// symbol, so it mustn't excuse a difference.
#[test]
fn a_missing_codeword_is_no_reserved_symbol() {
    // HLIT 258, HDIST 1 with a zero length, then `a`, a match of length 3
    // and end of block, followed by padding.
    let mut c = vec![
        0x0d, 0xc0, 0x01, 0x09, 0x00, 0x00, 0x00, 0x80, 0xa0, 0xad, 0xfd, 0x3f, 0x91, 0xc4,
    ];
    c.extend_from_slice(&[0; 16]);
    let used = c.len() - 16;
    assert_eq!(
        check_same(Format::Deflate, &c, 100),
        Outcome::Ok(used, b"aaaa".to_vec())
    );
    assert!(!compare::rejected_for_reserved_symbol(
        Format::Deflate,
        &c,
        100
    ));
    assert_eq!(compare::walk_deflate(Format::Deflate, &c), None);
}

/// The walker behind [`compare::rejected_for_reserved_symbol`] reads valid
/// streams of every block type to their end.
#[test]
fn walker_reads_valid_streams() {
    for s in compare::synthetic_corpus() {
        for level in [0, 1, 6, 12] {
            for format in Format::ALL {
                let c = reference_compress(format, level, &s.data);
                assert_eq!(
                    compare::walk_deflate(format, &c),
                    Some(false),
                    "{} level {level} {format:?}",
                    s.name
                );
            }
        }
    }
}
