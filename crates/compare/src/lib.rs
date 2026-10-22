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
/// libdeflate (with an `out_len`-byte buffer) didn't, but zlib (zlib-rs)
/// rejects it too: libdeflate is more lenient than zlib, and sirocco is as
/// strict. For raw DEFLATE, also panics if sirocco accepted `input` and
/// zlib doesn't, or decodes it differently. (The body of every format is raw
/// DEFLATE, and in the others zlib checks more of the wrapper than sirocco
/// and libdeflate do, such as a gzip header's CRC.)
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
                && zlib_decompress(format, input, out_len).is_err()),
        "sirocco {ours:?} vs libdeflate {theirs:?}: {what}"
    );
    if let (Format::Deflate, Outcome::Ok(_, out)) = (format, ours) {
        let z = zlib_decompress(format, input, out_len);
        assert!(
            matches!(&z, Ok(Some(o)) if o == out),
            "sirocco {ours:?} vs zlib-rs {z:?}: {what}"
        );
    }
}

/// Decompresses `input` with zlib (zlib-rs) into an `out_len`-byte buffer:
/// the output if the stream ends, `None` if the buffer or the input runs out
/// first, and an error if the data is bad.
pub fn zlib_decompress(
    format: Format,
    input: &[u8],
    out_len: usize,
) -> Result<Option<Vec<u8>>, zlib_rs::InflateError> {
    let mut z = match format {
        Format::Deflate => zlib_rs::Inflate::new(false, 15),
        Format::Zlib => zlib_rs::Inflate::new(true, 15),
        Format::Gzip => zlib_rs::Inflate::new(true, 16 + 15),
    };
    let mut out = vec![0; out_len];
    match z.decompress(input, &mut out, zlib_rs::InflateFlush::Finish)? {
        zlib_rs::Status::StreamEnd => {
            out.truncate(z.total_out() as usize);
            Ok(Some(out))
        }
        _ => Ok(None),
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
