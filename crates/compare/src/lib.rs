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
    let r = sirocco::Decompressor::new(sirocco::Format::Deflate)
        .decompress_into(data, out)
        .unwrap();
    assert_eq!(
        r.written,
        out.len(),
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

/// Decompresses with sirocco into an `out_len`-byte buffer.
pub fn sirocco_decompress(
    d: &mut sirocco::Decompressor,
    format: Format,
    input: &[u8],
    out_len: usize,
) -> Outcome {
    let mut out = vec![0u8; out_len];
    d.set_format(format.ff());
    match d.decompress_into(input, &mut out) {
        Ok(r) => {
            out.truncate(r.written);
            Outcome::Ok(r.consumed, out)
        }
        // libdeflate reports a bad checksum as bad data.
        Err(sirocco::DecompressError::BadData | sirocco::DecompressError::ChecksumMismatch) => {
            Outcome::BadData
        }
        Err(sirocco::DecompressError::InsufficientSpace) => Outcome::InsufficientSpace,
        Err(e) => panic!("unexpected error {e:?}"),
    }
}

/// Decompresses `input` by handing it over in pieces of the `ins` sizes and
/// taking output in pieces of the `outs` sizes (both cycled).
pub fn sirocco_stream(format: Format, input: &[u8], ins: &[usize], outs: &[usize]) -> Outcome {
    let mut d = sirocco::StreamDecompressor::new(format.ff());
    let (mut ip, mut out, mut buf) = (0usize, Vec::new(), vec![0; 1 << 17]);
    for step in 0.. {
        let end = ip.saturating_add(ins[step % ins.len()]).min(input.len());
        let n = outs[step % outs.len()];
        match d.decompress(&input[ip..end], &mut buf[..n], end == input.len()) {
            Ok(p) => {
                ip += p.consumed;
                out.extend_from_slice(&buf[..p.written]);
                if p.status == sirocco::StreamStatus::Done {
                    return Outcome::Ok(ip, out);
                }
            }
            // libdeflate reports a bad checksum and a cut-off stream as bad data.
            Err(
                sirocco::DecompressError::BadData
                | sirocco::DecompressError::ChecksumMismatch
                | sirocco::DecompressError::Truncated,
            ) => return Outcome::BadData,
            Err(e) => panic!("unexpected {e:?}"),
        }
    }
    unreachable!()
}
