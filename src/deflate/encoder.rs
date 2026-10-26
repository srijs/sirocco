//! [`Encoder`] and the state machine behind it.

use super::block::MAX_BLOCK_LENGTH;
use super::parse::{Params, Parser, Strategy, Tables, LOOKAHEAD};
use super::tables::WINDOW;
use super::write::Writer;
use super::{Flush, Level};
use crate::Format;

/// The least buffer [`Encoder::make_room`] can always free some of: the
/// window, the block being built, and the lookahead.
pub(super) const MIN_BUF_LEN: usize = WINDOW + MAX_BLOCK_LENGTH + LOOKAHEAD + 1;

/// Where a stream is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Nothing done yet: the header is next.
    Header,
    /// Compressing.
    Body,
    /// A sync or full flush's block is written: its marker is next.
    Marker {
        full: bool,
    },
    /// The last block is written: the trailer is next.
    Trailer,
    /// The trailer is being written.
    Finishing,
    Done,
}

/// Defines [`Encoder`]: with the `alloc` feature its tables default to the
/// `Box<[u16]>` that [`Encoder::new`] allocates, and without it there's no
/// default. Bare `Encoder` only exists with the feature, which keeps it
/// additive.
macro_rules! encoder {
    ($(#[$attr:meta])* pub struct Encoder { $($fields:tt)* }) => {
        #[cfg(feature = "alloc")]
        $(#[$attr])*
        pub struct Encoder<T = alloc::boxed::Box<[u16]>> { $($fields)* }
        #[cfg(not(feature = "alloc"))]
        $(#[$attr])*
        pub struct Encoder<T> { $($fields)* }
    };
}

encoder! {
/// A DEFLATE, zlib or gzip encoder that reads from a buffer you own.
///
/// [`compress`](Self::compress) compresses what is new in the buffer,
/// writing to an output buffer of any size, and carries on from there in the
/// next call. With all of the input and enough room, it compresses in one
/// call:
///
/// ```
/// use sirocco::{deflate::{Encoder, Flush, Level}, Format};
///
/// let data = b"hello hello hello";
/// let mut e = Encoder::new(Format::Zlib, Level::DEFAULT);
/// let mut out = [0; 64];
/// let p = e.compress(data, &mut out, Flush::Finish);
/// assert!(p.done);
/// assert_eq!(sirocco::decompress(Format::Zlib, p.output, 1 << 20)?, data);
/// # Ok::<(), sirocco::inflate::Error>(())
/// ```
///
/// Most of its memory is its tables, [`TABLES_LEN`](super::TABLES_LEN)
/// `u16`s (about 340 KiB), for finding matches and holding the block being
/// built. They are kept apart from the encoder, which is a few KiB, so that
/// they are never on the stack: [`new`](Encoder::new) (with the `alloc`
/// feature) allocates them zeroed, and nothing else is ever allocated;
/// [`with_tables`](Self::with_tables) takes memory of your own. Reuse an
/// encoder across streams: once a stream is done, the next call starts a new
/// one.
///
/// # The buffer
///
/// The buffer holds the input, which later input is matched against. Each
/// call's buffer must start with all of the last call's buffer, input not
/// yet consumed included, and may have more after it. Its contents aren't
/// checked: changed contents make output that decompresses to something
/// else, which zlib and gzip checksums catch.
///
/// When the buffer fills up, [`make_room`](Self::make_room) moves the input
/// still needed to the front, so that more fits after it. That is at most
/// the last 32 KiB and the block being built, about 100 KiB in all, so a
/// fixed buffer must be at least [`MIN_BUFFER_LEN`](super::MIN_BUFFER_LEN);
/// streaming through one is fastest with 256 KiB or more. A `Vec` that grows
/// works too.
///
/// Without a flush, the encoder holds back the last few hundred bytes of the
/// buffer until more input follows, to look for matches into it.
///
/// ```
/// use sirocco::{deflate::{Encoder, Flush, Level}, Format};
///
/// # let chunks = [&b"hello "[..], b"hello ", b"hello"];
/// let mut e = Encoder::new(Format::Gzip, Level::DEFAULT);
/// let (mut buf, mut len) = (vec![0; 256 << 10], 0);
/// let mut out = [0; 4096];
/// let mut compressed = Vec::new();
/// for chunk in chunks {
///     if buf.len() - len < chunk.len() {
///         len -= e.make_room(&mut buf[..len]);
///     }
///     buf[len..len + chunk.len()].copy_from_slice(chunk);
///     len += chunk.len();
///     loop {
///         let p = e.compress(&buf[..len], &mut out, Flush::None);
///         compressed.extend_from_slice(p.output);
///         if !p.full {
///             break;
///         }
///     }
/// }
/// loop {
///     let p = e.compress(&buf[..len], &mut out, Flush::Finish);
///     compressed.extend_from_slice(p.output);
///     if p.done {
///         break;
///     }
/// }
/// assert_eq!(sirocco::decompress(Format::Gzip, &compressed, 1 << 20)?, b"hello hello hello");
/// # Ok::<(), sirocco::inflate::Error>(())
/// ```
pub struct Encoder {
    s: State,
    tables: T,
}
}

/// All of an encoder but its tables.
struct State {
    format: Format,
    level: Level,
    params: Params,
    stage: Stage,
    parser: Parser,
    w: Writer,
    /// The running Adler-32 or CRC-32 of the input.
    check: u32,
    total_in: u64,
    total_out: u64,
    /// The position of the last sync or full flush, which a repeated one
    /// doesn't add to.
    flushed_at: Option<usize>,
}

impl<T> core::fmt::Debug for Encoder<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Encoder")
            .field("format", &self.s.format)
            .field("level", &self.s.level)
            .field("total_in", &self.s.total_in)
            .field("total_out", &self.s.total_out)
            .finish_non_exhaustive()
    }
}

/// What an [`Encoder::compress`] call did.
#[derive(Debug, PartialEq, Eq)]
#[must_use]
#[non_exhaustive]
pub struct Progress<'o> {
    /// Input bytes consumed by this call. Without a flush, the last few
    /// hundred bytes of the buffer wait for more input.
    pub consumed: usize,
    /// This call's output, at the start of the output buffer.
    pub output: &'o [u8],
    /// The stream is done: its trailer is written. Only with
    /// [`Flush::Finish`].
    pub done: bool,
    /// The output buffer is full and there is more to write: call again,
    /// with the same flush, once there's room.
    pub full: bool,
}

/// The parameters for a level: the fastest found that compress at least as
/// well as zlib's level does, on both the Silesia corpus and server logs
/// (JSON and access logs). Levels 1 to 3 search the hash table lazily, 4 the
/// hash chains greedily, and 5 to 9 lazily, ever deeper.
const fn params(level: Level) -> Params {
    use Strategy::*;
    // Unlimited inserts.
    const ALL: u32 = u32::MAX;
    let (strategy, depth, nice, insert, accel) = match level.0 {
        0 => (Stored, 0, 0, 0, 0),
        1 => (Fast, 1, 32, 0, 12),
        2 => (Fast, 1, 32, 16, 4),
        3 => (Fast, 2, 32, 32, 4),
        4 => (Greedy, 6, 32, 32, 2),
        5 => (Lazy, 6, 32, ALL, 0),
        6 => (Lazy, 16, 30, ALL, 0),
        7 => (Lazy, 24, 48, ALL, 0),
        8 => (Lazy, 35, 65, ALL, 0),
        _ => (Lazy, 48, 96, ALL, 0),
    };
    Params {
        strategy,
        depth,
        nice,
        insert,
        accel,
    }
}

#[cfg(feature = "alloc")]
impl Encoder {
    /// Creates an encoder for `format` at `level`, allocating its tables.
    #[must_use]
    pub fn new(format: Format, level: Level) -> Self {
        // A zeroed allocation, not an array built on the stack and moved.
        let tables = alloc::vec![0; super::TABLES_LEN].into_boxed_slice();
        Encoder::with_tables(format, level, tables)
    }
}

impl<T> Encoder<T> {
    /// Creates an encoder for `format` at `level` that works in `tables`,
    /// which must hold at least [`TABLES_LEN`](super::TABLES_LEN) `u16`s:
    /// a `&mut [u16]`, a `Box<[u16]>` or a `Vec<u16>`, say. Their contents
    /// don't matter.
    ///
    /// [`new`](Encoder::new) is simpler where it works. Taking the tables
    /// apart from the encoder lets them live anywhere without ever being
    /// built on the stack, or needing `unsafe` code to avoid it:
    ///
    /// - Allocated your way: fallibly (with `Vec::try_reserve`), from an
    ///   arena, or in memory mapped some special way.
    /// - Without an allocator: in a `static`, or wherever memory for them
    ///   is.
    /// - Borrowed: from scratch memory used for other things too, or for
    ///   one encoder after another.
    ///
    /// To change format or level, or keep a pool of tables for encoders of
    /// different settings, move one encoder's tables to the next with
    /// [`into_tables`](Self::into_tables), which costs nothing:
    ///
    /// ```
    /// use sirocco::{deflate::{Encoder, Flush, Level}, Format};
    ///
    /// let mut e = Encoder::new(Format::Zlib, Level::new(1));
    /// let mut out = [0; 64];
    /// assert!(e.compress(b"hello hello", &mut out, Flush::Finish).done);
    /// let mut e = Encoder::with_tables(Format::Gzip, Level::new(6), e.into_tables());
    /// assert!(e.compress(b"hello hello", &mut out, Flush::Finish).done);
    /// ```
    ///
    /// Or borrowing them:
    ///
    /// ```
    /// use sirocco::{deflate::{Encoder, Flush, Level, TABLES_LEN}, Format};
    ///
    /// let mut tables = vec![0; TABLES_LEN];
    /// let mut e = Encoder::with_tables(Format::Deflate, Level::DEFAULT, &mut tables[..]);
    /// let mut out = [0; 64];
    /// assert!(e.compress(b"hello hello", &mut out, Flush::Finish).done);
    /// ```
    #[must_use]
    pub const fn with_tables(format: Format, level: Level, tables: T) -> Self {
        Encoder {
            s: State {
                format,
                level,
                params: params(level),
                stage: Stage::Header,
                parser: Parser::new(),
                w: Writer::new(),
                check: initial_check(format),
                total_in: 0,
                total_out: 0,
                flushed_at: None,
            },
            tables,
        }
    }

    /// The format this encoder writes.
    #[must_use]
    pub const fn format(&self) -> Format {
        self.s.format
    }

    /// The level this encoder compresses at.
    #[must_use]
    pub const fn level(&self) -> Level {
        self.s.level
    }

    /// Input bytes consumed by the current (or just finished) stream.
    #[must_use]
    pub fn total_in(&self) -> u64 {
        self.s.total_in
    }

    /// Bytes written by the current (or just finished) stream.
    #[must_use]
    pub fn total_out(&self) -> u64 {
        self.s.total_out
    }

    /// Abandons the current stream. Not needed after one is done.
    pub fn reset(&mut self) {
        self.s.reset();
    }

    /// The tables, giving them back.
    pub fn into_tables(self) -> T {
        self.tables
    }
}

impl<T: AsMut<[u16]>> Encoder<T> {
    /// Compresses the input in `buf` not yet consumed into `out`.
    ///
    /// `buf` must start with the last call's buffer (see [the
    /// buffer](Self#the-buffer)). With [`Flush::None`], the encoder keeps
    /// what it hasn't written yet for later; the other flushes write it
    /// out. Once a flush is asked for, call with it until `full` isn't
    /// set (or, for [`Flush::Finish`], until `done`), adding no input.
    ///
    /// # Panics
    ///
    /// If `buf` is shorter than the input already consumed, or the tables
    /// are shorter than [`TABLES_LEN`](super::TABLES_LEN).
    pub fn compress<'o>(&mut self, buf: &[u8], out: &'o mut [u8], flush: Flush) -> Progress<'o> {
        let mut t = Tables::carve(self.tables.as_mut());
        self.s.compress(&mut t, buf, out, flush)
    }

    /// Makes room in `buf` by moving the input still needed to the front:
    /// the input not yet consumed, and the last 32 KiB and the block being
    /// built of what was. Returns the bytes dropped from the front.
    ///
    /// Pass the part of the buffer the last call had.
    pub fn make_room(&mut self, buf: &mut [u8]) -> usize {
        let mut t = Tables::carve(self.tables.as_mut());
        self.s.make_room(&mut t, buf)
    }
}

impl State {
    fn reset(&mut self) {
        self.stage = Stage::Header;
        self.parser.pos = 0;
        self.parser.open = false;
        self.w.reset();
        self.check = initial_check(self.format);
        self.total_in = 0;
        self.total_out = 0;
        self.flushed_at = None;
    }

    fn compress<'o>(
        &mut self,
        t: &mut Tables<'_>,
        buf: &[u8],
        out: &'o mut [u8],
        flush: Flush,
    ) -> Progress<'o> {
        if self.stage == Stage::Done {
            self.reset();
        }
        let start = self.parser.pos;
        assert!(
            start <= buf.len(),
            "the buffer is shorter than the input already consumed"
        );
        let mut op = 0;
        let full = self.run(t, buf, out, &mut op, flush);
        let consumed = self.parser.pos - start;
        self.total_out += op as u64;
        Progress {
            consumed,
            output: &out[..op],
            done: self.stage == Stage::Done,
            full,
        }
    }

    fn make_room(&mut self, t: &mut Tables<'_>, buf: &mut [u8]) -> usize {
        let p = &mut self.parser;
        let pos = p.pos;
        if !matches!(self.stage, Stage::Body | Stage::Marker { .. }) || pos > buf.len() {
            return 0;
        }
        let pinned = if p.open || !self.w.is_idle() {
            p.block.start
        } else {
            pos
        };
        let drop = pos.saturating_sub(WINDOW).min(pinned);
        if drop == 0 {
            return 0;
        }
        if self.params.strategy != Strategy::Stored {
            p.mf.catch_up(&mut t.lz, pos);
            p.mf.base -= drop;
        }
        buf.copy_within(drop.., 0);
        p.pos -= drop;
        p.block.start -= drop;
        self.flushed_at = self.flushed_at.and_then(|f| f.checked_sub(drop));
        drop
    }

    /// Writes and compresses until done, out of input, or out of room.
    /// Returns whether it stopped for room.
    fn run(
        &mut self,
        t: &mut Tables<'_>,
        buf: &[u8],
        out: &mut [u8],
        op: &mut usize,
        flush: Flush,
    ) -> bool {
        loop {
            if !self.w.drain(&self.parser.block, t.seqs, buf, out, op) {
                return true;
            }
            match self.stage {
                Stage::Header => self.begin_stream(t, buf, flush),
                Stage::Body => {
                    if !self.body(t, buf, flush) {
                        return false;
                    }
                }
                Stage::Marker { full } => {
                    self.w.stage_marker(false);
                    let pos = self.parser.pos;
                    if full {
                        self.reset_matches(t, pos, None);
                    }
                    self.flushed_at = Some(pos);
                    self.stage = Stage::Body;
                }
                Stage::Trailer => {
                    let mut t = [0; 8];
                    let len = match self.format {
                        Format::Deflate => 0,
                        Format::Zlib => {
                            t[..4].copy_from_slice(&self.check.to_be_bytes());
                            4
                        }
                        Format::Gzip => {
                            t[..4].copy_from_slice(&self.check.to_le_bytes());
                            t[4..].copy_from_slice(&(self.total_in as u32).to_le_bytes());
                            8
                        }
                    };
                    self.w.stage_bytes(&t[..len]);
                    self.stage = Stage::Finishing;
                }
                Stage::Finishing => {
                    self.stage = Stage::Done;
                    return false;
                }
                Stage::Done => return false,
            }
        }
    }

    fn begin_stream(&mut self, t: &mut Tables<'_>, buf: &[u8], flush: Flush) {
        // All of the input is here: the tables can be sized to it.
        let len = (flush == Flush::Finish).then_some(buf.len());
        self.reset_matches(t, 0, len);
        let level = self.level.0;
        match self.format {
            Format::Deflate => {}
            Format::Zlib => {
                // As zlib: FLEVEL 0 for levels 0 and 1, 1 below 6, 2 for 6,
                // 3 above.
                let flevel = match level {
                    0 | 1 => 0,
                    2..=5 => 1,
                    6 => 2,
                    _ => 3,
                };
                let header = 0x7800 | flevel << 6;
                let header = header + 31 - header % 31;
                self.w.stage_bytes(&u16::to_be_bytes(header));
            }
            Format::Gzip => {
                // No name, comment or time; XFL as zlib sets it; OS unknown.
                let xfl = match level {
                    9 => 2,
                    0 | 1 => 4,
                    _ => 0,
                };
                self.w
                    .stage_bytes(&[0x1F, 0x8B, 8, 0, 0, 0, 0, 0, xfl, 255]);
            }
        }
        self.stage = Stage::Body;
    }

    fn reset_matches(&mut self, t: &mut Tables<'_>, pos: usize, len: Option<usize>) {
        let s = self.params.strategy;
        if s != Strategy::Stored {
            let ht = if s == Strategy::Fast {
                self.params.depth as usize
            } else {
                0
            };
            self.parser.mf.reset(&mut t.lz, pos, ht, len);
        }
    }

    /// Parses and ends blocks. Returns `false` when it needs more input.
    fn body(&mut self, t: &mut Tables<'_>, buf: &[u8], flush: Flush) -> bool {
        let end = (flush == Flush::Finish).then_some(buf.len());
        let p = self.params;
        if !self.parser.open {
            self.parser.begin_block(p, buf, end);
        }
        let stop = if flush == Flush::None && p.strategy != Strategy::Stored {
            buf.len().saturating_sub(LOOKAHEAD)
        } else {
            buf.len()
        };
        let before = self.parser.pos;
        let ended = self.parser.parse(t, p, buf, stop.max(before), end);
        let pos = self.parser.pos;
        self.update_check(&buf[before..pos]);
        self.total_in += (pos - before) as u64;
        if ended {
            self.end_block(t, flush == Flush::Finish && pos == buf.len());
            return true;
        }
        match flush {
            Flush::None => false,
            Flush::Sync | Flush::Full => {
                if self.flushed_at == Some(pos) {
                    return false;
                }
                self.end_block(t, false);
                self.stage = Stage::Marker {
                    full: flush == Flush::Full,
                };
                true
            }
            Flush::Finish => {
                self.end_block(t, true);
                true
            }
        }
    }

    /// Ends the block being built at the parser's position and starts
    /// writing it, unless it's empty.
    fn end_block(&mut self, t: &mut Tables<'_>, last: bool) {
        let p = &mut self.parser;
        p.open = false;
        let len = p.pos - p.block.start;
        if self.params.strategy == Strategy::Stored {
            if len > 0 {
                self.w.begin_stored(len, last);
            } else if last {
                self.w.stage_marker(true);
            }
        } else if len > 0 || last {
            self.w.begin_block(&mut p.block, t.seqs, len, last);
        }
        if last {
            self.stage = Stage::Trailer;
        }
    }

    fn update_check(&mut self, data: &[u8]) {
        self.check = match self.format {
            Format::Deflate => 0,
            Format::Zlib => crate::checksum::adler32(self.check, data),
            Format::Gzip => crate::checksum::crc32(self.check, data),
        };
    }
}

const fn initial_check(format: Format) -> u32 {
    match format {
        Format::Zlib => 1,
        _ => 0,
    }
}
