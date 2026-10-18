//! DEFLATE, zlib and gzip decompression.
//!
//! - [`Decoder`] decompresses into your buffer, from input in one piece or
//!   many, without allocating.
//! - [`Scanner`] finds a stream's end and output length without writing it.
//! - [`Reader`] is a [`Read`](std::io::Read) over a
//!   [`BufRead`](std::io::BufRead).
//!
//! For a one-off call returning a `Vec`, see [`crate::decompress`].

mod decode;
#[cfg(feature = "std")]
mod reader;
mod stream;
mod tables;
#[cfg(all(test, feature = "alloc"))]
mod tests;
mod wrapper;

#[cfg(feature = "std")]
pub use reader::Reader;
pub use stream::{Decoder, Lengths, Progress, Scanner};

#[cfg(feature = "alloc")]
use alloc::vec::Vec;
use core::fmt;

#[cfg(feature = "alloc")]
use crate::Format;
#[cfg(feature = "alloc")]
use decode::Status;
#[cfg(feature = "alloc")]
use tables::Tables;
#[cfg(feature = "alloc")]
use wrapper::{body_bounds, Check};

/// Why decompression failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Error {
    /// Not valid data in the expected format.
    BadData,
    /// The zlib or gzip trailer doesn't match the output.
    ChecksumMismatch,
    /// The output would exceed [`crate::decompress`]'s limit.
    InsufficientSpace,
    /// Bytes follow the stream (only from [`crate::decompress`]).
    TrailingData,
    /// The input ended before the stream did.
    Truncated,
    /// A zlib stream needs a preset dictionary, which isn't supported.
    PresetDictionary,
    /// [`Decoder::decompress`]'s buffer has no room for the stream to go
    /// on: grow it or [make room](Decoder::make_room). A buffer shorter than
    /// the output it should hold is this too.
    BufferFull,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::BadData => "invalid compressed data",
            Error::ChecksumMismatch => "checksum mismatch in compressed data",
            Error::InsufficientSpace => "decompressed data longer than the limit",
            Error::TrailingData => "unexpected data after the compressed stream",
            Error::Truncated => "compressed data ended early",
            Error::PresetDictionary => "zlib stream needs a preset dictionary",
            Error::BufferFull => "the buffer has no room for the stream to go on",
        })
    }
}

impl core::error::Error for Error {}

/// [`Truncated`](Error::Truncated) becomes
/// [`UnexpectedEof`](std::io::ErrorKind::UnexpectedEof),
/// [`BufferFull`](Error::BufferFull)
/// [`InvalidInput`](std::io::ErrorKind::InvalidInput), and the rest
/// [`InvalidData`](std::io::ErrorKind::InvalidData), wrapping the [`Error`].
#[cfg(feature = "std")]
impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        let kind = match e {
            Error::Truncated => std::io::ErrorKind::UnexpectedEof,
            Error::BufferFull => std::io::ErrorKind::InvalidInput,
            _ => std::io::ErrorKind::InvalidData,
        };
        std::io::Error::new(kind, e)
    }
}

/// See [`crate::decompress`].
#[cfg(feature = "alloc")]
pub(crate) fn decompress(format: Format, input: &[u8], max_len: usize) -> Result<Vec<u8>, Error> {
    VecDecoder {
        format,
        tables: Tables::new(),
    }
    .decompress(input, max_len)
}

/// Decompresses whole buffers into a `Vec` that grows as needed.
#[cfg(feature = "alloc")]
struct VecDecoder {
    format: Format,
    tables: Tables,
}

#[cfg(feature = "alloc")]
impl VecDecoder {
    /// Decompresses all of `input` into a new `Vec` of at most `max_len`
    /// bytes. A gzip file's members are all decompressed and concatenated.
    fn decompress(&mut self, input: &[u8], max_len: usize) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        let mut rest = input;
        loop {
            let consumed = self.decompress_append(rest, &mut out, max_len)?;
            rest = &rest[consumed..];
            if rest.is_empty() {
                return Ok(out);
            }
            if self.format != Format::Gzip {
                return Err(Error::TrailingData);
            }
        }
    }

    /// Decompresses the stream at the start of `input`, appending to `out`
    /// without letting it grow past `max_len`. Returns the input consumed.
    fn decompress_append(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
        max_len: usize,
    ) -> Result<usize, Error> {
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
    fn decompress_append_from(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
        max_len: usize,
        guess: usize,
    ) -> Result<usize, Error> {
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
    fn inflate_growing(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
        start: usize,
        max_len: usize,
        guess: usize,
    ) -> Result<usize, Error> {
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
            match decode::run::<true, true>(
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
                    return Err(Error::InsufficientSpace);
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
