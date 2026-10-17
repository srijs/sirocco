//! Fast DEFLATE, zlib and gzip decompression in pure Rust.
//!
//! sirocco decodes with the same fast core whether it gets the whole input
//! at once or piece by piece.
//!
//! For a one-off call, use [`decompress`]:
//!
//! ```
//! use sirocco::Format;
//!
//! // "hello hello hello" as a raw DEFLATE stream.
//! let compressed = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00];
//! let data = sirocco::decompress(Format::Deflate, &compressed, 1 << 20)?;
//! assert_eq!(data, b"hello hello hello");
//! # Ok::<(), sirocco::DecompressError>(())
//! ```
//!
//! To decompress many buffers, reuse a [`Decompressor`]: it holds decode
//! tables that are costly to set up. [`Decompressor::decompress_into`] writes
//! into a buffer you provide, which also works without an allocator.
//!
//! When the input arrives in pieces, or the output is too large to hold at
//! once, use [`StreamDecompressor`], or [`DecompressReader`] to read
//! decompressed data from any [`std::io::BufRead`]:
//!
//! ```
//! use std::io::Read;
//!
//! # let compressed = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00];
//! let mut reader = sirocco::DecompressReader::new(sirocco::Format::Deflate, &compressed[..]);
//! let mut data = String::new();
//! reader.read_to_string(&mut data)?;
//! assert_eq!(data, "hello hello hello");
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! To find where a stream ends and how long its output is without
//! decompressing it into memory, for example to walk a git pack, use
//! [`Decompressor::measure`] or, for input in pieces, [`StreamMeasurer`].
//!
//! # Features
//!
//! - `std` (default): implements `std` integrations and enables `alloc`.
//! - `alloc` (default, via `std`): everything that returns a `Vec`, and
//!   [`StreamDecompressor`]. Without
//!   it, checksums and [`Decompressor::decompress_into`] still work.

#![no_std]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

#[cfg(feature = "alloc")]
extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

mod checksum;
mod decompress;
mod format;

pub use checksum::{adler32, crc32};
#[cfg(feature = "std")]
pub use decompress::DecompressReader;
pub use decompress::{DecompressError, Decompressed, Decompressor, Measured};
#[cfg(feature = "alloc")]
pub use decompress::{
    MeasureProgress, StreamDecompressor, StreamMeasurer, StreamProgress, StreamStatus,
};
pub use format::Format;

// Compile and run the README's examples as doctests.
#[cfg(all(doctest, feature = "std"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

/// Decompresses all of `input` into a new `Vec` of at most `max_len` bytes.
///
/// See [`Decompressor::decompress`] for the details. This sets up a fresh
/// [`Decompressor`] each time; reuse one to decompress many buffers.
#[cfg(feature = "alloc")]
pub fn decompress(
    format: Format,
    input: &[u8],
    max_len: usize,
) -> Result<alloc::vec::Vec<u8>, DecompressError> {
    Decompressor::new(format).decompress(input, max_len)
}
