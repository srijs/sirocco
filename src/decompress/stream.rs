//! Streaming decompression: input and output in pieces of any size.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::inflate::{self, Resume, Status};
use super::tables::Tables;
use super::wrapper::{trailer_len, Check, HeaderParser};
use super::DecompressError;
use crate::Format;

/// How far back a DEFLATE match can reach.
const WINDOW: usize = 32 * 1024;

/// The internal output buffer: the last [`WINDOW`] bytes of output, then room
/// to decode into before copying out.
const OUT_BUF_LEN: usize = 256 * 1024;

/// The room to leave for decoding before sliding the window back: more than
/// the largest stored block, so that every symbol and block fits.
const MIN_ROOM: usize = 66 * 1024;

/// The most input a pause can leave unconsumed: a stored block (up to 65535
/// bytes and its 4-byte header) or a Huffman block header (a few hundred
/// bytes), plus a little slack.
const CARRY_LEN: usize = 64 * 1024 + 1024;

/// Where a [`StreamDecompressor`] is in the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamStatus {
    /// The stream ended and its checksum (if any) matched. Further calls
    /// write nothing until [`reset`](StreamDecompressor::reset).
    Done,
    /// All of the input was consumed; pass more.
    NeedInput,
    /// The output buffer is full; pass more room.
    NeedOutput,
}

/// The result of [`StreamDecompressor::decompress`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct StreamProgress {
    /// Bytes of input used. Bytes after the end of the stream are never
    /// consumed.
    pub consumed: usize,
    /// Bytes written to the start of the output buffer.
    pub written: usize,
    /// Why the call returned.
    pub status: StreamStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Header,
    Body,
    Trailer,
    Done,
}

/// A DEFLATE, zlib or gzip decompressor that takes its input and gives its
/// output in pieces, for data that doesn't fit in memory or arrives over
/// time.
///
/// Call [`decompress`](Self::decompress) with whatever input is available
/// and an output buffer, and act on the returned [`StreamStatus`]. A call
/// that gets the whole stream and enough room for all of the output decodes
/// straight into the output buffer, as fast as
/// [`Decompressor::decompress_into`]. Otherwise output goes through an
/// internal 256 KiB buffer that also holds the 32 KiB of history that later
/// data can refer back to.
///
/// One gzip member is read per stream; call [`reset`](Self::reset) to read
/// the next one.
///
/// ```
/// use sirocco::{Format, StreamDecompressor, StreamStatus};
///
/// // "hello hello hello" as a raw DEFLATE stream, fed a byte at a time.
/// let compressed = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00];
/// let mut d = StreamDecompressor::new(Format::Deflate);
/// let mut out = Vec::new();
/// let mut buf = [0; 4];
/// let mut input = &compressed[..];
/// loop {
///     let chunk = &input[..input.len().min(1)];
///     let p = d.decompress(chunk, &mut buf, input.is_empty())?;
///     input = &input[p.consumed..];
///     out.extend_from_slice(&buf[..p.written]);
///     if p.status == StreamStatus::Done {
///         break;
///     }
/// }
/// assert_eq!(out, b"hello hello hello");
/// # Ok::<(), sirocco::DecompressError>(())
/// ```
///
/// [`Decompressor::decompress_into`]: crate::Decompressor::decompress_into
pub struct StreamDecompressor {
    format: Format,
    tables: Box<Tables>,
    /// Counts the output instead of writing it, for [`StreamMeasurer`]. Then
    /// `out_buf` stays empty and `out_filled` is the output position the
    /// decoder sees, which only needs to reach back as far as a match can.
    measure: bool,
    stage: Stage,
    /// Where decoding carries on. Its input position is set before each
    /// decode; its output position indexes `out_buf`.
    resume: Resume,
    /// Whether the body may still decode straight into the caller's output:
    /// true until its first pause.
    direct: bool,
    /// Recent output (the window) and decoded output not yet copied out.
    out_buf: Vec<u8>,
    /// `out_buf[out_read..out_filled]` is waiting to be copied out.
    out_read: usize,
    out_filled: usize,
    /// Where in this call's output the bytes written straight to it start,
    /// when there are some that the window in `out_buf` doesn't have yet.
    /// They are added before decoding into `out_buf` and at the end of the
    /// call.
    window_from: Option<usize>,
    /// Consumed input the decoder still needs: the start of a symbol or
    /// block that was cut off at the end of an earlier call's input, or
    /// (after the body) the start of the trailer.
    carry: Vec<u8>,
    header: HeaderParser,
    /// Trailer bytes collected so far.
    trailer: [u8; 8],
    trailer_len: usize,
    check: Check,
    total_in: u64,
    total_out: u64,
}

impl core::fmt::Debug for StreamDecompressor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StreamDecompressor")
            .field("format", &self.format)
            .field("total_in", &self.total_in)
            .field("total_out", &self.total_out)
            .finish_non_exhaustive()
    }
}

impl StreamDecompressor {
    /// Creates a streaming decompressor for `format`. Its buffers are
    /// allocated when first needed.
    #[must_use]
    pub fn new(format: Format) -> Self {
        Self::with_mode(format, false)
    }

    fn with_mode(format: Format, measure: bool) -> Self {
        let mut s = StreamDecompressor {
            format,
            tables: Box::new(Tables::new()),
            measure,
            stage: Stage::Header,
            resume: Resume::start(),
            direct: true,
            out_buf: Vec::new(),
            out_read: 0,
            out_filled: 0,
            window_from: None,
            carry: Vec::new(),
            header: HeaderParser::new(),
            trailer: [0; 8],
            trailer_len: 0,
            check: Check::new(format),
            total_in: 0,
            total_out: 0,
        };
        s.reset();
        s
    }

    /// The format this decompressor reads.
    #[must_use]
    pub fn format(&self) -> Format {
        self.format
    }

    /// Gets ready for a new stream, keeping the buffers.
    pub fn reset(&mut self) {
        self.stage = Stage::Header;
        self.resume = Resume::start();
        self.direct = true;
        self.out_read = 0;
        self.out_filled = 0;
        self.window_from = None;
        self.carry.clear();
        self.header = HeaderParser::new();
        self.trailer_len = 0;
        self.check = if self.measure {
            Check::len_only(self.format)
        } else {
            Check::new(self.format)
        };
        self.total_in = 0;
        self.total_out = 0;
    }

    /// Input bytes consumed since the stream started.
    #[must_use]
    pub fn total_in(&self) -> u64 {
        self.total_in
    }

    /// Output bytes written since the stream started.
    #[must_use]
    pub fn total_out(&self) -> u64 {
        self.total_out
    }

    /// Decompresses from `input` into `output`, returning how much of each
    /// was used.
    ///
    /// `end_of_input` says that no input follows `input`. Until it is set, a
    /// stream cut off at the end of `input` waits for more; with it set, that
    /// is a [`Truncated`](DecompressError::Truncated) error.
    pub fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        end_of_input: bool,
    ) -> Result<StreamProgress, DecompressError> {
        let mut ip = 0;
        let mut op = 0;
        let r = self.run(input, &mut ip, output, &mut op, end_of_input);
        self.flush_window(output, op);
        self.total_in += ip as u64;
        self.total_out += op as u64;
        let status = r?;
        Ok(StreamProgress {
            consumed: ip,
            written: op,
            status,
        })
    }

    fn run(
        &mut self,
        input: &[u8],
        ip: &mut usize,
        output: &mut [u8],
        op: &mut usize,
        end: bool,
    ) -> Result<StreamStatus, DecompressError> {
        loop {
            self.count_measured();
            // Copy out pending output first.
            if self.out_read < self.out_filled {
                let n = (self.out_filled - self.out_read).min(output.len() - *op);
                let src = &self.out_buf[self.out_read..self.out_read + n];
                output[*op..*op + n].copy_from_slice(src);
                self.check_update_out(*op, n, output);
                self.out_read += n;
                *op += n;
            }
            if self.out_read < self.out_filled {
                return Ok(StreamStatus::NeedOutput);
            }
            match self.stage {
                Stage::Header => {
                    if !self.header.parse(self.format, input, ip)? {
                        return need_input(end);
                    }
                    self.stage = Stage::Body;
                }
                Stage::Body => {
                    if *op == output.len() && !output.is_empty() {
                        return Ok(StreamStatus::NeedOutput);
                    }
                    match self.body(input, ip, output, op)? {
                        Step::Continue => {}
                        Step::NeedInput => {
                            self.count_measured();
                            return need_input(end);
                        }
                        Step::Done => self.stage = Stage::Trailer,
                    }
                }
                Stage::Trailer => {
                    // `out_read == out_filled` here, so the checksum covers
                    // all of the output.
                    if !self.collect_trailer(input, ip) {
                        return need_input(end);
                    }
                    self.check.verify(&self.trailer[..self.trailer_len])?;
                    self.stage = Stage::Done;
                }
                Stage::Done => return Ok(StreamStatus::Done),
            }
        }
    }

    /// Counts output decoded in measuring mode, which isn't copied out.
    fn count_measured(&mut self) {
        if self.measure {
            let n = self.out_filled - self.out_read;
            self.check.count(n);
            self.total_out += n as u64;
            self.out_read = self.out_filled;
        }
    }

    /// Decodes some of the DEFLATE body.
    fn body(
        &mut self,
        input: &[u8],
        ip: &mut usize,
        output: &mut [u8],
        op: &mut usize,
    ) -> Result<Step, DecompressError> {
        if self.direct && self.carry.is_empty() && !self.measure {
            // Nothing decoded yet, so no history is needed: decode straight
            // into the caller's buffer. A whole stream and enough room
            // finishes here with no copying.
            self.direct = false;
            let out = &mut output[*op..];
            self.resume.set_in_pos(*ip);
            self.resume.set_out_pos(0);
            let len = out.len();
            let r = inflate::run::<true, true>(
                &mut self.tables,
                input,
                out,
                len,
                Some(&self.resume),
                true,
            )?;
            return Ok(match r {
                Status::Done(consumed, written) => {
                    self.check_update_out(*op, written, output);
                    *op += written;
                    *ip = consumed;
                    Step::Done
                }
                Status::Full(r) => {
                    let written = r.out_pos();
                    self.check_update_out(*op, written, output);
                    // The output so far is history for what comes next.
                    self.window_from = Some(*op);
                    *op += written;
                    self.paused(r, input, ip, 0)
                }
            });
        }

        self.alloc_out_buf();
        if !self.measure && self.carry.is_empty() {
            if let Some(left) = self.resume.stored_left() {
                if *op < output.len() {
                    // Inside a stored block: copy it straight from the
                    // input to the caller's output.
                    let n = left.min(input.len() - *ip).min(output.len() - *op);
                    if n == 0 {
                        return Ok(Step::NeedInput);
                    }
                    output[*op..*op + n].copy_from_slice(&input[*ip..*ip + n]);
                    self.check_update_out(*op, n, output);
                    self.window_from.get_or_insert(*op);
                    *ip += n;
                    *op += n;
                    return Ok(if self.resume.skip_stored(n) {
                        Step::Done
                    } else {
                        Step::Continue
                    });
                }
            }
        }
        self.flush_window(output, *op);
        if self.measure {
            // Matches reach back at most `WINDOW`, so that's all the decoder
            // needs to see of the output so far.
            self.out_filled = self.out_filled.min(WINDOW);
            self.out_read = self.out_filled;
        } else if OUT_BUF_LEN - self.out_filled < MIN_ROOM {
            // Everything was copied out: keep only the window.
            self.out_buf
                .copy_within(self.out_filled - WINDOW..self.out_filled, 0);
            self.out_read = WINDOW;
            self.out_filled = WINDOW;
        }
        self.resume.set_out_pos(self.out_filled);
        // Stop at stored blocks, to copy them straight out as above.
        self.resume.set_stop_at_stored(!self.measure);

        if self.carry.is_empty() {
            self.resume.set_in_pos(*ip);
            let r = decode(
                &mut self.tables,
                self.measure,
                input,
                &mut self.out_buf,
                &self.resume,
            )?;
            Ok(match r {
                Status::Done(consumed, written) => {
                    self.out_filled = written;
                    *ip = consumed;
                    Step::Done
                }
                Status::Full(r) => {
                    let out_pos = r.out_pos();
                    self.paused(r, input, ip, out_pos)
                }
            })
        } else {
            // Decode from the carried bytes followed by as much new input
            // as fits. Only new bytes the decoder gets past count as
            // consumed: the rest stay with the caller.
            let old = self.carry.len();
            let take = (input.len() - *ip).min(CARRY_LEN - old);
            self.carry.extend_from_slice(&input[*ip..*ip + take]);
            self.resume.set_in_pos(0);
            let r = decode(
                &mut self.tables,
                self.measure,
                &self.carry,
                &mut self.out_buf,
                &self.resume,
            );
            let r = match r {
                Ok(r) => r,
                Err(e) => {
                    self.carry.truncate(old);
                    return Err(e);
                }
            };
            Ok(match r {
                Status::Done(consumed, written) => {
                    self.out_filled = written;
                    self.unload_carry(consumed, old, ip);
                    Step::Done
                }
                Status::Full(mut r) => {
                    self.out_filled = r.out_pos();
                    let pos = r.in_pos();
                    if r.needs_input() && pos < old {
                        // Still short of input: all of it is needed.
                        self.carry.drain(..pos);
                        *ip += take;
                        r.set_in_pos(0);
                        self.resume = r;
                        if take == 0 {
                            return Ok(Step::NeedInput);
                        }
                        Step::Continue
                    } else {
                        self.unload_carry(pos, old, ip);
                        self.resume = r;
                        Step::Continue
                    }
                }
            })
        }
    }

    /// After decoding from `carry` (whose first `old` bytes were carried
    /// over and the rest copied from `input[*ip..]`) up to `pos`: gives the
    /// copied bytes the decoder didn't reach back to the caller.
    fn unload_carry(&mut self, pos: usize, old: usize, ip: &mut usize) {
        if pos >= old {
            *ip += pos - old;
            self.carry.clear();
        } else {
            self.carry.truncate(old);
            self.carry.drain(..pos);
        }
    }

    /// Handles a pause while decoding from `input` with output position
    /// `out_filled`.
    fn paused(&mut self, mut r: Resume, input: &[u8], ip: &mut usize, out_filled: usize) -> Step {
        self.out_filled = out_filled;
        r.set_out_pos(out_filled);
        let pos = r.in_pos();
        self.resume = r;
        if self.resume.needs_input() {
            // Carry the cut-off symbol or block over to the next call.
            self.carry.extend_from_slice(&input[pos..]);
            *ip = input.len();
            Step::NeedInput
        } else {
            *ip = pos;
            Step::Continue
        }
    }

    /// Adds the output written straight to `output[..op]` since
    /// `window_from` to the window, as far as it may be needed.
    #[inline(always)]
    fn flush_window(&mut self, output: &[u8], op: usize) {
        if let Some(from) = self.window_from.take() {
            // The rest of a stored block comes before any match can refer
            // back, and replaces that much of the window.
            let keep = WINDOW - self.resume.stored_left().unwrap_or(0).min(WINDOW);
            self.add_window(&output[from.max(op.saturating_sub(keep))..op]);
        }
    }

    /// Adds `new`, output written straight to the caller's buffer, to the
    /// window.
    fn add_window(&mut self, new: &[u8]) {
        self.alloc_out_buf();
        if new.len() >= WINDOW {
            self.out_buf[..WINDOW].copy_from_slice(&new[new.len() - WINDOW..]);
            self.out_filled = WINDOW;
        } else {
            if OUT_BUF_LEN - self.out_filled < new.len() {
                // Keep only the window, which is all of `out_buf` that
                // matters once everything was copied out.
                self.out_buf
                    .copy_within(self.out_filled - WINDOW..self.out_filled, 0);
                self.out_filled = WINDOW;
            }
            self.out_buf[self.out_filled..self.out_filled + new.len()].copy_from_slice(new);
            self.out_filled += new.len();
        }
        self.out_read = self.out_filled;
    }

    fn alloc_out_buf(&mut self) {
        if self.measure {
            self.carry.reserve_exact(CARRY_LEN);
        } else if self.out_buf.is_empty() {
            self.out_buf = alloc::vec![0; OUT_BUF_LEN];
            self.carry.reserve_exact(CARRY_LEN);
        }
    }

    /// Moves trailer bytes into `trailer` from `carry` then `input`.
    /// Returns whether it holds all of them.
    fn collect_trailer(&mut self, input: &[u8], ip: &mut usize) -> bool {
        while self.trailer_len < trailer_len(self.format) {
            if !self.carry.is_empty() {
                self.trailer[self.trailer_len] = self.carry.remove(0);
            } else if *ip < input.len() {
                self.trailer[self.trailer_len] = input[*ip];
                *ip += 1;
            } else {
                return false;
            }
            self.trailer_len += 1;
        }
        true
    }

    /// Adds `output[at..at + n]` to the checksum.
    fn check_update_out(&mut self, at: usize, n: usize, output: &[u8]) {
        self.check.update(&output[at..at + n]);
    }
}

/// Finds where a DEFLATE, zlib or gzip stream ends and how many bytes it
/// decompresses to, from input in pieces, without writing any output.
///
/// The streaming counterpart of [`Decompressor::measure`]: every symbol is
/// decoded, so malformed data is rejected, but nothing is copied and no
/// history is kept, so a match costs the same as a literal. The zlib and
/// gzip checksums cover the output, so they can't be checked; the gzip size
/// field is.
///
/// ```
/// use sirocco::{Format, StreamMeasurer};
///
/// // "hello hello hello" as a raw DEFLATE stream, then a trailing byte.
/// let input = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00, 0xff];
/// let mut m = StreamMeasurer::new(Format::Deflate);
/// let len = m.measure_reader(&mut &input[..])?;
/// assert_eq!((m.total_in(), len), (10, 17));
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// [`Decompressor::measure`]: crate::Decompressor::measure
#[derive(Debug)]
pub struct StreamMeasurer {
    s: StreamDecompressor,
}

/// The result of [`StreamMeasurer::measure`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct MeasureProgress {
    /// Bytes of input used. Bytes after the end of the stream are never
    /// consumed.
    pub consumed: usize,
    /// Whether the stream ended: [`total_in`](StreamMeasurer::total_in) and
    /// [`total_out`](StreamMeasurer::total_out) are then its compressed and
    /// decompressed sizes.
    pub done: bool,
}

impl StreamMeasurer {
    /// Creates a streaming measurer for `format`.
    #[must_use]
    pub fn new(format: Format) -> Self {
        StreamMeasurer {
            s: StreamDecompressor::with_mode(format, true),
        }
    }

    /// The format this measurer reads.
    #[must_use]
    pub fn format(&self) -> Format {
        self.s.format()
    }

    /// Gets ready for a new stream.
    pub fn reset(&mut self) {
        self.s.reset();
    }

    /// Input bytes consumed since the stream started.
    #[must_use]
    pub fn total_in(&self) -> u64 {
        self.s.total_in()
    }

    /// Bytes the stream has decompressed to so far.
    #[must_use]
    pub fn total_out(&self) -> u64 {
        self.s.total_out()
    }

    /// Decodes from `input` without writing the output, returning how much
    /// input was used and whether the stream ended.
    ///
    /// `end_of_input` says that no input follows `input`. Until it is set, a
    /// stream cut off at the end of `input` waits for more; with it set, that
    /// is a [`Truncated`](DecompressError::Truncated) error.
    pub fn measure(
        &mut self,
        input: &[u8],
        end_of_input: bool,
    ) -> Result<MeasureProgress, DecompressError> {
        let p = self.s.decompress(input, &mut [], end_of_input)?;
        Ok(MeasureProgress {
            consumed: p.consumed,
            done: p.status == StreamStatus::Done,
        })
    }

    /// Resets, then reads one stream from `reader`, consuming exactly its
    /// bytes so that whatever follows can be read next. Returns the
    /// decompressed size; [`total_in`](Self::total_in) is the compressed
    /// size. Input that ends before the stream does is an
    /// [`UnexpectedEof`](std::io::ErrorKind::UnexpectedEof) error.
    #[cfg(feature = "std")]
    pub fn measure_reader<R: std::io::BufRead>(&mut self, mut reader: R) -> std::io::Result<u64> {
        self.reset();
        loop {
            let input = reader.fill_buf()?;
            let eof = input.is_empty();
            let p = self.measure(input, eof)?;
            reader.consume(p.consumed);
            if p.done {
                return Ok(self.total_out());
            }
        }
    }
}

/// Decompresses a stream read from a [`BufRead`](std::io::BufRead), as a
/// [`Read`](std::io::Read).
///
/// It consumes exactly the compressed stream from the inner reader, so
/// whatever follows it can be read from [`into_inner`](Self::into_inner)'s
/// reader afterwards. Reads return 0 bytes once the stream has ended; input
/// that ends before the stream does is an
/// [`UnexpectedEof`](std::io::ErrorKind::UnexpectedEof) error.
#[cfg(feature = "std")]
#[derive(Debug)]
pub struct DecompressReader<R> {
    inner: R,
    d: StreamDecompressor,
    done: bool,
}

#[cfg(feature = "std")]
impl<R: std::io::BufRead> DecompressReader<R> {
    /// Reads a `format` stream from `inner`.
    pub fn new(format: Format, inner: R) -> Self {
        DecompressReader {
            inner,
            d: StreamDecompressor::new(format),
            done: false,
        }
    }

    /// The streaming decompressor, for its input and output totals.
    pub fn decompressor(&self) -> &StreamDecompressor {
        &self.d
    }

    /// The inner reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// The inner reader, positioned after the compressed stream once it has
    /// ended.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

#[cfg(feature = "std")]
impl<R: std::io::BufRead> std::io::Read for DecompressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        loop {
            let input = self.inner.fill_buf()?;
            let eof = input.is_empty();
            // `Truncated` becomes `UnexpectedEof`.
            let p = self.d.decompress(input, buf, eof)?;
            self.inner.consume(p.consumed);
            match p.status {
                StreamStatus::Done => {
                    self.done = true;
                    return Ok(p.written);
                }
                _ if p.written > 0 => return Ok(p.written),
                _ => {}
            }
        }
    }
}

/// Decodes from `input` with more input to come, carrying on from `resume`:
/// into `out_buf`, or with `measure` counting the output instead.
fn decode(
    t: &mut Tables,
    measure: bool,
    input: &[u8],
    out_buf: &mut [u8],
    resume: &Resume,
) -> Result<Status, DecompressError> {
    if measure {
        // Pause well short of overflowing the output position (on 32-bit);
        // the next call starts from at most `WINDOW` again.
        inflate::run::<true, false>(t, input, &mut [], usize::MAX / 2, Some(resume), true)
    } else {
        let len = out_buf.len();
        inflate::run::<true, true>(t, input, out_buf, len, Some(resume), true)
    }
}

/// Out of input: fine unless it was the end.
fn need_input(end: bool) -> Result<StreamStatus, DecompressError> {
    if end {
        Err(DecompressError::Truncated)
    } else {
        Ok(StreamStatus::NeedInput)
    }
}

enum Step {
    /// Made progress; go round again.
    Continue,
    NeedInput,
    /// The DEFLATE stream ended.
    Done,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompress::reference_compress;
    use crate::Decompressor;
    use alloc::vec;
    use proptest::prelude::*;

    const FORMATS: [Format; 3] = [Format::Deflate, Format::Zlib, Format::Gzip];

    /// Decompresses `c` through a `StreamDecompressor`, cycling through the
    /// given input and output chunk sizes. Returns the output and the input
    /// consumed.
    fn stream(
        format: Format,
        c: &[u8],
        in_sizes: &[usize],
        out_sizes: &[usize],
    ) -> Result<(Vec<u8>, usize), DecompressError> {
        let mut d = StreamDecompressor::new(format);
        let (mut pos, mut out, mut stuck) = (0, Vec::new(), 0);
        let mut space = vec![0; *out_sizes.iter().max().unwrap()];
        for i in 0.. {
            let n = in_sizes[i % in_sizes.len()].min(c.len() - pos);
            let end = pos + n == c.len();
            let buf = &mut space[..out_sizes[i % out_sizes.len()]];
            let p = d.decompress(&c[pos..pos + n], buf, end)?;
            assert!(p.consumed <= n && p.written <= buf.len());
            pos += p.consumed;
            out.extend_from_slice(&buf[..p.written]);
            assert_eq!(d.total_in(), pos as u64);
            assert_eq!(d.total_out(), out.len() as u64);
            match p.status {
                StreamStatus::Done => return Ok((out, pos)),
                StreamStatus::NeedInput => assert!(p.consumed == n, "{p:?}"),
                StreamStatus::NeedOutput => assert!(p.written == buf.len(), "{p:?}"),
            }
            if p.consumed == 0 && p.written == 0 {
                stuck += 1;
                assert!(stuck < 100, "no progress at {pos} of {}: {p:?}", c.len());
            } else {
                stuck = 0;
            }
        }
        unreachable!()
    }

    /// Measures `c` through a `StreamMeasurer`, cycling through the given
    /// input chunk sizes. Returns the output length and the input consumed.
    fn stream_measure(
        format: Format,
        c: &[u8],
        in_sizes: &[usize],
    ) -> Result<(u64, usize), DecompressError> {
        let mut m = StreamMeasurer::new(format);
        let (mut pos, mut stuck) = (0, 0);
        for i in 0.. {
            let n = in_sizes[i % in_sizes.len()].min(c.len() - pos);
            let end = pos + n == c.len();
            let p = m.measure(&c[pos..pos + n], end)?;
            assert!(p.consumed <= n);
            pos += p.consumed;
            assert_eq!(m.total_in(), pos as u64);
            if p.done {
                return Ok((m.total_out(), pos));
            }
            assert!(p.consumed == n, "{p:?}");
            if p.consumed == 0 {
                stuck += 1;
                assert!(stuck < 100, "no progress at {pos} of {}: {p:?}", c.len());
            } else {
                stuck = 0;
            }
        }
        unreachable!()
    }

    /// The single-call result: output and input consumed.
    fn one_shot(format: Format, c: &[u8], len: usize) -> Result<(Vec<u8>, usize), DecompressError> {
        let mut out = vec![0; len];
        let r = Decompressor::new(format).decompress_into(c, &mut out)?;
        out.truncate(r.written);
        Ok((out, r.consumed))
    }

    fn data() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            prop::collection::vec(any::<u8>(), 0..2000),
            // Compressible: short alphabet, long enough for several blocks
            // and matches that reach back across chunk boundaries.
            prop::collection::vec(0u8..4, 0..100_000),
            (any::<u8>(), 0usize..100_000).prop_map(|(b, n)| vec![b; n]),
            // Incompressible runs (stored blocks) each followed by a copy
            // of their end, which Huffman blocks refer back into.
            prop::collection::vec(
                (
                    prop::collection::vec(any::<u8>(), 0..80_000),
                    0usize..33_000
                ),
                1..4
            )
            .prop_map(|runs| {
                let mut v = Vec::new();
                for (run, copy) in runs {
                    v.extend_from_slice(&run);
                    let start = v.len() - copy.min(run.len());
                    v.extend_from_within(start..);
                }
                v
            }),
            // Longer than the internal buffer.
            (any::<u64>(), 0usize..1_500_000).prop_map(|(mut x, n)| {
                (0..n)
                    .map(|_| {
                        x = x
                            .wrapping_mul(6_364_136_223_846_793_005)
                            .wrapping_add(1_442_695_040_888_963_407);
                        b"aaaaabbbccd \n"[(x >> 60) as usize % 13]
                    })
                    .collect()
            }),
        ]
    }

    fn sizes() -> impl Strategy<Value = Vec<usize>> {
        prop::collection::vec(
            prop_oneof![
                0usize..4,
                1usize..300,
                300usize..70_000,
                Just(usize::MAX >> 1)
            ],
            1..6,
        )
        .prop_filter("some input", |v| v.iter().any(|&n| n > 0))
    }

    fn out_sizes() -> impl Strategy<Value = Vec<usize>> {
        prop::collection::vec(
            prop_oneof![0usize..4, 1usize..300, 300usize..70_000, Just(1 << 20)],
            1..6,
        )
        .prop_filter("some room", |v| v.iter().any(|&n| n > 0))
    }

    #[cfg(feature = "std")]
    #[test]
    fn reader_stops_at_the_end_of_the_stream() {
        use std::io::{BufReader, Read};
        let data: Vec<u8> = (0..300_000u32)
            .map(|i| (i % 251) as u8 ^ (i / 1000) as u8)
            .collect();
        for format in FORMATS {
            let mut c = reference_compress(format, 6, &data);
            c.extend_from_slice(b"next");
            for cap in [1, 7, 100, 8192, 1 << 20] {
                let mut r = DecompressReader::new(format, BufReader::with_capacity(cap, &c[..]));
                let mut out = Vec::new();
                r.read_to_end(&mut out).unwrap();
                assert!(out == data, "{format:?} cap {cap}");
                let mut rest = Vec::new();
                r.into_inner().read_to_end(&mut rest).unwrap();
                assert_eq!(rest, b"next", "{format:?} cap {cap}");
            }
            let mut r = DecompressReader::new(format, &c[..c.len() / 2]);
            let e = r.read_to_end(&mut Vec::new()).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof, "{format:?}");
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn measure_reader_stops_at_the_end_of_each_stream() {
        use std::io::{BufReader, Read};
        let data: Vec<u8> = (0..300_000u32)
            .map(|i| (i % 251) as u8 ^ (i / 1000) as u8)
            .collect();
        for format in FORMATS {
            let first = reference_compress(format, 6, &data);
            let second = reference_compress(format, 1, b"second");
            let mut c = [&first[..], &second[..], b"next"].concat();
            for cap in [1, 7, 100, 8192, 1 << 20] {
                let mut reader = BufReader::with_capacity(cap, &c[..]);
                let mut m = StreamMeasurer::new(format);
                assert_eq!(m.measure_reader(&mut reader).unwrap(), data.len() as u64);
                assert_eq!(m.total_in(), first.len() as u64);
                assert_eq!(m.measure_reader(&mut reader).unwrap(), 6);
                assert_eq!(m.total_in(), second.len() as u64);
                let mut rest = Vec::new();
                reader.read_to_end(&mut rest).unwrap();
                assert_eq!(rest, b"next", "{format:?} cap {cap}");
            }
            c.truncate(first.len() / 2);
            let e = StreamMeasurer::new(format)
                .measure_reader(&c[..])
                .unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof, "{format:?}");
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn chunked_matches_one_shot(
            data in data(),
            level in 0i32..=12,
            f in 0usize..3,
            in_sizes in sizes(),
            out_sizes in out_sizes(),
            trailing in prop::collection::vec(any::<u8>(), 0..20),
        ) {
            let format = FORMATS[f];
            let mut c = reference_compress(format, level, &data);
            let len = c.len();
            c.extend(&trailing);
            let (out, consumed) = stream(format, &c, &in_sizes, &out_sizes).unwrap();
            prop_assert!(out == data);
            prop_assert_eq!(consumed, len);
        }

        #[test]
        fn chunked_errors_match_one_shot(
            data in prop::collection::vec(0u8..8, 0..20_000),
            level in 0i32..=12,
            f in 0usize..3,
            flips in prop::collection::vec((any::<prop::sample::Index>(), 0u8..8), 1..4),
            in_sizes in sizes(),
            out_sizes in out_sizes(),
        ) {
            let format = FORMATS[f];
            let mut c = reference_compress(format, level, &data);
            for (i, bit) in flips {
                let i = i.index(c.len());
                c[i] ^= 1 << bit;
            }
            let want = one_shot(format, &c, data.len() + 1000);
            let got = stream(format, &c, &in_sizes, &out_sizes);
            match (&want, &got) {
                (Ok(w), Ok(g)) => prop_assert!(w == g),
                (Err(_), Err(_)) => {}
                _ => prop_assert!(false, "one-shot {:?} vs stream {:?}",
                    want.as_ref().map(|w| (w.0.len(), w.1)), got.as_ref().map(|g| (g.0.len(), g.1))),
            }
        }

        /// Measuring in chunks finds the same end and length as decoding.
        #[test]
        fn chunked_measure_matches_one_shot(
            data in data(),
            level in 0i32..=12,
            f in 0usize..3,
            in_sizes in sizes(),
            trailing in prop::collection::vec(any::<u8>(), 0..20),
        ) {
            let format = FORMATS[f];
            let mut c = reference_compress(format, level, &data);
            let len = c.len();
            c.extend(&trailing);
            prop_assert_eq!(stream_measure(format, &c, &in_sizes), Ok((data.len() as u64, len)));
        }

        /// Measuring corrupt input in chunks fails where the one-shot
        /// measure does (which matches decoding, short of the checksums).
        #[test]
        fn chunked_measure_errors_match_one_shot(
            data in prop::collection::vec(0u8..8, 0..20_000),
            level in 0i32..=12,
            f in 0usize..3,
            flips in prop::collection::vec((any::<prop::sample::Index>(), 0u8..8), 1..4),
            in_sizes in sizes(),
        ) {
            let format = FORMATS[f];
            let mut c = reference_compress(format, level, &data);
            for (i, bit) in flips {
                let i = i.index(c.len());
                c[i] ^= 1 << bit;
            }
            let want = Decompressor::new(format).measure(&c).map(|m| (m.len as u64, m.consumed));
            let got = stream_measure(format, &c, &in_sizes);
            match (&want, &got) {
                (Ok(_), _) => prop_assert_eq!(&got, &want),
                // The one-shot measure can't tell truncation from bad data.
                (Err(_), Err(_)) => {}
                _ => prop_assert!(false, "one-shot {:?} vs stream {:?}", want, got),
            }
        }

        #[test]
        fn truncated_measure_is_an_error(
            data in prop::collection::vec(0u8..8, 1..5_000),
            f in 0usize..3,
            cut in any::<prop::sample::Index>(),
            in_sizes in sizes(),
        ) {
            let format = FORMATS[f];
            let c = reference_compress(format, 6, &data);
            let c = &c[..cut.index(c.len())];
            prop_assert!(stream_measure(format, c, &in_sizes).is_err());
        }

        #[test]
        fn truncated_input_is_an_error(
            data in prop::collection::vec(0u8..8, 1..5_000),
            f in 0usize..3,
            cut in any::<prop::sample::Index>(),
            in_sizes in sizes(),
        ) {
            let format = FORMATS[f];
            let c = reference_compress(format, 6, &data);
            let c = &c[..cut.index(c.len())];
            prop_assert!(stream(format, c, &in_sizes, &[1 << 20]).is_err());
        }
    }
}
