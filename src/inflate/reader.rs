//! [`Reader`]: decompression as a [`Read`].

use std::io::{self, BufRead, Read};
use std::vec::Vec;

use super::stream::Decoder;
use crate::Format;

/// The buffer a [`Reader`] decodes into. At 128 KiB, large streams were 1-2%
/// slower.
const BUF_LEN: usize = 256 * 1024;

/// The room [`Reader::read_to_end`] first adds to the caller's `Vec`; it
/// doubles from there.
const FIRST_VEC_LEN: usize = 16 * 1024;

/// Decompresses a stream read from a [`BufRead`], as a [`Read`].
///
/// It consumes exactly the stream from the inner reader, so what follows can
/// be read from [`into_inner`](Self::into_inner). Reads return 0 once the
/// stream ends. One gzip member is read.
///
/// Decode errors are [`InvalidData`](io::ErrorKind::InvalidData) (wrapping
/// an [`Error`](super::Error)), or [`UnexpectedEof`](io::ErrorKind::UnexpectedEof)
/// if the input ends early. The inner reader's errors pass through.
///
/// [`read_to_end`](Read::read_to_end) and
/// [`read_to_string`](Read::read_to_string) decode straight into the caller's
/// buffer if nothing was read yet.
///
/// ```
/// use std::io::Read;
/// use sirocco::{inflate::Reader, Format};
///
/// // "hello hello hello" as a raw DEFLATE stream.
/// let compressed = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00];
/// let mut data = String::new();
/// Reader::new(Format::Deflate, &compressed[..]).read_to_string(&mut data)?;
/// assert_eq!(data, "hello hello hello");
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct Reader<R> {
    inner: R,
    decoder: std::boxed::Box<Decoder>,
    /// Allocated on the first read.
    buf: Vec<u8>,
    /// Output in `buf` not yet read.
    pending: core::ops::Range<usize>,
    /// `buf` is full: make room before the next decode.
    full: bool,
    done: bool,
}

impl<R: core::fmt::Debug> core::fmt::Debug for Reader<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Reader")
            .field("inner", &self.inner)
            .field("decoder", &self.decoder)
            .finish_non_exhaustive()
    }
}

impl<R: BufRead> Reader<R> {
    /// Reads a `format` stream from `inner`.
    pub fn new(format: Format, inner: R) -> Self {
        Reader {
            inner,
            decoder: std::boxed::Box::new(Decoder::new(format)),
            buf: Vec::new(),
            pending: 0..0,
            full: false,
            done: false,
        }
    }

    /// The decoder, for its input and output totals.
    pub fn decoder(&self) -> &Decoder {
        &self.decoder
    }

    /// The inner reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// The inner reader. Reading from it mid-stream takes input from this
    /// reader.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// The inner reader, positioned after the stream once it ended.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Decodes until there is output or the stream is done.
    fn fill(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            // `vec!` gets zeroed pages straight from the allocator.
            self.buf = std::vec![0; BUF_LEN];
        }
        while self.pending.is_empty() && !self.done {
            if self.full {
                self.decoder.make_room(&mut self.buf);
            }
            let input = self.inner.fill_buf()?;
            let eof = input.is_empty();
            let p = self.decoder.decompress(input, &mut self.buf, eof)?;
            let (consumed, len) = (p.consumed, p.output.len());
            (self.done, self.full) = (p.done, p.full);
            self.inner.consume(consumed);
            let end = self.decoder.core.pos();
            self.pending = end - len..end;
        }
        Ok(())
    }

    /// Decodes the whole stream onto the end of `out`, growing it. Only before
    /// any output.
    fn decode_to_end(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        let base = out.len();
        let mut len = 0;
        out.resize(base + FIRST_VEC_LEN, 0);
        loop {
            let input = self.inner.fill_buf()?;
            let eof = input.is_empty();
            match self.decoder.decompress(input, &mut out[base..], eof) {
                Ok(p) => {
                    let (consumed, done, full) = (p.consumed, p.done, p.full);
                    len += p.output.len();
                    self.inner.consume(consumed);
                    if done {
                        self.done = true;
                        out.truncate(base + len);
                        return Ok(len);
                    }
                    if full {
                        out.resize(base + 2 * len, 0);
                    }
                }
                Err(e) => {
                    out.truncate(base + len);
                    return Err(e.into());
                }
            }
        }
    }
}

impl<R: BufRead> Read for Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.fill()?;
        let n = self.pending.len().min(buf.len());
        buf[..n].copy_from_slice(&self.buf[self.pending.start..self.pending.start + n]);
        self.pending.start += n;
        Ok(n)
    }

    fn read_to_end(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        if !self.done && self.decoder.total_out() == 0 && self.decoder.core.pos() == 0 {
            return self.decode_to_end(out);
        }
        let start = out.len();
        loop {
            self.fill()?;
            if self.pending.is_empty() {
                return Ok(out.len() - start);
            }
            out.extend_from_slice(&self.buf[self.pending.clone()]);
            self.pending.start = self.pending.end;
        }
    }

    fn read_to_string(&mut self, out: &mut std::string::String) -> io::Result<usize> {
        // The default doesn't call `read_to_end`.
        let mut bytes = Vec::new();
        let n = self.read_to_end(&mut bytes)?;
        let s = core::str::from_utf8(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        out.push_str(s);
        Ok(n)
    }
}
