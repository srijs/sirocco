//! Whole-buffer DEFLATE, zlib and gzip decompression.

mod inflate;
#[cfg(feature = "alloc")]
mod stream;
mod tables;
mod wrapper;

#[cfg(feature = "std")]
pub use stream::DecompressReader;
#[cfg(feature = "alloc")]
pub use stream::{
    MeasureProgress, StreamDecompressor, StreamMeasurer, StreamProgress, StreamStatus,
};

#[cfg(feature = "alloc")]
use alloc::vec::Vec;
use core::fmt;

use crate::Format;
use inflate::Status;
use tables::Tables;
use wrapper::{body_bounds, Check};

/// Why decompression failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DecompressError {
    /// The input is not valid compressed data in the expected format, or is
    /// truncated.
    BadData,
    /// The data decompressed, but the checksum or size in the zlib or gzip
    /// footer doesn't match it, so the output is wrong.
    ChecksumMismatch,
    /// The data would decompress to more bytes than the output buffer (or
    /// the size limit) allows.
    InsufficientSpace,
    /// There are bytes after the end of the compressed data. Only the `Vec`
    /// helpers, which expect the whole input to be one stream, report this;
    /// [`Decompressor::decompress_into`] reports how much it consumed
    /// instead.
    TrailingData,
    /// The input ended before the stream did. Only
    /// [`StreamDecompressor`] reports this, when told the input has ended;
    /// whole-buffer decompression can't tell it apart from
    /// [`BadData`](DecompressError::BadData).
    Truncated,
}

impl fmt::Display for DecompressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DecompressError::BadData => "invalid compressed data",
            DecompressError::ChecksumMismatch => "checksum mismatch in compressed data",
            DecompressError::InsufficientSpace => "output buffer too small",
            DecompressError::TrailingData => "unexpected data after the compressed stream",
            DecompressError::Truncated => "compressed data ended early",
        })
    }
}

impl core::error::Error for DecompressError {}

#[cfg(feature = "std")]
impl From<DecompressError> for std::io::Error {
    fn from(e: DecompressError) -> Self {
        let kind = match e {
            DecompressError::Truncated => std::io::ErrorKind::UnexpectedEof,
            _ => std::io::ErrorKind::InvalidData,
        };
        std::io::Error::new(kind, e)
    }
}

/// The result of [`Decompressor::decompress_into`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Decompressed {
    /// Bytes of input that made up the compressed stream, including any zlib
    /// or gzip header and footer. Bytes after that were not looked at.
    pub consumed: usize,
    /// Bytes written to the start of the output buffer.
    pub written: usize,
}

/// The result of [`Decompressor::measure`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Measured {
    /// Bytes of input that made up the compressed stream, including any zlib
    /// or gzip header and footer.
    pub consumed: usize,
    /// Bytes the stream decompresses to.
    pub len: usize,
}

/// A DEFLATE, zlib or gzip decompressor.
///
/// It works on whole buffers: all of the compressed data goes
/// in at once. [`decompress_into`](Self::decompress_into) needs an output
/// buffer big enough for all of the decompressed data;
/// [`decompress`](Self::decompress) grows a `Vec` instead. It holds the
/// Huffman decode tables (about 13 KiB), so reuse one across calls rather
/// than creating one per call; consider boxing it if stack space is tight.
///
/// ```
/// use sirocco::{Decompressor, Format};
///
/// let compressed = [0x4b, 0x4c, 0x4a, 0x06, 0x00]; // "abc", raw DEFLATE
/// let mut d = Decompressor::new(Format::Deflate);
/// let mut out = [0u8; 3];
/// let r = d.decompress_into(&compressed, &mut out)?;
/// assert_eq!((r.consumed, r.written), (5, 3));
/// assert_eq!(&out, b"abc");
/// # Ok::<(), sirocco::DecompressError>(())
/// ```
#[derive(Clone)]
pub struct Decompressor {
    format: Format,
    tables: Tables,
}

impl fmt::Debug for Decompressor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Decompressor")
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl Decompressor {
    /// Creates a decompressor for `format`.
    #[must_use]
    pub const fn new(format: Format) -> Self {
        Decompressor {
            format,
            tables: Tables::new(),
        }
    }

    /// The format this decompressor reads.
    #[must_use]
    pub const fn format(&self) -> Format {
        self.format
    }

    /// Switches to reading `format`, reusing the decode tables.
    pub fn set_format(&mut self, format: Format) {
        self.format = format;
    }

    /// Decompresses the stream at the start of `input` into the start of
    /// `output`.
    ///
    /// Bytes in `input` after the end of the stream are left alone; the
    /// result says where the stream ended, so concatenated streams can be
    /// walked one at a time. If the decompressed size is known, pass an
    /// output buffer of exactly that size and check
    /// [`written`](Decompressed::written).
    pub fn decompress_into(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<Decompressed, DecompressError> {
        // Raw DEFLATE skips the wrapper altogether, which matters for the
        // smallest streams.
        let raw = self.format == Format::Deflate;
        let (start, body_end) = if raw {
            (0, input.len())
        } else {
            body_bounds(self.format, input)?
        };
        let len = output.len();
        let (consumed, written) = whole(inflate::run::<false, true>(
            &mut self.tables,
            &input[start..body_end],
            output,
            len,
            None,
            false,
        ))?;
        if raw {
            return Ok(Decompressed { consumed, written });
        }
        let mut check = Check::new(self.format);
        check.update(&output[..written]);
        let consumed = check.verify_at(input, start + consumed)?;
        Ok(Decompressed { consumed, written })
    }

    /// Finds where the stream at the start of `input` ends and how many
    /// bytes it decompresses to, without writing any output.
    ///
    /// This decodes every symbol, so malformed DEFLATE data is still
    /// rejected, but it skips all the copying: a match costs the same as a
    /// literal, whatever its length. The zlib and gzip checksums cover the
    /// decompressed bytes, so they can't be checked; the gzip size field is.
    /// Use it to walk concatenated streams (such as a git pack) or to size an
    /// output buffer.
    ///
    /// ```
    /// use sirocco::{Decompressor, Format};
    ///
    /// let compressed = [0x4b, 0x4c, 0x4a, 0x06, 0x00, 0xff]; // "abc" and a trailing byte
    /// let m = Decompressor::new(Format::Deflate).measure(&compressed)?;
    /// assert_eq!((m.consumed, m.len), (5, 3));
    /// # Ok::<(), sirocco::DecompressError>(())
    /// ```
    pub fn measure(&mut self, input: &[u8]) -> Result<Measured, DecompressError> {
        let (start, body_end) = body_bounds(self.format, input)?;
        let body = &input[start..body_end];
        let (consumed, len) = whole(inflate::run::<false, false>(
            &mut self.tables,
            body,
            &mut [],
            usize::MAX,
            None,
            false,
        ))?;
        let mut check = Check::len_only(self.format);
        check.count(len);
        let consumed = check.verify_at(input, start + consumed)?;
        Ok(Measured { consumed, len })
    }

    /// Decompresses all of `input` into a new `Vec` of at most `max_len`
    /// bytes.
    ///
    /// Unlike [`decompress_into`](Self::decompress_into), `input` must be
    /// exactly one stream, or [`TrailingData`](DecompressError::TrailingData)
    /// is returned. The exception is gzip, where a file may hold several
    /// members back to back (as `cat a.gz b.gz` makes): they are all
    /// decompressed and concatenated, as `gzip -d` does.
    ///
    /// `max_len` guards against inputs that decompress to far more than
    /// expected; [`InsufficientSpace`](DecompressError::InsufficientSpace) is
    /// returned if the output would be longer.
    #[cfg(feature = "alloc")]
    pub fn decompress(&mut self, input: &[u8], max_len: usize) -> Result<Vec<u8>, DecompressError> {
        let mut out = Vec::new();
        let mut rest = input;
        loop {
            let consumed = self.decompress_append(rest, &mut out, max_len)?;
            rest = &rest[consumed..];
            if rest.is_empty() {
                return Ok(out);
            }
            if self.format != Format::Gzip {
                return Err(DecompressError::TrailingData);
            }
        }
    }

    /// Decompresses the stream at the start of `input`, appending to `out`
    /// without letting it grow past `max_len`. Returns the input consumed.
    #[cfg(feature = "alloc")]
    fn decompress_append(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
        max_len: usize,
    ) -> Result<usize, DecompressError> {
        // A whole-buffer decode needs room for all of the output up front,
        // so start from a guess and grow the buffer when it fills up. Gzip
        // records the size (of a single member, mod 2^32) in its last 4
        // bytes; otherwise guess from the input size.
        let guess = match (self.format, input.len().checked_sub(4)) {
            (Format::Gzip, Some(i)) => u32::from_le_bytes(input[i..].try_into().unwrap()) as usize,
            _ => input.len().saturating_mul(4).max(1024),
        };
        self.decompress_append_from(input, out, max_len, guess)
    }

    /// [`decompress_append`](Self::decompress_append), starting from a
    /// `guess`-byte buffer. On error, `out` is left as it was.
    #[cfg(feature = "alloc")]
    fn decompress_append_from(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
        max_len: usize,
        guess: usize,
    ) -> Result<usize, DecompressError> {
        let out_start = out.len();
        let r = body_bounds(self.format, input).and_then(|(start, body_end)| {
            let consumed =
                self.inflate_growing(&input[start..body_end], out, out_start, max_len, guess)?;
            let mut check = Check::new(self.format);
            check.update(&out[out_start..]);
            check.verify_at(input, start + consumed)
        });
        if r.is_err() {
            out.truncate(out_start);
        }
        r
    }

    /// Decompresses a raw DEFLATE stream, appending to `out` (which holds
    /// `start` bytes) from a `guess`-byte buffer, which grows until the
    /// stream ends or `out` would pass `max_len`. Returns the input consumed.
    #[cfg(feature = "alloc")]
    fn inflate_growing(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
        start: usize,
        max_len: usize,
        guess: usize,
    ) -> Result<usize, DecompressError> {
        let room = max_len - start;
        let mut len = guess.min(room);
        let mut resume = None;
        loop {
            if out.capacity() == 0 {
                // `vec!` gets zeroed pages straight from the allocator,
                // which is much cheaper for big buffers than writing the
                // zeros.
                *out = alloc::vec![0; len];
            } else {
                out.resize(start + len, 0);
            }
            match inflate::run::<true, true>(
                &mut self.tables,
                input,
                &mut out[start..],
                len,
                resume.as_ref(),
                false,
            )? {
                Status::Done(consumed, written) => {
                    out.truncate(start + written);
                    return Ok(consumed);
                }
                Status::Full(_) if len == room => {
                    return Err(DecompressError::InsufficientSpace);
                }
                // Carry on into a bigger buffer. Matches refer back into the
                // output itself, so moving it is fine.
                Status::Full(r) => {
                    len = next_len(len, r.in_pos(), input.len()).min(room);
                    resume = Some(r);
                }
            }
        }
    }
}

/// The result of a decode that isn't resumable, which only pauses when the
/// output is full: the input consumed and the output written.
fn whole(status: Result<Status, DecompressError>) -> Result<(usize, usize), DecompressError> {
    match status? {
        Status::Done(consumed, written) => Ok((consumed, written)),
        Status::Full(_) => Err(DecompressError::InsufficientSpace),
    }
}

/// The next output buffer size after `len` bytes came from the first
/// `in_pos` of `in_len` input bytes: the ratio so far extrapolated over the
/// rest of the input, plus an eighth. Grows by at least a quarter, so a
/// stream whose ratio jumps late still takes few steps.
#[cfg(feature = "alloc")]
fn next_len(len: usize, in_pos: usize, in_len: usize) -> usize {
    let rest = (in_len - in_pos) as u128;
    let estimate = if in_pos == 0 {
        len
    } else {
        usize::try_from(len as u128 * rest / in_pos as u128).unwrap_or(usize::MAX)
    };
    let extra = estimate.saturating_add(estimate / 8).max(len / 4).max(1024);
    len.saturating_add(extra)
}

/// Compresses `data` for these tests with miniz_oxide, at `level` capped to
/// its highest (10). A pure-Rust compressor keeps C out of this crate's
/// builds; `crates/compare` tests against libdeflate.
#[cfg(all(test, feature = "alloc"))]
fn reference_compress(format: Format, level: i32, data: &[u8]) -> Vec<u8> {
    use miniz_oxide::deflate::{compress_to_vec, compress_to_vec_zlib};
    let level = level.min(10) as u8;
    match format {
        Format::Deflate => compress_to_vec(data, level),
        Format::Zlib => compress_to_vec_zlib(data, level),
        Format::Gzip => {
            let mut out = alloc::vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
            out.extend(compress_to_vec(data, level));
            out.extend(crate::crc32(0, data).to_le_bytes());
            out.extend((data.len() as u32).to_le_bytes());
            out
        }
    }
}

#[cfg(all(test, feature = "alloc"))]
mod tests {
    use super::*;
    use alloc::vec;

    const FORMATS: [Format; 3] = [Format::Deflate, Format::Zlib, Format::Gzip];

    #[test]
    fn vec_grows_past_the_first_guess() {
        // About 1000:1, far past the 4x guess for deflate and zlib.
        let data = vec![b'a'; 1 << 20];
        for format in FORMATS {
            let c = reference_compress(format, 2, &data);
            let mut d = Decompressor::new(format);
            assert_eq!(d.decompress(&c, data.len()).as_deref(), Ok(&data[..]));
            assert_eq!(
                d.decompress(&c, data.len() - 1),
                Err(DecompressError::InsufficientSpace),
                "{format:?}"
            );
        }
    }

    #[test]
    fn vec_rejects_trailing_data() {
        for format in FORMATS {
            let mut c = reference_compress(format, 2, b"hello hello hello");
            c.push(0);
            let want = match format {
                // A gzip member can't start with a zero byte.
                Format::Gzip => DecompressError::BadData,
                _ => DecompressError::TrailingData,
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

        // decompress_into stops after the first member.
        let mut out = [0; 100];
        let r = Decompressor::new(Format::Gzip)
            .decompress_into(&c, &mut out)
            .unwrap();
        assert_eq!(&out[..r.written], b"first member, ");
        assert!(r.consumed < c.len());
    }

    #[test]
    fn gzip_vec_survives_a_wrong_size_footer() {
        let data = vec![b'z'; 100_000];
        for isize in [0u32, 1, 99_999, 100_001, u32::MAX] {
            let mut c = reference_compress(Format::Gzip, 2, &data);
            let n = c.len();
            c[n - 4..].copy_from_slice(&isize.to_le_bytes());
            // The footer is wrong, so this must fail, but cleanly and
            // without allocating more than the limit.
            assert_eq!(
                crate::decompress(Format::Gzip, &c, 200_000),
                Err(DecompressError::ChecksumMismatch),
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
                Err(DecompressError::ChecksumMismatch),
                "{format:?}"
            );
        }
    }

    #[test]
    fn empty_input_round_trips() {
        for format in FORMATS {
            let c = reference_compress(format, 6, b"");
            assert_eq!(crate::decompress(format, &c, 0), Ok(vec![]), "{format:?}");
        }
    }
}

#[cfg(all(test, feature = "alloc"))]
mod resume_tests {
    use super::*;
    use alloc::vec;
    use proptest::prelude::*;

    /// Decompresses through the growing-Vec path starting from a `guess`-byte
    /// buffer, so it has to stop and resume many times.
    fn decompress_from_guess(
        format: Format,
        c: &[u8],
        max_len: usize,
        guess: usize,
    ) -> Result<Vec<u8>, DecompressError> {
        let mut out = b"prefix".to_vec();
        let consumed =
            Decompressor::new(format).decompress_append_from(c, &mut out, 6 + max_len, guess)?;
        assert_eq!(consumed, c.len());
        assert_eq!(&out[..6], b"prefix");
        Ok(out.split_off(6))
    }

    fn data() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(
            prop_oneof![
                prop::collection::vec(any::<u8>(), 0..2000),
                (any::<u8>(), 1usize..3000).prop_map(|(b, n)| vec![b; n]),
                (prop::collection::vec(any::<u8>(), 1..9), 1usize..300)
                    .prop_map(|(p, n)| p.repeat(n)),
            ],
            0..20,
        )
        .prop_map(|chunks| chunks.concat())
    }

    /// Decodes corrupt input `c` in one go and again with a buffer that has to
    /// grow (so the decode resumes along the way), and checks both agree.
    fn assert_resumed_errors_match(format: Format, c: &[u8], max_len: usize) {
        let mut whole = vec![0; max_len];
        let want = Decompressor::new(format)
            .decompress_into(c, &mut whole)
            .map(|d| {
                whole.truncate(d.written);
                (d.consumed, whole)
            });
        let mut out = Vec::new();
        let got = Decompressor::new(format)
            .decompress_append_from(c, &mut out, max_len, 1)
            .map(|consumed| (consumed, out));
        assert_eq!(got, want);
    }

    /// A bit flip near the end of the stream that makes the decoder read
    /// implicit zero bytes before it has to resume. Resuming used to give
    /// back more input than had been read.
    #[test]
    fn resume_after_reading_past_the_end() {
        let c = [
            0xed, 0xc0, 0x31, 0x0d, 0x00, 0x00, 0x0c, 0x02, 0xb0, 0x63, 0x16, 0xf0, 0xaf, 0x75,
            0x36, 0x20, 0x69, 0x13, 0x00, 0x0a, 0x18, 0x2c, 0x78,
        ];
        assert_resumed_errors_match(Format::Deflate, &c, 2104 + 5000);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Stopping before a literal, a match or a stored block and carrying
        /// on in a bigger buffer gives the same output as one big buffer.
        #[test]
        fn resumed_decode_matches(data in data(), level in 0i32..=12, f in 0usize..3, guess in 0usize..64) {
            let format = [Format::Deflate, Format::Zlib, Format::Gzip][f];
            let c = reference_compress(format, level, &data);
            prop_assert_eq!(decompress_from_guess(format, &c, data.len(), guess), Ok(data.clone()));
            if !data.is_empty() {
                prop_assert_eq!(
                    decompress_from_guess(format, &c, data.len() - 1, guess),
                    Err(DecompressError::InsufficientSpace)
                );
            }
        }

        /// Corrupt input gives the same error whether or not the decode had
        /// to resume along the way.
        #[test]
        fn resumed_decode_errors_match(data in data(), level in 0i32..=12, f in 0usize..3, flip in any::<prop::sample::Index>(), bit in 0u8..8) {
            let format = [Format::Deflate, Format::Zlib, Format::Gzip][f];
            let mut c = reference_compress(format, level, &data);
            let i = flip.index(c.len());
            c[i] ^= 1 << bit;
            assert_resumed_errors_match(format, &c, data.len() + 5000);
        }

        /// Measuring a stream finds the same end and length as decoding it,
        /// and ignores what follows.
        #[test]
        fn measure_matches_decode(data in data(), level in 0i32..=12, f in 0usize..3, tail in prop::collection::vec(any::<u8>(), 0..16)) {
            let format = [Format::Deflate, Format::Zlib, Format::Gzip][f];
            let mut c = reference_compress(format, level, &data);
            let n = c.len();
            c.extend(tail);
            let m = Decompressor::new(format).measure(&c);
            prop_assert_eq!(m, Ok(Measured { consumed: n, len: data.len() }));
        }

        /// Measuring corrupt input fails the same way as decoding it, except
        /// that only the gzip size field is checked, not the checksums.
        #[test]
        fn measure_errors_match_decode(data in data(), level in 0i32..=12, f in 0usize..3, flip in any::<prop::sample::Index>(), bit in 0u8..8) {
            let format = [Format::Deflate, Format::Zlib, Format::Gzip][f];
            let mut c = reference_compress(format, level, &data);
            let i = flip.index(c.len());
            c[i] ^= 1 << bit;
            // Measuring has no output limit, so the decode needs room for
            // anything the corrupt stream could expand to, or it would stop
            // with InsufficientSpace before reaching the error that measuring
            // finds. A 258-byte match costs at least 2 bits, so DEFLATE
            // expands by at most 1032x.
            let mut out = vec![0; c.len() * 1032];
            let want = Decompressor::new(format).decompress_into(&c, &mut out);
            let got = Decompressor::new(format).measure(&c);
            match want {
                Ok(d) => prop_assert_eq!(got, Ok(Measured { consumed: d.consumed, len: d.written })),
                Err(DecompressError::ChecksumMismatch) => prop_assert!(
                    matches!(got, Ok(_) | Err(DecompressError::ChecksumMismatch)),
                    "{:?}", got
                ),
                Err(e) => prop_assert_eq!(got, Err(e)),
            }
        }
    }
}
