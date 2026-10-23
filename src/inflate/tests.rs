//! Tests of [`Decoder`], [`Scanner`], [`Reader`](super::Reader) and the
//! `Vec` decoder.

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use proptest::prelude::*;

use super::decode::PRECODE_LENS_PERMUTATION;
use super::tables::{NUM_LITLEN_SYMS, STATIC_LENS};
use super::*;
use crate::Format;

const FORMATS: [Format; 3] = [Format::Deflate, Format::Zlib, Format::Gzip];

/// Compresses with miniz_oxide (level capped at 10), keeping C out of this
/// crate's builds; `crates/compare` tests against zlib-rs.
fn reference_compress(format: Format, level: i32, data: &[u8]) -> Vec<u8> {
    use miniz_oxide::deflate::{compress_to_vec, compress_to_vec_zlib};
    let level = level.min(10) as u8;
    match format {
        Format::Deflate => compress_to_vec(data, level),
        Format::Zlib => compress_to_vec_zlib(data, level),
        Format::Gzip => {
            let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
            out.extend(compress_to_vec(data, level));
            out.extend(crate::checksum::crc32(0, data).to_le_bytes());
            out.extend((data.len() as u32).to_le_bytes());
            out
        }
    }
}

/// Decodes `c` in one call into `len` bytes: the output (`None` if the
/// buffer filled up) and the input used.
fn one_shot(format: Format, c: &[u8], len: usize) -> Result<(Option<Vec<u8>>, usize), Error> {
    let mut buf = vec![0; len];
    let p = Decoder::new(format).decompress(c, &mut buf, true)?;
    let (done, consumed, out) = (p.done, p.consumed, p.output.to_vec());
    Ok((done.then_some(out), consumed))
}

/// Decodes `c` in pieces of the `in_sizes` (cycled) through one `buf_len`
/// buffer: the output and the input used.
fn stream(
    format: Format,
    c: &[u8],
    in_sizes: &[usize],
    buf_len: usize,
) -> Result<(Vec<u8>, usize), Error> {
    let mut d = Decoder::new(format);
    let mut buf = vec![0; buf_len];
    let (mut pos, mut out, mut stuck) = (0, Vec::new(), 0);
    for i in 0.. {
        let n = in_sizes[i % in_sizes.len()].min(c.len() - pos);
        let end = pos + n == c.len();
        let p = d.decompress(&c[pos..pos + n], &mut buf, end)?;
        let (consumed, written, done, full) = (p.consumed, p.output.len(), p.done, p.full);
        out.extend_from_slice(p.output);
        assert!(consumed <= n);
        pos += consumed;
        assert_eq!(d.total_in(), pos as u64);
        assert_eq!(d.total_out(), out.len() as u64);
        if done {
            assert!(!full);
            return Ok((out, pos));
        }
        assert!(!full || d.core.pos() == buf_len);
        // All input is used unless the buffer filled up.
        assert!(consumed == n || full, "{consumed} of {n}, buffer not full");
        if end {
            assert!(full, "not done at the end of the input");
        }
        if full {
            assert!(d.make_room(&mut buf) > 0);
        }
        if consumed == 0 && written == 0 {
            stuck += 1;
            assert!(stuck < 100, "no progress at {pos} of {}", c.len());
        } else {
            stuck = 0;
        }
    }
    unreachable!()
}

/// Scans `c` in pieces of the `in_sizes` (cycled): the output length and the
/// input used.
fn scan(format: Format, c: &[u8], in_sizes: &[usize]) -> Result<(u64, usize), Error> {
    let mut s = Scanner::new(format);
    let mut pos = 0;
    for i in 0.. {
        let n = in_sizes[i % in_sizes.len()].min(c.len() - pos);
        let end = pos + n == c.len();
        if let Some(used) = s.scan(&c[pos..pos + n], end)? {
            assert!(used <= n);
            pos += used;
            assert_eq!(s.total_in(), pos as u64);
            return Ok((s.total_out(), pos));
        }
        pos += n;
        assert_eq!(s.total_in(), pos as u64);
        assert!(!end, "not done at the end of the input");
    }
    unreachable!()
}

/// Proptest cases per test: 256, or `SIROCCO_PROPTEST_CASES` at build time
/// (CI lowers it for emulated targets).
fn cases() -> u32 {
    match option_env!("SIROCCO_PROPTEST_CASES") {
        Some(n) => n.parse().expect("SIROCCO_PROPTEST_CASES is a number"),
        None => 256,
    }
}

fn data() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..2000),
        // Compressible: short alphabet, long enough for several blocks and
        // matches that reach back across chunk boundaries.
        prop::collection::vec(0u8..4, 0..100_000),
        (any::<u8>(), 0usize..100_000).prop_map(|(b, n)| vec![b; n]),
        // Incompressible runs (stored blocks) each followed by a copy of
        // their end, which Huffman blocks refer back into.
        prop::collection::vec(
            (
                prop::collection::vec(any::<u8>(), 0..80_000),
                0usize..33_000
            ),
            1..4
        )
        .prop_map(|runs| {
            let mut v = Vec::new();
            for (run, copy) in runs {
                v.extend_from_slice(&run);
                let start = v.len() - copy.min(run.len());
                v.extend_from_within(start..);
            }
            v
        }),
        // Many times the smallest streaming buffer.
        (any::<u64>(), 0usize..1_500_000).prop_map(|(mut x, n)| {
            (0..n)
                .map(|_| {
                    x = x
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    b"aaaaabbbccd \n"[(x >> 60) as usize % 13]
                })
                .collect()
        }),
    ]
}

/// Smaller data, for the many-case tests.
fn small_data() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop_oneof![
            prop::collection::vec(any::<u8>(), 0..2000),
            (any::<u8>(), 1usize..3000).prop_map(|(b, n)| vec![b; n]),
            (prop::collection::vec(any::<u8>(), 1..9), 1usize..300).prop_map(|(p, n)| p.repeat(n)),
        ],
        0..20,
    )
    .prop_map(|chunks| chunks.concat())
}

fn in_sizes() -> impl Strategy<Value = Vec<usize>> {
    prop::collection::vec(
        prop_oneof![
            0usize..4,
            1usize..300,
            300usize..70_000,
            Just(usize::MAX >> 1)
        ],
        1..6,
    )
    .prop_filter("some input", |v| v.iter().any(|&n| n > 0))
}

fn buf_len() -> impl Strategy<Value = usize> {
    prop_oneof![
        // The least that can make room: just over the 32 KiB window.
        Just((32usize << 10) + 1),
        (32usize << 10)..(64 << 10) + 70_000,
        Just(1 << 20),
    ]
}

#[test]
fn one_shot_decodes_into_an_exact_buffer() {
    let data: Vec<u8> = (0..300_000u32)
        .map(|i| (i % 251) as u8 ^ (i / 1000) as u8)
        .collect();
    for format in FORMATS {
        let mut c = reference_compress(format, 6, &data);
        let n = c.len();
        c.extend_from_slice(b"next");
        assert_eq!(
            one_shot(format, &c, data.len()),
            Ok((Some(data.clone()), n)),
            "{format:?}"
        );
        // One byte short: filled with all but the last byte.
        let mut buf = vec![0; data.len() - 1];
        let p = Decoder::new(format).decompress(&c, &mut buf, true).unwrap();
        assert!(!p.done);
        assert!(p.output == &data[..data.len() - 1], "{format:?}");
    }
}

#[test]
fn a_short_buffer_is_filled_completely() {
    // Long matches, so most buffer ends split one.
    let data = b"0123456789".repeat(1000);
    for format in FORMATS {
        let c = reference_compress(format, 9, &data);
        for len in (0..300).chain([999, 1000, 1001, 9999, 10_000]) {
            let mut buf = vec![0; len];
            let p = Decoder::new(format).decompress(&c, &mut buf, true).unwrap();
            let n = len.min(data.len());
            assert_eq!(p.output, &data[..n], "{format:?} {len}");
            assert_eq!(p.done, len >= data.len(), "{format:?} {len}");
        }
    }
}

#[test]
fn a_full_buffer_needs_room() {
    let data = b"0123456789".repeat(10_000);
    let c = reference_compress(Format::Zlib, 9, &data);
    for len in [1000, 40_000] {
        let mut d = Decoder::new(Format::Zlib);
        let mut buf = vec![0; len];
        let p = d.decompress(&c, &mut buf, true).unwrap();
        assert!(!p.done && p.full);
        let rest = &c[p.consumed..];
        // Calling again without making room is an error, which changes
        // nothing.
        assert_eq!(d.decompress(rest, &mut buf, true), Err(Error::BufferFull));
        assert_eq!(d.decompress(rest, &mut buf, true), Err(Error::BufferFull));
        // Up to 32 KiB of output there is no room to make.
        assert_eq!(d.make_room(&mut buf), len.saturating_sub(32 << 10));
        if len > 32 << 10 {
            let p = d.decompress(rest, &mut buf, true).unwrap();
            assert_eq!(p.output, &data[len..2 * len - (32 << 10)]);
            continue;
        }
        // The stream goes on in a bigger buffer holding the same output.
        let mut big = buf.clone();
        big.resize(data.len(), 0);
        let p = d.decompress(rest, &mut big, true).unwrap();
        assert!(p.done && !p.full);
        assert_eq!(big, data);
    }
}

#[test]
fn the_buffer_can_move_and_grow() {
    let data: Vec<u8> = (0..300_000u32)
        .map(|i| (i % 251) as u8 ^ (i / 1000) as u8)
        .collect();
    for format in FORMATS {
        let c = reference_compress(format, 6, &data);
        for piece in [7, 1000, usize::MAX] {
            let mut d = Decoder::new(format);
            let (mut out, mut len, mut pos) = (vec![0; 3], 0, 0usize);
            loop {
                let end = pos.saturating_add(piece).min(c.len());
                let p = d
                    .decompress(&c[pos..end], &mut out, end == c.len())
                    .unwrap();
                len += p.output.len();
                pos += p.consumed;
                if p.done {
                    break;
                }
                if p.full {
                    // A new allocation, so it moves too.
                    let mut bigger = vec![0; out.len() * 2];
                    bigger[..len].copy_from_slice(&out[..len]);
                    out = bigger;
                }
            }
            out.truncate(len);
            assert!(out == data, "{format:?} pieces of {piece}");
            assert_eq!(pos, c.len());
        }
    }
}

#[test]
fn a_buffer_shorter_than_its_output_is_rejected() {
    let data = b"0123456789".repeat(10_000);
    let c = reference_compress(Format::Deflate, 9, &data);
    let mut d = Decoder::new(Format::Deflate);
    let mut buf = vec![0; 1000];
    let p = d.decompress(&c[..100], &mut buf, false).unwrap();
    let n = p.output.len();
    assert!(n > 0 && !p.done);
    assert_eq!(
        d.decompress(&c[100..], &mut buf[..n - 1], false),
        Err(Error::BufferFull)
    );
}

#[test]
fn a_buffer_can_change_before_there_is_output() {
    let c = reference_compress(Format::Gzip, 6, b"hello hello hello");
    let mut d = Decoder::new(Format::Gzip);
    let mut first = [0; 100];
    // Only the gzip header: no output yet.
    let p = d.decompress(&c[..5], &mut first, false).unwrap();
    assert!(!p.done && p.output.is_empty() && p.consumed == 5);
    let mut second = [0; 100];
    let p = d.decompress(&c[5..], &mut second, true).unwrap();
    assert!(p.done);
    assert_eq!(p.output, b"hello hello hello");
}

#[test]
fn concatenated_streams_start_by_themselves() {
    for format in FORMATS {
        let parts: [&[u8]; 3] = [b"first stream", b"", &[7; 5000]];
        let mut c = Vec::new();
        for p in parts {
            c.extend(reference_compress(format, 6, p));
        }
        c.extend_from_slice(b"trailer");
        let mut d = Decoder::new(format);
        let mut s = Scanner::new(format);
        let mut buf = vec![0; 10_000];
        let mut rest = &c[..];
        for part in parts {
            let p = d.decompress(rest, &mut buf, true).unwrap();
            assert!(p.done && p.output == part, "{format:?}");
            assert_eq!(s.scan(rest, true), Ok(Some(p.consumed)), "{format:?}");
            assert_eq!(s.total_out(), part.len() as u64);
            rest = &rest[p.consumed..];
        }
        assert_eq!(rest, b"trailer");
    }
}

/// Found by fuzzing: a zlib stream too short for a trailer is truncated
/// (libdeflate: bad data). Without room, it is out of room first, as zlib.
#[test]
fn input_too_short_for_a_trailer_is_truncated() {
    let input = [0x08, 0x5b, 0x9a, 0x08, 0xdb];
    let mut buf = [0; 64];
    let r = Decoder::new(Format::Zlib).decompress(&input, &mut buf, true);
    assert_eq!(r, Err(Error::Truncated));
    let p = Decoder::new(Format::Zlib)
        .decompress(&input, &mut [], true)
        .unwrap();
    assert!(p.full && !p.done);
}

#[test]
fn a_preset_dictionary_is_its_own_error() {
    // A valid zlib header with FDICT set, then a dictionary id.
    let input = [0x78, 0x20, 0, 0, 0, 0, 0x03, 0x00, 0, 0, 0, 1];
    assert_eq!(
        one_shot(Format::Zlib, &input, 10),
        Err(Error::PresetDictionary)
    );
    assert_eq!(
        stream(Format::Zlib, &input, &[1], 1 << 16),
        Err(Error::PresetDictionary)
    );
    assert_eq!(
        scan(Format::Zlib, &input, &[1]),
        Err(Error::PresetDictionary)
    );
    assert_eq!(
        crate::decompress(Format::Zlib, &input, 10),
        Err(Error::PresetDictionary)
    );
    // The same bit in a header that is wrong anyway is just bad data.
    assert_eq!(
        one_shot(Format::Zlib, b"invalid zlib", 10),
        Err(Error::BadData)
    );
}

#[test]
fn an_error_ends_the_stream() {
    let good = reference_compress(Format::Zlib, 6, b"good data");
    let mut bad = good.clone();
    bad[0] ^= 0xff;
    let mut d = Decoder::new(Format::Zlib);
    let mut buf = [0; 100];
    assert_eq!(d.decompress(&bad, &mut buf, true), Err(Error::BadData));
    let p = d.decompress(&good, &mut buf, true).unwrap();
    assert!(p.done);
    assert_eq!(p.output, b"good data");
}

#[test]
fn truncated_input_is_reported_as_such() {
    let data: Vec<u8> = (0..100_000u32).map(|i| (i * 7 % 253) as u8).collect();
    for format in FORMATS {
        for level in [0, 1, 9] {
            let c = reference_compress(format, level, &data);
            for cut in [1, c.len() / 3, c.len() - 1] {
                let mut buf = vec![0; data.len()];
                let r = Decoder::new(format).decompress(&c[..cut], &mut buf, true);
                assert_eq!(
                    r.err(),
                    Some(Error::Truncated),
                    "{format:?} level {level} cut {cut}"
                );
                let r = Scanner::new(format).scan(&c[..cut], true);
                assert_eq!(
                    r,
                    Err(Error::Truncated),
                    "{format:?} level {level} cut {cut}"
                );
            }
        }
    }
}

/// Writes bits LSB first, as DEFLATE packs them.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    bits: u64,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, n: u32) {
        self.bits |= u64::from(value) << self.n;
        self.n += n;
        while self.n >= 8 {
            self.out.push(self.bits as u8);
            self.bits >>= 8;
            self.n -= 8;
        }
    }

    /// Writes a `len`-bit Huffman codeword, MSB first.
    fn put_code(&mut self, code: u32, len: u32) {
        self.put(code.reverse_bits() >> (32 - len), len);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.bits as u8);
        }
        self.out
    }
}

/// Canonical Huffman codewords for `lens`.
fn canonical_codes(lens: &[u8]) -> Vec<u32> {
    let mut count = [0u32; 16];
    for &l in lens {
        count[l as usize] += 1;
    }
    count[0] = 0;
    let mut next = [0u32; 16];
    let mut code = 0;
    for len in 1..16 {
        code = (code + count[len - 1]) << 1;
        next[len] = code;
    }
    lens.iter()
        .map(|&l| {
            let c = next[l as usize];
            next[l as usize] += 1;
            c
        })
        .collect()
}

/// A complete code over `n` symbols (from `2^len` to `2^(len + 1)`) of
/// `len`- and `len + 1`-bit codewords.
fn complete_lens(n: usize, len: u8) -> Vec<u8> {
    let mut lens = vec![len; n];
    lens[(2 << len) - n..].fill(len + 1);
    lens
}

/// A codeword in [`dynamic_block_stream`]: of a litlen symbol, or raw.
#[derive(Clone, Copy)]
enum Codeword {
    Litlen(usize),
    Raw(u32, u32),
}

/// A raw DEFLATE stream of one dynamic Huffman block with the precode
/// codeword lengths `precode_lens` (by symbol, which mustn't include the
/// repeat symbols), the codeword lengths `litlen_lens` and `offset_lens`,
/// and the codewords `body`.
fn dynamic_block_stream(
    precode_lens: &[u8; 19],
    litlen_lens: &[u8],
    offset_lens: &[u8],
    body: &[Codeword],
) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.put(1, 1); // final block
    w.put(2, 2); // dynamic Huffman
    w.put((litlen_lens.len() - 257) as u32, 5);
    w.put((offset_lens.len() - 1) as u32, 5);
    w.put(19 - 4, 4);
    for sym in PRECODE_LENS_PERMUTATION {
        w.put(u32::from(precode_lens[usize::from(sym)]), 3);
    }
    let precode = canonical_codes(precode_lens);
    for &len in litlen_lens.iter().chain(offset_lens) {
        let len = usize::from(len);
        w.put_code(precode[len], u32::from(precode_lens[len]));
    }
    let litlen = canonical_codes(litlen_lens);
    for &codeword in body {
        match codeword {
            Codeword::Litlen(sym) => w.put_code(litlen[sym], u32::from(litlen_lens[sym])),
            Codeword::Raw(code, len) => w.put_code(code, len),
        }
    }
    w.finish()
}

/// A raw DEFLATE stream decoding to "ab" in one dynamic block, whose header
/// declares `num_litlen` litlen and `num_offset` offset codeword lengths,
/// all as 7-bit precode codewords. With the most valid symbols (286 and 30)
/// it has the longest possible header: 286 bytes, with 316 lengths.
fn dynamic_header_stream(num_litlen: usize, num_offset: usize) -> Vec<u8> {
    // A complete precode with 7-bit codewords for lengths 4, 5, 8 and 9.
    let mut precode_lens = [0u8; 19];
    for (sym, len) in [
        (0, 1),
        (1, 2),
        (2, 3),
        (3, 4),
        (6, 5),
        (4, 7),
        (5, 7),
        (8, 7),
        (9, 7),
    ] {
        precode_lens[sym] = len;
    }
    // Complete codes: litlen of 8 and 9 bits, offset of 4 and 5.
    let body = [usize::from(b'a'), usize::from(b'b'), 256].map(Codeword::Litlen);
    let litlen_lens = complete_lens(num_litlen, 8);
    let offset_lens = complete_lens(num_offset, 4);
    dynamic_block_stream(&precode_lens, &litlen_lens, &offset_lens, &body)
}

#[test]
fn carry_holds_the_longest_block_header() {
    let c = dynamic_header_stream(286, 30);
    // The 2286-bit header, then `a`, `b` and end of block: 8, 8 and 9 bits.
    assert_eq!(c.len(), (2286usize + 25).div_ceil(8));
    assert_eq!(
        one_shot(Format::Deflate, &c, 10),
        Ok((Some(b"ab".to_vec()), c.len()))
    );
    // A byte at a time, so the whole header is carried.
    for sizes in [&[1][..], &[2, 3], &[285], &[286], &[100, 250]] {
        assert_eq!(
            stream(Format::Deflate, &c, sizes, 1 << 16),
            Ok((b"ab".to_vec(), c.len()))
        );
        assert_eq!(scan(Format::Deflate, &c, sizes), Ok((2, c.len())));
    }
}

/// `c` followed by bytes, as in a git pack, so that errors near the end of
/// the stream aren't truncation.
fn padded(c: &[u8]) -> Vec<u8> {
    [c, &[0; 16]].concat()
}

/// Checks that every decoder rejects the raw DEFLATE stream `c` (padded) as
/// bad data: in one call into `out_lens` byte buffers, in pieces, scanning
/// and into a `Vec`.
fn assert_rejected(c: &[u8], out_lens: &[usize], what: &str) {
    let c = &padded(c);
    for &len in out_lens {
        let r = one_shot(Format::Deflate, c, len);
        assert_eq!(r, Err(Error::BadData), "{what}: one-shot into {len}");
    }
    for sizes in [&[1][..], &[7, 300], &[usize::MAX >> 1]] {
        let r = stream(Format::Deflate, c, sizes, 1 << 16);
        assert_eq!(r, Err(Error::BadData), "{what}: stream in {sizes:?}");
        let r = scan(Format::Deflate, c, sizes);
        assert_eq!(r, Err(Error::BadData), "{what}: scan in {sizes:?}");
    }
    let r = crate::decompress(Format::Deflate, c, 1 << 20);
    assert_eq!(r, Err(Error::BadData), "{what}: Vec");
}

/// Checks that every decoder decodes the raw DEFLATE stream `c` (padded) to
/// `out`.
fn assert_decodes(c: &[u8], out: &[u8], what: &str) {
    let (used, c) = (c.len(), &padded(c));
    let r = one_shot(Format::Deflate, c, out.len());
    assert_eq!(r, Ok((Some(out.to_vec()), used)), "{what}: one-shot");
    for sizes in [&[1][..], &[usize::MAX >> 1]] {
        let r = stream(Format::Deflate, c, sizes, 1 << 16);
        assert_eq!(r, Ok((out.to_vec(), used)), "{what}: stream in {sizes:?}");
        let r = scan(Format::Deflate, c, sizes);
        assert_eq!(r, Ok((out.len() as u64, used)), "{what}: scan in {sizes:?}");
    }
}

/// RFC 1951 has 286 litlen and 30 offset symbols; a dynamic block header
/// that declares more is invalid (see the README's "Strictness"), although
/// they have codewords in complete codes.
#[test]
fn a_header_with_too_many_symbols_is_rejected() {
    for (num_litlen, num_offset) in [(287, 30), (288, 30), (286, 31), (286, 32), (288, 32)] {
        let c = dynamic_header_stream(num_litlen, num_offset);
        let what = format!("HLIT {num_litlen} HDIST {num_offset}");
        assert_rejected(&c, &[2, 1000], &what);
    }
}

/// The output of [`fixed_block_stream`] before its match.
const FIXED_PREFIX_LEN: usize = 1 + 96 * 258;

/// A raw DEFLATE stream of one fixed Huffman block: `a`, 96 matches of
/// length 258 at offset 1 (so that 24577, the furthest offset of symbol 29,
/// is in reach), a match of litlen symbol `len_sym` and offset symbol
/// `offset_sym` with zero extra bits, `tail` literal `z`s and end of block.
fn fixed_block_stream(len_sym: usize, offset_sym: u32, tail: usize) -> Vec<u8> {
    let lens = &STATIC_LENS[..NUM_LITLEN_SYMS];
    let codes = canonical_codes(lens);
    let mut w = BitWriter::default();
    let litlen = |w: &mut BitWriter, sym: usize| w.put_code(codes[sym], u32::from(lens[sym]));
    w.put(1, 1); // final block
    w.put(1, 2); // fixed Huffman
    litlen(&mut w, usize::from(b'a'));
    for _ in 0..96 {
        litlen(&mut w, 285);
        w.put_code(0, 5);
    }
    litlen(&mut w, len_sym);
    w.put_code(offset_sym, 5);
    if (4..30).contains(&offset_sym) {
        w.put(0, (offset_sym - 2) / 2);
    }
    for _ in 0..tail {
        litlen(&mut w, usize::from(b'z'));
    }
    litlen(&mut w, 256);
    w.finish()
}

/// Litlen symbols 286 and 287 and offset symbols 30 and 31 are invalid (see
/// the README's "Strictness"), and only the fixed codes have codewords for
/// them. Their valid neighbors decode.
#[test]
fn reserved_fixed_symbols_are_rejected() {
    // Long tails get to the symbols in the fastloop, short ones (or a short
    // output buffer) in the tail fastloop or the generic loop, also with no
    // room left for the match.
    for tail in [0, 600] {
        for (len_sym, offset_sym) in [(286, 0), (287, 0), (257, 30), (257, 31)] {
            let c = fixed_block_stream(len_sym, offset_sym, tail);
            let what = format!("litlen {len_sym}, offset {offset_sym}, tail {tail}");
            let out_lens = [FIXED_PREFIX_LEN, FIXED_PREFIX_LEN + 50, 1 << 20];
            assert_rejected(&c, &out_lens, &what);
        }
        for (len_sym, offset_sym, len) in [(285, 0, 258), (257, 29, 3)] {
            let c = fixed_block_stream(len_sym, offset_sym, tail);
            let out = [vec![b'a'; FIXED_PREFIX_LEN + len], vec![b'z'; tail]].concat();
            let what = format!("litlen {len_sym}, offset {offset_sym}, tail {tail}");
            assert_decodes(&c, &out, &what);
        }
    }
}

/// An incomplete code (empty, or of a single 1-bit codeword) is valid, but
/// the codewords it lacks aren't (see the README's "Strictness").
#[test]
fn missing_codewords_are_rejected() {
    use Codeword::{Litlen, Raw};
    // Lengths 0 to 15 as 4-bit precode codewords.
    let mut precode = [4; 19];
    precode[16..].fill(0);
    let block = |litlen: &[u8], offset: &[u8], body: &[Codeword]| {
        dynamic_block_stream(&precode, litlen, offset, body)
    };
    // `a`, 256 and 257 as a complete code.
    let mut lens = vec![0; 258];
    (lens[usize::from(b'a')], lens[256], lens[257]) = (1, 2, 2);
    let a = Litlen(usize::from(b'a'));
    // An empty offset code, used or not.
    assert_decodes(&block(&lens, &[0], &[a, Litlen(256)]), b"a", "empty unused");
    let c = block(&lens, &[0], &[a, Litlen(257), Raw(0, 1), Litlen(256)]);
    assert_rejected(&c, &[100], "empty offset code");
    // One 1-bit offset codeword, for offset 1.
    let c = block(&lens, &[1], &[a, Litlen(257), Raw(0, 1), Litlen(256)]);
    assert_decodes(&c, b"aaaa", "single offset codeword");
    let c = block(&lens, &[1], &[a, Litlen(257), Raw(1, 1), Litlen(256)]);
    assert_rejected(&c, &[100], "missing offset codeword");
    // One 1-bit offset codeword, for offset symbol 1 (offset 2).
    let c = block(&lens, &[0, 1], &[a, a, Litlen(257), Raw(0, 1), Litlen(256)]);
    assert_decodes(&c, b"aaaaa", "single offset codeword of symbol 1");
    let c = block(&lens, &[0, 1], &[a, a, Litlen(257), Raw(1, 1), Litlen(256)]);
    assert_rejected(&c, &[100], "missing offset codeword of symbol 1");
    // One 1-bit litlen codeword: end of block.
    let mut lens = vec![0; 257];
    lens[256] = 1;
    assert_decodes(
        &block(&lens, &[0], &[Raw(0, 1)]),
        b"",
        "single litlen codeword",
    );
    let c = block(&lens, &[0], &[Raw(1, 1), Raw(0, 1)]);
    assert_rejected(&c, &[100], "missing litlen codeword");
}

/// A litlen code without end of block makes a block that can't end. zlib
/// rejects it in the header, before any output.
#[test]
fn a_litlen_code_without_end_of_block_is_rejected() {
    let mut precode = [4; 19];
    precode[16..].fill(0);
    let mut lens = vec![0; 257];
    (lens[usize::from(b'a')], lens[usize::from(b'b')]) = (1, 1);
    let c = dynamic_block_stream(&precode, &lens, &[0], &[Codeword::Raw(0, 1); 64]);
    assert_rejected(&c, &[0, 1, 1000], "no end of block");
}

/// zlib rejects an incomplete precode, even with a single 1-bit codeword.
#[test]
fn an_incomplete_precode_is_rejected() {
    // Only precode symbol 18 (11 - 138 zeros), as codeword `0`.
    let mut w = BitWriter::default();
    w.put(1, 1); // final block
    w.put(2, 2); // dynamic Huffman
    w.put(0, 5 + 5 + 4); // 257 litlen, 1 offset and 4 precode lengths
    for len in [0, 0, 1, 0] {
        w.put(len, 3);
    }
    assert_rejected(&w.finish(), &[0, 100], "incomplete precode");
}

#[cfg(feature = "std")]
#[test]
fn reader_stops_at_the_end_of_the_stream() {
    use std::io::{BufReader, Read};
    let data: Vec<u8> = (0..300_000u32)
        .map(|i| (i % 251) as u8 ^ (i / 1000) as u8)
        .collect();
    for format in FORMATS {
        let mut c = reference_compress(format, 6, &data);
        c.extend_from_slice(b"next");
        for cap in [1, 7, 100, 8192, 1 << 20] {
            // `read_to_end` decodes straight into the `Vec`.
            let mut r = Reader::new(format, BufReader::with_capacity(cap, &c[..]));
            let mut out = b"before".to_vec();
            assert_eq!(r.read_to_end(&mut out).unwrap(), data.len());
            assert!(
                out[..6] == *b"before" && out[6..] == data[..],
                "{format:?} cap {cap}"
            );
            assert_eq!(r.read(&mut [0; 10]).unwrap(), 0);
            let mut rest = Vec::new();
            r.into_inner().read_to_end(&mut rest).unwrap();
            assert_eq!(rest, b"next", "{format:?} cap {cap}");

            // Some `read`s first, then `read_to_end` copies.
            let mut r = Reader::new(format, BufReader::with_capacity(cap, &c[..]));
            let mut out = vec![0; 1000];
            let mut n = 0;
            while n < out.len() {
                n += r.read(&mut out[n..(n + 77).min(1000)]).unwrap();
            }
            r.read_to_end(&mut out).unwrap();
            assert!(out == data, "{format:?} cap {cap}");
            assert_eq!(r.decoder().total_in(), (c.len() - 4) as u64);
        }
        let mut r = Reader::new(format, &c[..c.len() / 2]);
        let e = r.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof, "{format:?}");
        let mut r = Reader::new(format, &c[..c.len() / 2]);
        let e = r
            .read(&mut vec![0; 1 << 20])
            .and_then(|_| r.read(&mut vec![0; 1 << 20]));
        assert_eq!(
            e.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof,
            "{format:?}"
        );
    }
}

#[cfg(feature = "std")]
#[test]
fn reader_errors_wrap_the_decode_error() {
    use std::io::Read;
    let mut c = reference_compress(Format::Zlib, 6, b"some data");
    c[0] ^= 0xff;
    let e = Reader::new(Format::Zlib, &c[..])
        .read_to_end(&mut Vec::new())
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        e.get_ref().and_then(|e| e.downcast_ref::<Error>()),
        Some(&Error::BadData)
    );
    let mut s = std::string::String::new();
    let c = reference_compress(Format::Zlib, 6, &[0xff, 0xfe]);
    let e = Reader::new(Format::Zlib, &c[..])
        .read_to_string(&mut s)
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
}

#[cfg(feature = "std")]
#[test]
fn scan_reader_stops_at_the_end_of_each_stream() {
    use std::io::{BufReader, Read};
    let data: Vec<u8> = (0..300_000u32)
        .map(|i| (i % 251) as u8 ^ (i / 1000) as u8)
        .collect();
    for format in FORMATS {
        let first = reference_compress(format, 6, &data);
        let second = reference_compress(format, 1, b"second");
        let mut c = [&first[..], &second[..], b"next"].concat();
        for cap in [1, 7, 100, 8192, 1 << 20] {
            let mut reader = BufReader::with_capacity(cap, &c[..]);
            let mut s = Scanner::new(format);
            let l = s.scan_reader(&mut reader).unwrap();
            assert_eq!(
                (l.compressed, l.decompressed),
                (first.len() as u64, data.len() as u64)
            );
            let l = s.scan_reader(&mut reader).unwrap();
            assert_eq!((l.compressed, l.decompressed), (second.len() as u64, 6));
            let mut rest = Vec::new();
            reader.read_to_end(&mut rest).unwrap();
            assert_eq!(rest, b"next", "{format:?} cap {cap}");
        }
        c.truncate(first.len() / 2);
        let e = Scanner::new(format).scan_reader(&c[..]).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof, "{format:?}");
    }
}

#[test]
fn vec_grows_past_the_first_guess() {
    // About 1000:1, far past the 4x guess for deflate and zlib.
    let data = vec![b'a'; 1 << 20];
    for format in FORMATS {
        let c = reference_compress(format, 2, &data);
        assert_eq!(
            crate::decompress(format, &c, data.len()).as_deref(),
            Ok(&data[..])
        );
        assert_eq!(
            crate::decompress(format, &c, data.len() - 1),
            Err(Error::InsufficientSpace),
            "{format:?}"
        );
    }
}

#[test]
fn vec_rejects_trailing_data() {
    for format in FORMATS {
        let mut c = reference_compress(format, 2, b"hello hello hello");
        c.extend([0, 0]);
        let want = match format {
            // A gzip member can't start with zero bytes.
            Format::Gzip => Error::BadData,
            _ => Error::TrailingData,
        };
        assert_eq!(crate::decompress(format, &c, 1000), Err(want), "{format:?}");
    }
}

#[test]
fn gzip_vec_reads_every_member() {
    let mut c = reference_compress(Format::Gzip, 2, b"first member, ");
    c.extend(reference_compress(Format::Gzip, 10, &[b'x'; 5000]));
    let mut want = b"first member, ".to_vec();
    want.extend([b'x'; 5000]);
    assert_eq!(crate::decompress(Format::Gzip, &c, 1 << 20), Ok(want));

    // A decoder reads one member.
    let mut out = [0; 100];
    let p = Decoder::new(Format::Gzip)
        .decompress(&c, &mut out, true)
        .unwrap();
    assert!(p.done);
    assert_eq!(p.output, b"first member, ");
    assert!(p.consumed < c.len());
}

#[test]
fn gzip_vec_survives_a_wrong_size_footer() {
    let data = vec![b'z'; 100_000];
    for isize in [0u32, 1, 99_999, 100_001, u32::MAX] {
        let mut c = reference_compress(Format::Gzip, 2, &data);
        let n = c.len();
        c[n - 4..].copy_from_slice(&isize.to_le_bytes());
        // The footer is wrong, so this must fail, but cleanly and without
        // allocating more than the limit.
        assert_eq!(
            crate::decompress(Format::Gzip, &c, 200_000),
            Err(Error::ChecksumMismatch),
            "ISIZE {isize}"
        );
    }
}

#[test]
fn checksum_mismatch_is_reported() {
    for format in [Format::Zlib, Format::Gzip] {
        let mut c = reference_compress(format, 2, b"checksummed data");
        let i = match format {
            Format::Zlib => c.len() - 1,
            _ => c.len() - 5,
        };
        c[i] ^= 1;
        assert_eq!(
            crate::decompress(format, &c, 1000),
            Err(Error::ChecksumMismatch),
            "{format:?}"
        );
        assert_eq!(
            Decoder::new(format).decompress(&c, &mut [0; 100], true),
            Err(Error::ChecksumMismatch),
            "{format:?}"
        );
    }
}

#[test]
fn empty_input_round_trips() {
    for format in FORMATS {
        let c = reference_compress(format, 6, b"");
        assert_eq!(crate::decompress(format, &c, 0), Ok(vec![]), "{format:?}");
        assert_eq!(
            one_shot(format, &c, 0),
            Ok((Some(vec![]), c.len())),
            "{format:?}"
        );
    }
}

/// Decompresses via the growing `Vec`, from a `guess`-byte start, so it
/// resumes many times.
fn decompress_from_guess(
    format: Format,
    c: &[u8],
    max_len: usize,
    guess: usize,
) -> Result<Vec<u8>, Error> {
    let mut out = b"prefix".to_vec();
    let consumed = VecDecoder {
        format,
        tables: Tables::new(),
    }
    .decompress_append_from(c, &mut out, 6 + max_len, guess)?;
    assert_eq!(consumed, c.len());
    assert_eq!(&out[..6], b"prefix");
    Ok(out.split_off(6))
}

/// Checks corrupt input fails alike in one go and through a growing buffer.
fn assert_resumed_errors_match(format: Format, c: &[u8], max_len: usize) {
    // A flipped bit can make the output longer than `max_len`.
    let want = one_shot(format, c, max_len)
        .and_then(|(out, consumed)| Ok((consumed, out.ok_or(Error::InsufficientSpace)?)));
    let mut out = Vec::new();
    let got = VecDecoder {
        format,
        tables: Tables::new(),
    }
    .decompress_append_from(c, &mut out, max_len, 1)
    .map(|consumed| (consumed, out));
    match (&want, &got) {
        (Ok(_), _) => assert_eq!(got, want),
        // The kind of error may differ (bad data or truncated).
        (Err(_), Err(_)) => {}
        _ => panic!("{want:?} vs {got:?}"),
    }
}

/// A bit flip near the end of the stream that makes the decoder read
/// implicit zero bytes before it has to resume. Resuming used to give back
/// more input than had been read.
#[test]
fn resume_after_reading_past_the_end() {
    let c = [
        0xed, 0xc0, 0x31, 0x0d, 0x00, 0x00, 0x0c, 0x02, 0xb0, 0x63, 0x16, 0xf0, 0xaf, 0x75, 0x36,
        0x20, 0x69, 0x13, 0x00, 0x0a, 0x18, 0x2c, 0x78,
    ];
    assert_resumed_errors_match(Format::Deflate, &c, 2104 + 5000);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn chunked_matches_one_shot(
        data in data(),
        level in 0i32..=12,
        f in 0usize..3,
        in_sizes in in_sizes(),
        buf_len in buf_len(),
        trailing in prop::collection::vec(any::<u8>(), 0..20),
    ) {
        let format = FORMATS[f];
        let mut c = reference_compress(format, level, &data);
        let len = c.len();
        c.extend(&trailing);
        prop_assert_eq!(one_shot(format, &c, data.len()), Ok((Some(data.clone()), len)));
        let (out, consumed) = stream(format, &c, &in_sizes, buf_len).unwrap();
        prop_assert!(out == data);
        prop_assert_eq!(consumed, len);
        prop_assert_eq!(scan(format, &c, &in_sizes), Ok((data.len() as u64, len)));
    }

    #[test]
    fn chunked_errors_match_one_shot(
        data in prop::collection::vec(0u8..8, 0..20_000),
        level in 0i32..=12,
        f in 0usize..3,
        flips in prop::collection::vec((any::<prop::sample::Index>(), 0u8..8), 1..4),
        in_sizes in in_sizes(),
        buf_len in buf_len(),
    ) {
        let format = FORMATS[f];
        let mut c = reference_compress(format, level, &data);
        for (i, bit) in flips {
            let i = i.index(c.len());
            c[i] ^= 1 << bit;
        }
        // DEFLATE expands at most 1032x.
        let room = c.len() * 1032;
        let want = one_shot(format, &c, room);
        let got = stream(format, &c, &in_sizes, buf_len.max(room));
        match (&want, &got) {
            (Ok((Some(w), wc)), Ok((g, gc))) => prop_assert!(w == g && wc == gc),
            (Err(_), Err(_)) => {}
            _ => prop_assert!(false, "one-shot {:?} vs stream {:?}",
                want.as_ref().map(|w| (w.0.as_ref().map(Vec::len), w.1)),
                got.as_ref().map(|g| (g.0.len(), g.1))),
        }
        // Scanning fails alike, short of the checksums.
        let scanned = scan(format, &c, &in_sizes);
        match &want {
            Ok((Some(w), wc)) => prop_assert_eq!(scanned, Ok((w.len() as u64, *wc))),
            // The checksum may also be all of a cut-off trailer, which then
            // looks truncated.
            Err(Error::ChecksumMismatch) => prop_assert!(
                matches!(scanned, Ok(_) | Err(Error::ChecksumMismatch | Error::Truncated)),
                "{:?}", scanned
            ),
            // One-shot ends the body before the trailer; scanning can't, so a
            // corrupt stream can be truncated to one and bad data to the other.
            Err(e @ (Error::BadData | Error::Truncated)) => prop_assert!(
                matches!(scanned, Err(Error::BadData | Error::Truncated)), "{:?} vs {:?}", e, scanned
            ),
            Err(e) => prop_assert_eq!(scanned, Err(*e)),
            Ok((None, _)) => unreachable!(),
        }
    }

    /// A short buffer is filled with the start of the output.
    #[test]
    fn short_buffers_get_the_start(data in small_data(), level in 0i32..=12, f in 0usize..3, len in 0usize..600) {
        let format = FORMATS[f];
        let c = reference_compress(format, level, &data);
        let mut buf = vec![0; len];
        let p = Decoder::new(format).decompress(&c, &mut buf, true).unwrap();
        let n = len.min(data.len());
        prop_assert_eq!(p.output, &data[..n]);
        prop_assert_eq!(p.done, len >= data.len());
    }

    #[test]
    fn truncated_input_is_an_error(
        data in prop::collection::vec(0u8..8, 1..5_000),
        f in 0usize..3,
        cut in any::<prop::sample::Index>(),
        in_sizes in in_sizes(),
    ) {
        let format = FORMATS[f];
        let c = reference_compress(format, 6, &data);
        let c = &c[..cut.index(c.len())];
        prop_assert!(stream(format, c, &in_sizes, 1 << 16).is_err());
        prop_assert!(scan(format, c, &in_sizes).is_err());
        prop_assert!(one_shot(format, c, data.len()).is_err());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// Stopping before a literal, a match or a stored block and carrying on
    /// in a bigger buffer gives the same output as one big buffer.
    #[test]
    fn resumed_decode_matches(data in small_data(), level in 0i32..=12, f in 0usize..3, guess in 0usize..64) {
        let format = FORMATS[f];
        let c = reference_compress(format, level, &data);
        prop_assert_eq!(decompress_from_guess(format, &c, data.len(), guess), Ok(data.clone()));
        if !data.is_empty() {
            prop_assert_eq!(
                decompress_from_guess(format, &c, data.len() - 1, guess),
                Err(Error::InsufficientSpace)
            );
        }
    }

    /// Corrupt input gives the same error whether or not the decode had to
    /// resume along the way.
    #[test]
    fn resumed_decode_errors_match(data in small_data(), level in 0i32..=12, f in 0usize..3, flip in any::<prop::sample::Index>(), bit in 0u8..8) {
        let format = FORMATS[f];
        let mut c = reference_compress(format, level, &data);
        let i = flip.index(c.len());
        c[i] ^= 1 << bit;
        assert_resumed_errors_match(format, &c, data.len() + 5000);
    }
}

/// Input that runs out as the output fills up is truncated, as zlib.
#[test]
fn a_stored_block_cut_off_at_a_full_buffer_is_truncated() {
    let input = [0x01, 10, 0, !10, 0xFF, 1, 2, 3, 4, 5];
    let mut buf = [0; 5];
    let r = Decoder::new(Format::Deflate).decompress(&input, &mut buf, true);
    assert_eq!(r, Err(Error::Truncated));
    assert_eq!(
        crate::decompress(Format::Deflate, &input, 5),
        Err(Error::Truncated)
    );
}

/// Asserts every way to decode `input` fails with `want`.
fn assert_fails(format: Format, input: &[u8], want: Error) {
    assert_eq!(
        one_shot(format, input, 100).map(|_| ()),
        Err(want),
        "{input:x?}"
    );
    assert_eq!(
        stream(format, input, &[1], 1 << 16).map(|_| ()),
        Err(want),
        "{input:x?}"
    );
    assert_eq!(
        crate::decompress(format, input, 100),
        Err(want),
        "{input:x?}"
    );
}

/// As zlib, each part of a header or trailer is checked once it is in,
/// and the input ending before then is truncation.
#[test]
fn header_and_trailer_parts_are_checked_as_they_come_in() {
    assert_fails(Format::Gzip, &[0x1F], Error::Truncated);
    assert_fails(Format::Gzip, &[0x1F, 0x8C], Error::BadData);
    assert_fails(Format::Gzip, &[0x1F, 0x8B, 9], Error::Truncated);
    assert_fails(Format::Gzip, &[0x1F, 0x8B, 9, 0], Error::BadData);
    assert_fails(Format::Gzip, &[0x1F, 0x8B, 8, 0x20], Error::BadData);
    // zlib reads the dictionary's ID before asking for the dictionary.
    assert_fails(Format::Zlib, &[0x78, 0x20, 0, 0, 0], Error::Truncated);
    assert_fails(
        Format::Zlib,
        &[0x78, 0x20, 0, 0, 0, 0],
        Error::PresetDictionary,
    );

    // A trailer's checksum is checked without its size.
    let mut c = reference_compress(Format::Gzip, 6, b"hello hello hello");
    let n = c.len();
    c[n - 8] ^= 1;
    assert_fails(Format::Gzip, &c[..n - 2], Error::ChecksumMismatch);
    c[n - 8] ^= 1;
    assert_fails(Format::Gzip, &c[..n - 2], Error::Truncated);
}

#[test]
fn a_gzip_header_crc_is_checked() {
    let data = b"hello hello hello";
    let c = reference_compress(Format::Gzip, 6, data);
    // Set FHCRC and insert the header's CRC after its fixed 10 bytes.
    let mut header = c[..10].to_vec();
    header[3] |= 0x02;
    let crc = crate::checksum::crc32(0, &header) as u16;
    let with = |crc: u16| [&header[..], &crc.to_le_bytes(), &c[10..]].concat();
    let good = with(crc);
    assert_eq!(
        one_shot(Format::Gzip, &good, 100),
        Ok((Some(data.to_vec()), good.len()))
    );
    assert_eq!(
        stream(Format::Gzip, &good, &[1], 1 << 16).map(|r| r.0),
        Ok(data.to_vec())
    );
    assert_eq!(
        crate::decompress(Format::Gzip, &good, 100),
        Ok(data.to_vec())
    );
    assert_fails(Format::Gzip, &with(crc ^ 1), Error::BadData);
}

/// zlib decodes every length of an empty precode as 0 from 1 bit, so the
/// block lacks end of block, or is truncated if the input ends first.
#[test]
fn an_empty_precode_reads_a_bit_per_length() {
    let mut w = BitWriter::default();
    w.put(1, 1); // final block
    w.put(2, 2); // dynamic Huffman
    w.put(0, 5 + 5 + 4); // 257 litlen, 1 offset and 4 precode lengths
    w.put(0, 12); // all 0
    let header = w.finish();
    // 29 header bits and 258 length bits fit in 36 bytes.
    let stream = |len: usize| [&header[..], &vec![0; len - header.len()]].concat();
    assert_fails(Format::Deflate, &stream(36), Error::BadData);
    assert_fails(Format::Deflate, &stream(35), Error::Truncated);
}

/// Found by fuzzing: a repeat of the previous length with none before it is
/// bad data only once its count is in, as zlib.
#[test]
fn a_leading_repeat_is_checked_after_its_count() {
    // A dynamic block whose first code length is precode symbol 16, cut
    // off one bit into its 2-bit count.
    let input = [0xBC, 0, 4, 0xA5];
    assert_fails(Format::Deflate, &input, Error::Truncated);
    assert_fails(
        Format::Deflate,
        &[&input[..], &[0]].concat(),
        Error::BadData,
    );
}
