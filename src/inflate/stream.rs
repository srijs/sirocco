//! [`Decoder`], [`Scanner`] and the state machine behind both.

use super::decode::{self, Resume, Status};
use super::tables::Tables;
use super::wrapper::{body_start, trailer_len, Check, HeaderParser};
use super::Error;
use crate::Format;

/// How far back a DEFLATE match can reach.
pub(super) const WINDOW: usize = 32 * 1024;

/// Room for input cut off mid-symbol or mid-block-header, which the decoder
/// needs again. The longest is a dynamic block header: 286 bytes (see
/// `carry_holds_the_longest_block_header`).
const CARRY_LEN: usize = 1024;

/// Where a stream is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Header,
    Body,
    Trailer,
    Done,
}

/// What a step of the DEFLATE body did.
enum Step {
    Continue,
    NeedInput,
    Full,
    Done,
}

/// What [`Core::run`] did.
pub(super) struct Run {
    pub(super) consumed: usize,
    /// Where this call's output starts; it ends at [`Core::pos`].
    pub(super) start: usize,
    pub(super) done: bool,
    /// Stopped because the buffer is full.
    pub(super) full: bool,
}

/// Why [`Core::steps`] stopped.
#[derive(PartialEq, Eq)]
enum Stop {
    Done,
    NeedInput,
    Full,
}

/// One stream's state. With `WRITE` it decodes into the caller's buffer;
/// without, it only counts the output.
#[derive(Clone)]
pub(super) struct Core<const WRITE: bool> {
    format: Format,
    tables: Tables,
    stage: Stage,
    resume: Resume,
    /// No body decoded yet, so a call with all of the input can use the
    /// one-shot loop.
    fresh: bool,
    /// With `WRITE`, the output bytes in the buffer. Without, the decoder's
    /// output position, at most [`WINDOW`] between calls.
    pos: usize,
    /// Cut-off input: `carry[..carry_len]`.
    carry: [u8; CARRY_LEN],
    carry_len: usize,
    header: HeaderParser,
    trailer: [u8; 8],
    trailer_len: usize,
    check: Check,
    total_in: u64,
    total_out: u64,
}

impl<const WRITE: bool> Core<WRITE> {
    const fn new(format: Format) -> Self {
        Core {
            format,
            tables: Tables::new(),
            stage: Stage::Header,
            resume: Resume::START,
            fresh: true,
            pos: 0,
            carry: [0; CARRY_LEN],
            carry_len: 0,
            header: HeaderParser::new(),
            trailer: [0; 8],
            trailer_len: 0,
            check: Self::new_check(format),
            total_in: 0,
            total_out: 0,
        }
    }

    const fn new_check(format: Format) -> Check {
        if WRITE {
            Check::new(format)
        } else {
            Check::len_only(format)
        }
    }

    /// Starts a new stream, keeping the tables.
    fn restart(&mut self) {
        self.stage = Stage::Header;
        self.resume = Resume::START;
        self.fresh = true;
        self.pos = 0;
        self.carry_len = 0;
        self.header = HeaderParser::new();
        self.trailer_len = 0;
        self.check = Self::new_check(self.format);
        self.total_in = 0;
        self.total_out = 0;
    }

    /// The output bytes in the buffer.
    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn pos(&self) -> usize {
        self.pos
    }

    /// Decodes `input` into `buf[self.pos..]`; `buf[..self.pos]` holds the
    /// history. Without `WRITE`, `buf` is empty. After a done or failed
    /// stream, starts a new one.
    pub(super) fn run(&mut self, input: &[u8], buf: &mut [u8], end: bool) -> Result<Run, Error> {
        if self.stage == Stage::Done {
            self.restart();
        }
        let start = self.pos;
        let mut ip = 0;
        let r = self.steps(input, &mut ip, buf, end);
        self.total_in += ip as u64;
        match r {
            Ok(stop) => {
                let done = stop == Stop::Done;
                if done {
                    self.stage = Stage::Done;
                }
                Ok(Run {
                    consumed: ip,
                    start,
                    done,
                    full: stop == Stop::Full,
                })
            }
            Err(e) => {
                self.stage = Stage::Done;
                Err(e)
            }
        }
    }

    /// Whether [`run_whole`](Self::run_whole) applies: nothing of this
    /// stream read yet, or the last one is done.
    fn can_run_whole(&self) -> bool {
        self.stage == Stage::Done || (self.stage == Stage::Header && self.total_in == 0)
    }

    /// [`run`](Self::run) for a whole stream in `input`, with the one-shot
    /// loop. If `buf` fills up, `run` carries on from there.
    ///
    /// Not inlined, so the decode loop is inlined here alone: inlined into
    /// `Decoder::decompress`, small streams decoded 5-10% slower.
    #[inline(never)]
    fn run_whole(&mut self, input: &[u8], buf: &mut [u8]) -> Result<Run, Error> {
        let r = self.whole(input, buf);
        if r.is_err() {
            self.stage = Stage::Done;
        }
        r
    }

    #[inline(always)]
    fn whole(&mut self, input: &[u8], buf: &mut [u8]) -> Result<Run, Error> {
        let format = self.format;
        // The header, body and trailer in turn, each from all of the input
        // left, so that an error is found where zlib finds it.
        let start = body_start(format, input)?;
        let out_len = if WRITE { buf.len() } else { usize::MAX / 2 };
        let r = decode::run::<false, WRITE>(
            &mut self.tables,
            &input[start..],
            buf,
            out_len,
            None,
            false,
        )?;
        let (written, used) = match &r {
            Status::Done(consumed, written) => (*written, start + consumed),
            Status::Full(r) => (r.out_pos(), start + r.in_pos()),
        };
        let mut check = Self::new_check(format);
        if format != Format::Deflate {
            if WRITE {
                check.update(&buf[..written]);
            } else {
                check.count(written);
            }
        }
        if let Status::Full(r) = r {
            return Ok(self.carry_on(r, check, used, written));
        }
        let consumed = check.verify_at(input, used)?;
        self.stage = Stage::Done;
        self.pos = written;
        self.total_in = consumed as u64;
        self.total_out = written as u64;
        Ok(Run {
            consumed,
            start: 0,
            done: true,
            full: false,
        })
    }

    /// Sets up [`run`](Self::run) to carry on after `whole` filled the buffer.
    #[cold]
    #[inline(never)]
    fn carry_on(&mut self, r: Resume, check: Check, used: usize, written: usize) -> Run {
        self.restart();
        self.stage = Stage::Body;
        self.fresh = false;
        self.resume = r;
        self.check = check;
        self.pos = written;
        self.total_in = used as u64;
        self.total_out = written as u64;
        Run {
            consumed: used,
            start: 0,
            done: false,
            full: WRITE,
        }
    }

    /// Goes through the stages while there is input and room.
    fn steps(
        &mut self,
        input: &[u8],
        ip: &mut usize,
        buf: &mut [u8],
        end: bool,
    ) -> Result<Stop, Error> {
        loop {
            match self.stage {
                Stage::Header => {
                    if !self.header.parse(self.format, input, ip)? {
                        return need_input(end);
                    }
                    self.stage = Stage::Body;
                }
                Stage::Body => match self.body(input, ip, buf, end)? {
                    Step::Continue => {}
                    Step::NeedInput => return need_input(end),
                    Step::Full => return Ok(Stop::Full),
                    Step::Done => self.stage = Stage::Trailer,
                },
                Stage::Trailer => {
                    let complete = self.collect_trailer(input, ip);
                    self.check.verify(&self.trailer[..self.trailer_len])?;
                    if !complete {
                        return need_input(end);
                    }
                    return Ok(Stop::Done);
                }
                Stage::Done => return Ok(Stop::Done),
            }
        }
    }

    /// Decodes some of the body, from the carry if it holds input, otherwise
    /// from `input[*ip..]`.
    fn body(
        &mut self,
        input: &[u8],
        ip: &mut usize,
        buf: &mut [u8],
        end: bool,
    ) -> Result<Step, Error> {
        if !WRITE {
            // Matches reach back at most `WINDOW`.
            self.pos = self.pos.min(WINDOW);
        }
        // Without writes, pause well before the position overflows (32-bit).
        let out_len = if WRITE { buf.len() } else { usize::MAX / 2 };
        let fresh = core::mem::replace(&mut self.fresh, false);

        if self.carry_len == 0 {
            let rest = &input[*ip..];
            let r = if fresh && end {
                // All input is here: the one-shot loop.
                decode::run::<false, WRITE>(&mut self.tables, rest, buf, out_len, None, false)?
            } else {
                self.resume.set_in_pos(0);
                self.resume.set_out_pos(self.pos);
                let resume = &self.resume;
                decode::run::<true, WRITE>(
                    &mut self.tables,
                    rest,
                    buf,
                    out_len,
                    Some(resume),
                    !end,
                )?
            };
            return Ok(match r {
                Status::Done(consumed, written) => {
                    self.output(buf, written);
                    *ip += consumed;
                    Step::Done
                }
                Status::Full(r) => {
                    self.output(buf, r.out_pos());
                    let pos = r.in_pos();
                    self.resume = r;
                    if self.resume.needs_input() {
                        // Keep the cut-off symbol or block header.
                        let keep = &rest[pos..];
                        if keep.len() > CARRY_LEN {
                            return Err(Error::BadData);
                        }
                        self.carry[..keep.len()].copy_from_slice(keep);
                        self.carry_len = keep.len();
                        *ip = input.len();
                        Step::NeedInput
                    } else {
                        *ip += pos;
                        Self::full()
                    }
                }
            });
        }

        // Decode from the carry plus as much new input as fits. New bytes
        // count as used only once the decoder is past them.
        let old = self.carry_len;
        let rest = &input[*ip..];
        let take = rest.len().min(CARRY_LEN - old);
        self.carry[old..old + take].copy_from_slice(&rest[..take]);
        self.carry_len = old + take;
        let more = !end || take < rest.len();
        self.resume.set_in_pos(0);
        self.resume.set_out_pos(self.pos);
        let r = decode::run::<true, WRITE>(
            &mut self.tables,
            &self.carry[..self.carry_len],
            buf,
            out_len,
            Some(&self.resume),
            more,
        )?;
        Ok(match r {
            Status::Done(consumed, written) => {
                self.output(buf, written);
                self.unload_carry(consumed, old, ip);
                Step::Done
            }
            Status::Full(mut r) => {
                self.output(buf, r.out_pos());
                let pos = r.in_pos();
                if r.needs_input() && pos < old {
                    // Still short of input: keep all of it.
                    self.drain_carry(pos);
                    *ip += take;
                    r.set_in_pos(0);
                    self.resume = r;
                    if take == 0 {
                        if self.carry_len == CARRY_LEN {
                            // Longer than any block header.
                            return Err(Error::BadData);
                        }
                        return Ok(Step::NeedInput);
                    }
                    Step::Continue
                } else {
                    let needs_input = r.needs_input();
                    self.unload_carry(pos, old, ip);
                    self.resume = r;
                    if needs_input {
                        // Past the carry: go on from `input`.
                        Step::Continue
                    } else {
                        Self::full()
                    }
                }
            }
        })
    }

    /// The step after a pause for room. Without writes, that pause only
    /// keeps the position small, so go on.
    const fn full() -> Step {
        if WRITE {
            Step::Full
        } else {
            Step::Continue
        }
    }

    /// Adds the output up to `new_pos` to the checksum and totals.
    fn output(&mut self, buf: &[u8], new_pos: usize) {
        let n = new_pos - self.pos;
        if WRITE {
            self.check.update(&buf[self.pos..new_pos]);
        } else {
            self.check.count(n);
        }
        self.total_out += n as u64;
        self.pos = new_pos;
    }

    /// Drops the first `n` carried bytes.
    fn drain_carry(&mut self, n: usize) {
        self.carry.copy_within(n..self.carry_len, 0);
        self.carry_len -= n;
    }

    /// After decoding the carry up to `pos`: the first `old` bytes were
    /// carried over, the rest copied from `input[*ip..]`. Gives back the
    /// copied bytes the decoder didn't reach.
    fn unload_carry(&mut self, pos: usize, old: usize, ip: &mut usize) {
        if pos >= old {
            *ip += pos - old;
            self.carry_len = 0;
        } else {
            self.carry_len = old;
            self.drain_carry(pos);
        }
    }

    /// Collects the trailer from the carry, then `input`. Returns whether
    /// it is complete.
    fn collect_trailer(&mut self, input: &[u8], ip: &mut usize) -> bool {
        let need = trailer_len(self.format);
        let from_carry = (need - self.trailer_len).min(self.carry_len);
        self.trailer[self.trailer_len..self.trailer_len + from_carry]
            .copy_from_slice(&self.carry[..from_carry]);
        self.trailer_len += from_carry;
        self.drain_carry(from_carry);
        let n = (need - self.trailer_len).min(input.len() - *ip);
        self.trailer[self.trailer_len..self.trailer_len + n].copy_from_slice(&input[*ip..*ip + n]);
        self.trailer_len += n;
        *ip += n;
        self.trailer_len == need
    }
}

/// Out of input: an error only at the end.
fn need_input(end: bool) -> Result<Stop, Error> {
    if end {
        Err(Error::Truncated)
    } else {
        Ok(Stop::NeedInput)
    }
}

/// A DEFLATE, zlib or gzip decoder that writes into a buffer you own.
///
/// With all of the input and room for all of the output,
/// [`decompress`](Self::decompress) decodes in one call. Otherwise each call
/// carries on where the last stopped.
///
/// It holds about 14 KiB inline and never allocates. Reuse it across
/// streams (box it if stack is tight); once a stream is done, the next call
/// starts a new one.
///
/// ```
/// use sirocco::{inflate::Decoder, Format};
///
/// // "hello hello hello" as a raw DEFLATE stream.
/// let compressed = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00];
/// let mut d = Decoder::new(Format::Deflate);
/// let mut buf = [0; 17];
/// let p = d.decompress(&compressed, &mut buf, true)?;
/// assert!(p.done);
/// assert_eq!(p.output, b"hello hello hello");
/// # Ok::<(), sirocco::inflate::Error>(())
/// ```
///
/// # The buffer
///
/// The buffer holds the stream's history, which later data copies from. Each
/// call's buffer must start with what the last call left in it; it may move
/// or grow (a growing `Vec` works). The contents aren't checked: other
/// contents give wrong output, which zlib and gzip checksums catch.
///
/// Use [`Progress::output`] where it lies rather than copying it.
///
/// When [`Progress::full`] is set, the stream needs room: grow the buffer, or
/// call [`make_room`](Self::make_room) to move the last 32 KiB to the front.
/// The decoder never moves anything itself. A fixed buffer must be at least
/// [`MIN_BUFFER_LEN`](super::MIN_BUFFER_LEN), or `make_room` frees nothing;
/// streaming through one is fastest with 256 KiB or more.
///
/// ```
/// use sirocco::{inflate::Decoder, Format};
/// # let compressed = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00];
/// # let mut input = &compressed[..];
/// let mut d = Decoder::new(Format::Deflate);
/// let mut buf = vec![0; 256 << 10];
/// let mut data = Vec::new();
/// loop {
///     let p = d.decompress(input, &mut buf, true)?;
///     data.extend_from_slice(p.output);
///     if p.done {
///         break;
///     }
///     let full = p.full;
///     input = &input[p.consumed..];
///     if full {
///         d.make_room(&mut buf);
///     }
/// }
/// assert_eq!(data, b"hello hello hello");
/// # Ok::<(), sirocco::inflate::Error>(())
/// ```
///
/// Collecting into a growing `Vec`, with no copies:
///
/// ```
/// use sirocco::{inflate::Decoder, Format};
/// # let compressed = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00];
/// # let mut input = &compressed[..];
/// let mut d = Decoder::new(Format::Deflate);
/// let (mut out, mut len) = (vec![0; 4096], 0);
/// loop {
///     let p = d.decompress(input, &mut out, true)?;
///     len += p.output.len();
///     if p.done {
///         break;
///     }
///     let full = p.full;
///     input = &input[p.consumed..];
///     if full {
///         out.resize(2 * out.len(), 0);
///     }
/// }
/// out.truncate(len);
/// assert_eq!(out, b"hello hello hello");
/// # Ok::<(), sirocco::inflate::Error>(())
/// ```
#[derive(Clone)]
pub struct Decoder {
    pub(super) core: Core<true>,
}

impl core::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Decoder")
            .field("format", &self.core.format)
            .field("total_in", &self.core.total_in)
            .field("total_out", &self.core.total_out)
            .finish_non_exhaustive()
    }
}

/// What a [`Decoder::decompress`] call did.
#[derive(Debug, PartialEq, Eq)]
#[must_use]
#[non_exhaustive]
pub struct Progress<'b> {
    /// Input bytes used; pass `input[consumed..]` first next time. Before
    /// the stream ends, all input is used unless the buffer filled up.
    pub consumed: usize,
    /// This call's output, in the buffer.
    pub output: &'b [u8],
    /// The stream ended and its checksum (if any) matched. `consumed` stops
    /// at its end.
    ///
    /// If not done, call again with the rest of the input (and more, if it
    /// was all used). With `end_of_input` set, only a full buffer leaves it
    /// not done.
    pub done: bool,
    /// The buffer is full and the stream may need more room: grow it, or
    /// call [`Decoder::make_room`], before the next call.
    pub full: bool,
}

impl Decoder {
    /// Creates a decoder for `format`.
    #[must_use]
    pub const fn new(format: Format) -> Self {
        Decoder {
            core: Core::new(format),
        }
    }

    /// The format this decoder reads.
    #[must_use]
    pub const fn format(&self) -> Format {
        self.core.format
    }

    /// Abandons the current stream. Not needed after one is done.
    pub fn reset(&mut self) {
        self.core.restart();
    }

    /// Input bytes used by the current (or just finished) stream.
    #[must_use]
    pub fn total_in(&self) -> u64 {
        self.core.total_in
    }

    /// Bytes decompressed by the current (or just finished) stream.
    #[must_use]
    pub fn total_out(&self) -> u64 {
        self.core.total_out
    }

    /// Decompresses from `input` into `buf`.
    ///
    /// `end_of_input` says no input follows; a stream cut off then is
    /// [`Error::Truncated`]. `buf` must start with what the last call left
    /// in it (see [the buffer](Self#the-buffer)).
    ///
    /// An error in the data ends the stream. [`Error::BufferFull`] doesn't:
    /// make room and call again.
    pub fn decompress<'b>(
        &mut self,
        input: &[u8],
        buf: &'b mut [u8],
        end_of_input: bool,
    ) -> Result<Progress<'b>, Error> {
        let core = &mut self.core;
        if end_of_input && core.can_run_whole() {
            let r = core.run_whole(input, buf)?;
            let buf: &'b [u8] = buf;
            return Ok(Progress {
                consumed: r.consumed,
                output: &buf[..core.pos],
                done: r.done,
                full: core.stage == Stage::Body && core.pos == buf.len(),
            });
        }
        if core.stage == Stage::Done {
            core.restart();
        }
        if core.pos > buf.len() {
            return Err(Error::BufferFull);
        }
        let r = core.run(input, buf, end_of_input)?;
        if r.full && r.consumed == 0 && r.start == core.pos {
            // No progress without room: don't let a loop spin.
            return Err(Error::BufferFull);
        }
        let buf: &'b [u8] = buf;
        Ok(Progress {
            consumed: r.consumed,
            output: &buf[r.start..core.pos],
            done: r.done,
            full: core.stage == Stage::Body && core.pos == buf.len(),
        })
    }

    /// Makes room in `buf` by moving the last 32 KiB of output to the front,
    /// which is all the history the stream needs. Returns the bytes dropped
    /// from the front: 0 if the output is no longer than that, or the stream
    /// isn't in its body.
    ///
    /// Call it with the buffer of the last call, when [`Progress::full`]
    /// was set, instead of growing the buffer.
    pub fn make_room(&mut self, buf: &mut [u8]) -> usize {
        let core = &mut self.core;
        let pos = core.pos;
        if core.stage != Stage::Body || pos <= WINDOW || pos > buf.len() {
            return 0;
        }
        buf.copy_within(pos - WINDOW..pos, 0);
        core.pos = WINDOW;
        pos - WINDOW
    }
}

/// Finds where a stream ends and how long its output is, without writing
/// the output.
///
/// Every symbol is decoded, so bad data is rejected, but nothing is copied.
/// The zlib and gzip checksums can't be checked; the gzip size is. Use it to
/// walk concatenated streams, like a git pack, or to size a buffer.
///
/// It never allocates. Once a stream is done, the next call starts a new one.
///
/// ```
/// use sirocco::{inflate::Scanner, Format};
///
/// // "hello hello hello" as a raw DEFLATE stream, then a trailing byte.
/// let input = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00, 0xff];
/// let mut s = Scanner::new(Format::Deflate);
/// assert_eq!(s.scan(&input, true)?, Some(10));
/// assert_eq!((s.total_in(), s.total_out()), (10, 17));
/// # Ok::<(), sirocco::inflate::Error>(())
/// ```
#[derive(Clone)]
pub struct Scanner {
    core: Core<false>,
}

impl core::fmt::Debug for Scanner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Scanner")
            .field("format", &self.core.format)
            .field("total_in", &self.core.total_in)
            .field("total_out", &self.core.total_out)
            .finish_non_exhaustive()
    }
}

/// A scanned stream's lengths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Lengths {
    /// Input bytes, including any header and trailer.
    pub compressed: u64,
    /// Output bytes.
    pub decompressed: u64,
}

impl Scanner {
    /// Creates a scanner for `format`.
    #[must_use]
    pub const fn new(format: Format) -> Self {
        Scanner {
            core: Core::new(format),
        }
    }

    /// The format this scanner reads.
    #[must_use]
    pub const fn format(&self) -> Format {
        self.core.format
    }

    /// Abandons the current stream.
    pub fn reset(&mut self) {
        self.core.restart();
    }

    /// Input bytes used by the current (or just finished) stream.
    #[must_use]
    pub fn total_in(&self) -> u64 {
        self.core.total_in
    }

    /// Output bytes of the current (or just finished) stream so far.
    #[must_use]
    pub fn total_out(&self) -> u64 {
        self.core.total_out
    }

    /// Decodes `input` without writing the output.
    ///
    /// `None`: all of `input` was used; pass more. `Some(n)`: the stream
    /// ended after `n` bytes, and [`total_in`](Self::total_in) and
    /// [`total_out`](Self::total_out) are its lengths.
    ///
    /// `end_of_input` says no input follows; a stream cut off then is
    /// [`Error::Truncated`]. An error ends the stream.
    pub fn scan(&mut self, input: &[u8], end_of_input: bool) -> Result<Option<usize>, Error> {
        let r = self.core.run(input, &mut [], end_of_input)?;
        debug_assert!(r.done || r.consumed == input.len());
        Ok(r.done.then_some(r.consumed))
    }

    /// Reads one stream from `reader`, consuming exactly its bytes, and
    /// returns its lengths. Abandons any stream in progress.
    ///
    /// Decode errors are [`InvalidData`](std::io::ErrorKind::InvalidData)
    /// (wrapping an [`Error`]), or
    /// [`UnexpectedEof`](std::io::ErrorKind::UnexpectedEof) if the input ends
    /// early. `reader`'s errors pass through.
    #[cfg(feature = "std")]
    pub fn scan_reader<R: std::io::BufRead>(&mut self, mut reader: R) -> std::io::Result<Lengths> {
        self.reset();
        loop {
            let input = reader.fill_buf()?;
            let eof = input.is_empty();
            let r = self.scan(input, eof);
            let consumed = match r? {
                Some(n) => {
                    reader.consume(n);
                    return Ok(Lengths {
                        compressed: self.total_in(),
                        decompressed: self.total_out(),
                    });
                }
                None => input.len(),
            };
            reader.consume(consumed);
        }
    }
}
