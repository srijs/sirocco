//! Shared helpers for comparing sirocco against libdeflate and other
//! DEFLATE libraries.
//!
//! The built-in corpus is synthetic and deterministic so tests and benchmarks
//! run anywhere without downloads. Real-world corpora (Silesia, Canterbury,
//! enwik8) come from `scripts/fetch-corpus.sh`; see [`corpus`].

use std::path::{Path, PathBuf};

pub mod suite;

/// A named benchmark / test input.
#[derive(Debug)]
pub struct Sample {
    /// Human-readable label used in test and benchmark names.
    pub name: String,
    /// The input bytes.
    pub data: Vec<u8>,
}

/// Small xorshift PRNG so the corpus is reproducible without extra deps.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

pub(crate) const WORDS: &[&str] = &[
    "the", "of", "and", "deflate", "huffman", "literal", "match", "length", "offset", "block",
    "stream", "window", "symbol", "code", "table", "fast", "rust", "port", "buffer", "zlib",
];

/// Builds the synthetic corpus: incompressible, a single repeated byte, and
/// text-like data at a few sizes.
#[must_use]
pub fn synthetic_corpus() -> Vec<Sample> {
    let mut out = Vec::new();
    for &size in &[4 << 10, 256 << 10, 4 << 20] {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ size as u64);

        let random: Vec<u8> = (0..size).map(|_| rng.next() as u8).collect();
        out.push(Sample {
            name: format!("random-{}", human(size)),
            data: random,
        });

        // A non-zero byte: an all-zero `vec!` is backed by the shared zero
        // page, which makes it unrealistically cache-friendly.
        let repeat = vec![b'a'; size];
        out.push(Sample {
            name: format!("repeat-{}", human(size)),
            data: repeat,
        });

        let mut text = Vec::with_capacity(size);
        while text.len() < size {
            let w = WORDS[(rng.next() % WORDS.len() as u64) as usize];
            text.extend_from_slice(w.as_bytes());
            text.push(if rng.next().is_multiple_of(12) {
                b'\n'
            } else {
                b' '
            });
        }
        text.truncate(size);
        out.push(Sample {
            name: format!("text-{}", human(size)),
            data: text,
        });
    }
    out
}

/// Synthetic corpus plus every file under the real-corpus directory, if any.
///
/// The directory is `SIROCCO_CORPUS_DIR` if set, else `corpus/` at the
/// workspace root (where `scripts/fetch-corpus.sh` puts it). Files are found
/// recursively and named by their path relative to it, e.g. `silesia/dickens`.
#[must_use]
pub fn corpus() -> Vec<Sample> {
    let mut out = synthetic_corpus();
    out.extend(real_corpus());
    out
}

/// Only the files under the real-corpus directory (see [`corpus`]).
pub fn real_corpus() -> Vec<Sample> {
    let Some(dir) = corpus_dir() else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    walk(&dir, &mut paths);
    paths.sort();
    paths
        .into_iter()
        .map(|p| Sample {
            name: p
                .strip_prefix(&dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/"),
            data: std::fs::read(&p).unwrap(),
        })
        .collect()
}

fn corpus_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SIROCCO_CORPUS_DIR") {
        return Some(dir.into());
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus");
    dir.is_dir().then_some(dir)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let p = e.unwrap().path();
        let hidden = p.file_name().unwrap().to_string_lossy().starts_with('.');
        if hidden {
            continue;
        }
        if p.is_dir() {
            walk(&p, out);
        } else if p.is_file() {
            out.push(p);
        }
    }
}

/// A DEFLATE decoder to compare against.
#[derive(Debug)]
pub struct Codec {
    pub name: &'static str,
    /// Decompresses raw DEFLATE into all of `out`, panicking on malformed
    /// input or if the output is not exactly `out.len()` bytes. Taking the
    /// buffer keeps allocation out of decompression timings.
    pub decompress_into: fn(data: &[u8], out: &mut [u8]),
}

impl Codec {
    /// Decompresses to exactly `size` bytes.
    pub fn decompress(&self, data: &[u8], size: usize) -> Vec<u8> {
        let mut out = vec![0; size];
        (self.decompress_into)(data, &mut out);
        out
    }
}

/// The decoders to compare. The first is the reference the others are
/// judged against.
///
/// zenflate is AGPL-3.0 (or commercial), so it is only built with the opt-in
/// `zenflate` feature.
pub fn codecs() -> Vec<Codec> {
    vec![
        Codec {
            name: "libdeflate",
            decompress_into: libdeflate_decompress_into,
        },
        Codec {
            name: "sirocco",
            decompress_into: sirocco_decompress_into,
        },
        Codec {
            name: "zlib-rs",
            decompress_into: zlib_rs_decompress_into,
        },
        Codec {
            name: "miniz_oxide",
            decompress_into: miniz_oxide_decompress_into,
        },
        #[cfg(feature = "zenflate")]
        Codec {
            name: "zenflate",
            decompress_into: zenflate_decompress_into,
        },
    ]
}

pub fn libdeflate_decompress_into(data: &[u8], out: &mut [u8]) {
    let n = libdeflater::Decompressor::new()
        .deflate_decompress(data, out)
        .unwrap();
    assert_eq!(n, out.len(), "libdeflate decompressed to the wrong size");
}

pub fn sirocco_decompress_into(data: &[u8], out: &mut [u8]) {
    let len = out.len();
    let p = sirocco::inflate::Decoder::new(sirocco::Format::Deflate)
        .decompress(data, out, true)
        .unwrap();
    assert!(p.done, "sirocco didn't finish");
    assert_eq!(
        p.output.len(),
        len,
        "sirocco decompressed to the wrong size"
    );
}

pub fn zlib_rs_decompress_into(data: &[u8], out: &mut [u8]) {
    let config = zlib_rs::InflateConfig { window_bits: -15 };
    let size = out.len();
    let (decompressed, rc) = zlib_rs::decompress_slice(out, data, config);
    assert_eq!(rc, zlib_rs::ReturnCode::Ok, "zlib-rs decompress");
    assert_eq!(
        decompressed.len(),
        size,
        "zlib-rs decompressed to the wrong size"
    );
}

pub fn miniz_oxide_decompress_into(data: &[u8], out: &mut [u8]) {
    use miniz_oxide::inflate::core::{decompress, inflate_flags, DecompressorOxide};
    use miniz_oxide::inflate::TINFLStatus;

    let mut d = DecompressorOxide::new();
    let flags = inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
    let (status, _, n) = decompress(&mut d, data, out, 0, flags);
    assert_eq!(status, TINFLStatus::Done, "miniz_oxide decompress");
    assert_eq!(n, out.len(), "miniz_oxide decompressed to the wrong size");
}

#[cfg(feature = "zenflate")]
pub fn zenflate_decompress_into(data: &[u8], out: &mut [u8]) {
    let r = zenflate::Decompressor::new()
        .deflate_decompress(data, out, zenflate::Unstoppable)
        .unwrap();
    assert_eq!(
        r.output_written,
        out.len(),
        "zenflate decompressed to the wrong size"
    );
}

fn human(n: usize) -> String {
    if n >= 1 << 20 {
        format!("{}MiB", n >> 20)
    } else {
        format!("{}KiB", n >> 10)
    }
}

/// The three container formats libdeflate and sirocco support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Deflate,
    Zlib,
    Gzip,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Deflate, Format::Zlib, Format::Gzip];

    /// The same format in sirocco's terms.
    #[must_use]
    pub fn ff(self) -> sirocco::Format {
        match self {
            Format::Deflate => sirocco::Format::Deflate,
            Format::Zlib => sirocco::Format::Zlib,
            Format::Gzip => sirocco::Format::Gzip,
        }
    }
}

/// Compresses `data` with libdeflate at `level` (0-12).
#[must_use]
pub fn reference_compress(format: Format, level: i32, data: &[u8]) -> Vec<u8> {
    let lvl = libdeflater::CompressionLvl::new(level).expect("valid level");
    let mut c = libdeflater::Compressor::new(lvl);
    let bound = match format {
        Format::Deflate => c.deflate_compress_bound(data.len()),
        Format::Zlib => c.zlib_compress_bound(data.len()),
        Format::Gzip => c.gzip_compress_bound(data.len()),
    };
    let mut out = vec![0; bound];
    let n = match format {
        Format::Deflate => c.deflate_compress(data, &mut out),
        Format::Zlib => c.zlib_compress(data, &mut out),
        Format::Gzip => c.gzip_compress(data, &mut out),
    }
    .expect("compress bound is enough");
    out.truncate(n);
    out
}

/// Outcome of a decompression, in a form both implementations map to.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Input bytes consumed and the decompressed bytes.
    Ok(usize, Vec<u8>),
    BadData,
    InsufficientSpace,
    /// The input ended early (only sirocco tells this apart).
    Truncated,
}

impl Outcome {
    /// Whether sirocco's outcome agrees with libdeflate's. libdeflate reports
    /// truncation as bad data or lack of room, so
    /// [`Truncated`](Outcome::Truncated) agrees with either.
    #[must_use]
    pub fn agrees_with(&self, reference: &Outcome) -> bool {
        match self {
            Outcome::Truncated => {
                matches!(reference, Outcome::BadData | Outcome::InsufficientSpace)
            }
            _ => self == reference,
        }
    }
}

/// Panics unless [`Outcome::agrees_with`], or sirocco rejected `input` and
/// libdeflate (with an `out_len`-byte buffer) didn't because of a symbol RFC
/// 1951 reserves (see [`rejected_for_reserved_symbol`]): there sirocco is
/// strict like zlib, and libdeflate lenient.
#[track_caller]
pub fn assert_agree(
    ours: &Outcome,
    theirs: &Outcome,
    format: Format,
    input: &[u8],
    out_len: usize,
    what: &dyn std::fmt::Display,
) {
    assert!(
        ours.agrees_with(theirs)
            || (matches!(ours, Outcome::BadData | Outcome::Truncated)
                && matches!(theirs, Outcome::Ok(..) | Outcome::InsufficientSpace)
                && rejected_for_reserved_symbol(format, input, out_len)),
        "sirocco {ours:?} vs libdeflate {theirs:?}: {what}"
    );
}

/// Whether the first thing wrong with `input` is a symbol RFC 1951 reserves,
/// reached within the first `out_len` bytes of output: litlen symbol 286 or
/// 287 or offset symbol 30 or 31 in a static Huffman block, or a dynamic
/// block header that declares more than 286 litlen or 30 offset codes.
/// sirocco rejects these, as zlib does; libdeflate decodes the symbols as 285
/// and 29, and accepts the headers.
///
/// Two independent decoders must agree: [`walk_deflate`] must find the
/// reserved symbol before any other problem, and zlib (zlib-rs) must fail
/// on it, with output at most `out_len` bytes. zlib's messages alone aren't
/// enough: it gives the same ones for a codeword missing from an incomplete
/// dynamic code, which libdeflate and sirocco decode as the code's one
/// symbol.
#[must_use]
pub fn rejected_for_reserved_symbol(format: Format, input: &[u8], out_len: usize) -> bool {
    walk_deflate(format, input) == Some(true)
        && zlib_fails_on_reserved_symbol(format, input, out_len)
}

/// zlib's errors for the reserved symbols.
const RESERVED_SYMBOL_ERRORS: [&str; 3] = [
    "invalid literal/length code",
    "invalid distance code",
    "too many length or distance symbols",
];

/// Whether zlib-rs fails on `input` with one of [`RESERVED_SYMBOL_ERRORS`],
/// having written at most `out_len` bytes.
fn zlib_fails_on_reserved_symbol(format: Format, input: &[u8], out_len: usize) -> bool {
    let mut z = match format {
        Format::Deflate => zlib_rs::Inflate::new(false, 15),
        Format::Zlib => zlib_rs::Inflate::new(true, 15),
        Format::Gzip => zlib_rs::Inflate::new(true, 16 + 15),
    };
    // A byte at a time, which keeps zlib-rs out of its fast decode loop:
    // errors there come back as "repeated call with bad state".
    let mut out = vec![0; 1 << 16];
    loop {
        let (bi, bo) = (z.total_in(), z.total_out());
        let Some(byte) = input.get(bi as usize..=bi as usize) else {
            return false;
        };
        match z.decompress(byte, &mut out, zlib_rs::InflateFlush::NoFlush) {
            Ok(zlib_rs::Status::StreamEnd) => return false,
            Ok(_) if z.total_in() == bi && z.total_out() == bo => return false,
            Ok(_) => {}
            Err(_) => {
                return z.total_out() <= out_len as u64
                    && z.error_message()
                        .is_some_and(|m| RESERVED_SYMBOL_ERRORS.contains(&m));
            }
        }
    }
}

/// Walks the first DEFLATE stream in `input` (after its `format` header) a
/// bit at a time, as zlib's `puff` does, without writing output. `Some(true)`
/// if it comes to a reserved symbol (see [`rejected_for_reserved_symbol`])
/// before anything else goes wrong, `Some(false)` if it ends without one, and
/// `None` if the input ends first or something else is wrong, such as a
/// codeword that an incomplete code lacks.
#[must_use]
pub fn walk_deflate(format: Format, input: &[u8]) -> Option<bool> {
    let start = match format {
        Format::Deflate => 0,
        // No preset dictionary.
        Format::Zlib => (*input.get(1)? & 0x20 == 0).then_some(2)?,
        Format::Gzip => {
            let flags = *input.get(3)?;
            let mut p = 10;
            if flags & 4 != 0 {
                p += 2 + usize::from(u16::from_le_bytes([*input.get(p)?, *input.get(p + 1)?]));
            }
            for flag in [8, 16] {
                if flags & flag != 0 {
                    p += input.get(p..)?.iter().position(|&b| b == 0)? + 1;
                }
            }
            if flags & 2 != 0 {
                p += 2;
            }
            p
        }
    };
    let mut b = Bits {
        data: input.get(start..)?,
        pos: 0,
    };
    loop {
        let last = b.get(1)? == 1;
        match b.get(2)? {
            0 => {
                b.pos = b.pos.next_multiple_of(8);
                let len = b.get(16)?;
                if b.get(16)? != !len & 0xffff {
                    return None;
                }
                b.pos += 8 * len as usize;
                if b.pos > 8 * b.data.len() {
                    return None;
                }
            }
            1 => {
                let mut lens = [8; 288];
                lens[144..256].fill(9);
                lens[256..280].fill(7);
                // All 32 offset symbols, so that 30 and 31 decode.
                let (litlen, offset) = (Code::new(&lens)?, Code::new(&[5; 32])?);
                if walk_symbols(&mut b, &litlen, &offset)? {
                    return Some(true);
                }
            }
            2 => {
                let num_litlen = b.get(5)? as usize + 257;
                let num_offset = b.get(5)? as usize + 1;
                let num_precode = b.get(4)? as usize + 4;
                if num_litlen > 286 || num_offset > 30 {
                    return Some(true);
                }
                let mut precode_lens = [0; 19];
                for &sym in &[
                    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
                ][..num_precode]
                {
                    precode_lens[sym] = b.get(3)? as u8;
                }
                let precode = Code::new(&precode_lens)?;
                let mut lens = Vec::new();
                while lens.len() < num_litlen + num_offset {
                    let (len, n) = match precode.decode(&mut b)? {
                        len @ 0..16 => (len as u8, 1),
                        16 => (*lens.last()?, 3 + b.get(2)?),
                        17 => (0, 3 + b.get(3)?),
                        _ => (0, 11 + b.get(7)?),
                    };
                    lens.extend(std::iter::repeat_n(len, n as usize));
                }
                if lens.len() != num_litlen + num_offset {
                    return None;
                }
                let (litlen, offset) = lens.split_at(num_litlen);
                if walk_symbols(&mut b, &Code::new(litlen)?, &Code::new(offset)?)? {
                    return Some(true);
                }
            }
            _ => return None,
        }
        if last {
            return Some(false);
        }
    }
}

/// Walks the symbols of a Huffman block, up to its end: `Some(true)` at a
/// reserved symbol.
fn walk_symbols(b: &mut Bits<'_>, litlen: &Code, offset: &Code) -> Option<bool> {
    loop {
        match litlen.decode(b)? {
            0..256 => {}
            256 => return Some(false),
            286.. => return Some(true),
            sym => {
                let extra = if (265..285).contains(&sym) {
                    (sym - 261) / 4
                } else {
                    0
                };
                b.get(extra as u32)?;
                match offset.decode(b)? {
                    30.. => return Some(true),
                    sym => b.get(if sym < 4 { 0 } else { (sym as u32 - 2) / 2 })?,
                };
            }
        }
    }
}

/// Reads a DEFLATE body a bit at a time, lowest bit first.
struct Bits<'a> {
    data: &'a [u8],
    /// In bits.
    pos: usize,
}

impl Bits<'_> {
    /// The next `n` bits, or `None` past the end of the data.
    fn get(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for i in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v |= u32::from((byte >> (self.pos % 8)) & 1) << i;
            self.pos += 1;
        }
        Some(v)
    }
}

/// A canonical Huffman code, possibly incomplete, decoded a bit at a time.
struct Code {
    /// Codewords of each length.
    counts: [i32; 16],
    /// The symbols in codeword order.
    symbols: Vec<usize>,
}

impl Code {
    /// The code with codeword lengths `lens`, unless it is over-subscribed.
    fn new(lens: &[u8]) -> Option<Code> {
        let mut counts = [0; 16];
        for &len in lens {
            counts[usize::from(len)] += 1;
        }
        let mut left = 1;
        for &count in &counts[1..] {
            left = 2 * left - count;
            if left < 0 {
                return None;
            }
        }
        let mut symbols: Vec<usize> = (0..lens.len()).filter(|&s| lens[s] != 0).collect();
        symbols.sort_by_key(|&s| lens[s]);
        Some(Code { counts, symbols })
    }

    /// The next symbol, or `None` for a codeword the code lacks.
    fn decode(&self, b: &mut Bits<'_>) -> Option<usize> {
        let (mut code, mut first, mut index) = (0, 0, 0);
        for &count in &self.counts[1..] {
            code |= b.get(1)? as i32;
            if code - count < first {
                return Some(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        None
    }
}

/// libdeflate's decompressor, called through `libdeflate-sys` so the
/// consumed input size (`actual_in_nbytes`) can be compared too.
#[derive(Debug)]
pub struct ReferenceDecompressor(*mut libdeflate_sys::libdeflate_decompressor);

impl Default for ReferenceDecompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl ReferenceDecompressor {
    #[must_use]
    pub fn new() -> Self {
        // SAFETY: plain allocation.
        let p = unsafe { libdeflate_sys::libdeflate_alloc_decompressor() };
        assert!(!p.is_null());
        ReferenceDecompressor(p)
    }

    pub fn decompress(&mut self, format: Format, input: &[u8], out_len: usize) -> Outcome {
        use libdeflate_sys::*;
        let mut out = vec![0u8; out_len];
        let (mut actual_in, mut actual_out) = (0usize, 0usize);
        let f = match format {
            Format::Deflate => libdeflate_deflate_decompress_ex,
            Format::Zlib => libdeflate_zlib_decompress_ex,
            Format::Gzip => libdeflate_gzip_decompress_ex,
        };
        // SAFETY: pointers and lengths come from live slices.
        let r = unsafe {
            f(
                self.0,
                input.as_ptr().cast(),
                input.len(),
                out.as_mut_ptr().cast(),
                out.len(),
                &raw mut actual_in,
                &raw mut actual_out,
            )
        };
        if r == libdeflate_result_LIBDEFLATE_SUCCESS {
            out.truncate(actual_out);
            Outcome::Ok(actual_in, out)
        } else if r == libdeflate_result_LIBDEFLATE_BAD_DATA {
            Outcome::BadData
        } else if r == libdeflate_result_LIBDEFLATE_INSUFFICIENT_SPACE {
            Outcome::InsufficientSpace
        } else {
            panic!("unexpected libdeflate result {r}")
        }
    }
}

impl Drop for ReferenceDecompressor {
    fn drop(&mut self) {
        // SAFETY: allocated in `new`, freed once.
        unsafe { libdeflate_sys::libdeflate_free_decompressor(self.0) }
    }
}

/// A sirocco decoder per [`Format`].
#[derive(Debug)]
pub struct SiroccoDecoders([Box<sirocco::inflate::Decoder>; 3]);

impl SiroccoDecoders {
    #[must_use]
    pub fn new() -> Self {
        SiroccoDecoders(Format::ALL.map(|f| Box::new(sirocco::inflate::Decoder::new(f.ff()))))
    }

    /// The decoder for `format`.
    pub fn get(&mut self, format: Format) -> &mut sirocco::inflate::Decoder {
        let i = Format::ALL.iter().position(|&f| f == format).unwrap();
        &mut self.0[i]
    }
}

impl Default for SiroccoDecoders {
    fn default() -> Self {
        Self::new()
    }
}

/// Decompresses with sirocco in one call into an `out_len`-byte buffer.
pub fn sirocco_decompress(
    d: &mut SiroccoDecoders,
    format: Format,
    input: &[u8],
    out_len: usize,
) -> Outcome {
    let mut out = vec![0u8; out_len];
    let d = d.get(format);
    d.reset();
    match d.decompress(input, &mut out, true) {
        Ok(p) if p.done => {
            let (consumed, written) = (p.consumed, p.output.len());
            out.truncate(written);
            Outcome::Ok(consumed, out)
        }
        Ok(_) => Outcome::InsufficientSpace,
        Err(e) => error_outcome(e),
    }
}

/// The outcome for a sirocco error.
fn error_outcome(e: sirocco::inflate::Error) -> Outcome {
    match e {
        // libdeflate reports a bad checksum or a preset dictionary as bad data.
        sirocco::inflate::Error::BadData
        | sirocco::inflate::Error::ChecksumMismatch
        | sirocco::inflate::Error::PresetDictionary => Outcome::BadData,
        sirocco::inflate::Error::Truncated => Outcome::Truncated,
        e => panic!("unexpected error {e:?}"),
    }
}

/// Decompresses `input` in pieces of the `ins` sizes (cycled) through one
/// buffer of `buf_len` bytes (at least 64 KiB), making room when it's full.
pub fn sirocco_stream(format: Format, input: &[u8], ins: &[usize], buf_len: usize) -> Outcome {
    let mut d = sirocco::inflate::Decoder::new(format.ff());
    let buf_len = buf_len.max(64 << 10);
    let (mut ip, mut out, mut buf) = (0usize, Vec::new(), vec![0; buf_len]);
    for step in 0.. {
        let end = ip.saturating_add(ins[step % ins.len()]).min(input.len());
        match d.decompress(&input[ip..end], &mut buf, end == input.len()) {
            Ok(p) => {
                ip += p.consumed;
                out.extend_from_slice(p.output);
                if p.done {
                    return Outcome::Ok(ip, out);
                }
                if p.full {
                    d.make_room(&mut buf);
                }
            }
            Err(e) => return error_outcome(e),
        }
    }
    unreachable!()
}
