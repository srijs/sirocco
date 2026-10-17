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
//! # Ok::<(), sirocco::inflate::Error>(())
//! ```
//!
//! Everything else is in [`inflate`]:
//!
//! - [`inflate::Decoder`] decompresses into your buffer, from input in one
//!   piece or many, without allocating. Reuse it: its tables are costly to
//!   set up.
//! - [`inflate::Reader`] reads from any [`std::io::BufRead`]:
//!
//! ```
//! use std::io::Read;
//! use sirocco::{inflate, Format};
//!
//! # let compressed = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00];
//! let mut reader = inflate::Reader::new(Format::Deflate, &compressed[..]);
//! let mut data = String::new();
//! reader.read_to_string(&mut data)?;
//! assert_eq!(data, "hello hello hello");
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! - [`inflate::Scanner`] finds a stream's end and output length without
//!   writing it, for example to walk a git pack.
//!
//! # Features
//!
//! - `std` (default): [`inflate::Reader`] and `std::io` integrations;
//!   enables `alloc`.
//! - `alloc`: [`decompress`]. Without it, [`inflate::Decoder`] and
//!   [`inflate::Scanner`] still work.

#![no_std]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

#[cfg(feature = "alloc")]
extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

mod checksum;
mod format;
pub mod inflate;

pub use format::Format;

// Compile and run the README's examples as doctests.
#[cfg(all(doctest, feature = "std"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

/// Decompresses all of `input` into a new `Vec` of at most `max_len` bytes.
///
/// `input` must be one stream, else
/// [`TrailingData`](inflate::Error::TrailingData); for gzip, all members are
/// decompressed, as `gzip -d` does. Longer output than `max_len` is
/// [`InsufficientSpace`](inflate::Error::InsufficientSpace).
///
/// It sets up decode tables each call; for many buffers, reuse an
/// [`inflate::Decoder`].
#[cfg(feature = "alloc")]
pub fn decompress(
    format: Format,
    input: &[u8],
    max_len: usize,
) -> Result<alloc::vec::Vec<u8>, inflate::Error> {
    inflate::decompress(format, input, max_len)
}
