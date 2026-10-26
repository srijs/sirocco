//! DEFLATE, zlib and gzip compression.
//!
//! - [`Encoder`] compresses from a buffer you own into another, in one call
//!   or many, without allocating.
//! - [`Writer`] is a [`Write`](std::io::Write) that compresses into another.
//!
//! For a one-off call returning a `Vec`, see [`crate::compress`].
//!
//! Levels are zlib's: 0 stores, 1 is fastest, 9 compresses most, and each
//! compresses about as well as zlib does at that level, in much less time.
//! The output is a standard stream that any inflater reads.
//!
//! The compressor follows libdeflate's: hash chains (or a hash table, at
//! level 1) searched greedily or lazily, blocks of up to 64 KiB ended where
//! the statistics of the data change, and for each block, whichever of a
//! dynamic, static or stored block is smallest.

// Guardrails (and scripts/deflate-size.sh reports the code's size): the
// workspace allows long functions for the decoder's unrolled loops, but the
// encoder's should stay short and simple; and it is safe Rust.
#![warn(clippy::too_many_lines, clippy::cognitive_complexity)]
#![deny(unsafe_code)]

mod block;
mod encoder;
mod huffman;
mod lz;
mod parse;
mod tables;
#[cfg(all(test, feature = "alloc"))]
mod tests;
mod write;
#[cfg(feature = "std")]
mod writer;

pub use encoder::{Encoder, Progress};

/// The `u16`s an [`Encoder`]'s tables hold, about 340 KiB.
pub const TABLES_LEN: usize = parse::TABLES_LEN;

/// The smallest fixed buffer that [`Encoder::make_room`] can always free
/// some of, about 100 KiB.
pub const MIN_BUFFER_LEN: usize = encoder::MIN_BUF_LEN;
#[cfg(feature = "std")]
pub use writer::Writer;

use crate::Format;

/// A compression level, from 0 to 9 as zlib's.
///
/// 0 stores the data uncompressed, 1 is fastest, and 9 compresses most. The
/// default is 6.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Level(u8);

impl Level {
    /// Level 0: stored blocks only.
    pub const NONE: Level = Level(0);
    /// Level 1.
    pub const FASTEST: Level = Level(1);
    /// Level 6, zlib's default.
    pub const DEFAULT: Level = Level(6);
    /// Level 9.
    pub const BEST: Level = Level(9);

    /// Level `level`.
    ///
    /// # Panics
    ///
    /// If `level` is over 9.
    #[must_use]
    pub const fn new(level: u8) -> Self {
        assert!(level <= 9, "compression levels go from 0 to 9");
        Level(level)
    }

    /// The level's number.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for Level {
    fn default() -> Self {
        Level::DEFAULT
    }
}

/// What [`Encoder::compress`] writes out, as zlib's flush modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Flush {
    /// Only what's ready: the encoder decides where blocks end, and holds
    /// back the end of the input for more.
    None,
    /// All of the input so far, ending on a byte boundary with an empty
    /// stored block (`00 00 FF FF`), so that the other end can decompress
    /// all of it. Costs a little compression each time.
    Sync,
    /// As [`Sync`](Flush::Sync), and later data doesn't refer back past this
    /// point, so decompression can start from here. Costs more compression.
    Full,
    /// The rest of the stream, and the trailer.
    Finish,
}

/// The most bytes [`Encoder::compress`] writes for a `len`-byte stream in
/// `format`, given in calls without [`Flush::Sync`] or [`Flush::Full`].
#[must_use]
pub const fn compress_bound(format: Format, len: usize) -> usize {
    // Blocks are at least `MIN_BLOCK_LENGTH` long but for the last, and never
    // longer than as stored blocks, with 5 bytes of header each.
    let blocks = len.div_ceil(block::MIN_BLOCK_LENGTH);
    let blocks = if blocks == 0 { 1 } else { blocks };
    let wrapper = match format {
        Format::Deflate => 0,
        Format::Zlib => 2 + 4,
        Format::Gzip => 10 + 8,
    };
    wrapper + 5 * blocks + len
}

/// See [`crate::compress`].
#[cfg(feature = "alloc")]
pub(crate) fn compress(format: Format, level: Level, input: &[u8]) -> alloc::vec::Vec<u8> {
    let mut e = Encoder::new(format, level);
    let mut out = alloc::vec![0; compress_bound(format, input.len())];
    let p = e.compress(input, &mut out, Flush::Finish);
    assert!(p.done, "the output buffer was smaller than compress_bound");
    let len = p.output.len();
    out.truncate(len);
    out.shrink_to_fit();
    out
}
