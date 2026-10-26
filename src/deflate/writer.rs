//! [`Writer`]: compression as a [`Write`].

use std::io::{self, Write};
use std::vec::Vec;

use super::{Encoder, Flush, Level};
use crate::Format;

/// The input buffer, which `make_room` must be able to free some of.
const BUF_LEN: usize = 256 * 1024;
const _: () = assert!(BUF_LEN >= 2 * super::MIN_BUFFER_LEN);

/// The output buffer, written to the inner writer when full.
const OUT_LEN: usize = 64 * 1024;

/// Compresses what is written to it into another [`Write`].
///
/// [`flush`](Write::flush) writes out all of the data so far (a
/// [`Flush::Sync`]), and [`finish`](Self::finish) ends the stream, which
/// must be called: dropping a writer doesn't. That way errors writing the
/// end aren't lost, and a writer dropped early, as when a panic unwinds,
/// leaves a stream that decompressors reject as cut short rather than a
/// complete-looking one of part of the data.
///
/// ```
/// use std::io::Write;
/// use sirocco::{deflate::{Level, Writer}, Format};
///
/// let mut w = Writer::new(Format::Gzip, Level::DEFAULT, Vec::new());
/// w.write_all(b"hello hello hello")?;
/// let compressed = w.finish()?;
/// assert_eq!(sirocco::decompress(Format::Gzip, &compressed, 1 << 20)?, b"hello hello hello");
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct Writer<W: Write> {
    inner: W,
    encoder: std::boxed::Box<Encoder>,
    /// The input, filled up to its length.
    buf: Vec<u8>,
    out: Vec<u8>,
}

impl<W: Write + core::fmt::Debug> core::fmt::Debug for Writer<W> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Writer")
            .field("inner", &self.inner)
            .field("encoder", &self.encoder)
            .finish_non_exhaustive()
    }
}

impl<W: Write> Writer<W> {
    /// Writes a `format` stream compressed at `level` to `inner`.
    pub fn new(format: Format, level: Level, inner: W) -> Self {
        Writer {
            inner,
            encoder: std::boxed::Box::new(Encoder::new(format, level)),
            buf: Vec::new(),
            out: Vec::new(),
        }
    }

    /// The encoder, for its input and output totals.
    pub fn encoder(&self) -> &Encoder {
        &self.encoder
    }

    /// The inner writer.
    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    /// The inner writer. Writing to it mid-stream corrupts the stream.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Ends the stream and returns the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.run(Flush::Finish)?;
        Ok(self.inner)
    }

    /// Compresses the buffer with `flush` and writes the output out.
    fn run(&mut self, flush: Flush) -> io::Result<()> {
        if self.out.is_empty() {
            self.out = std::vec![0; OUT_LEN];
        }
        loop {
            let p = self.encoder.compress(&self.buf, &mut self.out, flush);
            let full = p.full;
            self.inner.write_all(p.output)?;
            if !full {
                return Ok(());
            }
        }
    }
}

impl<W: Write> Write for Writer<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        if self.buf.capacity() == 0 {
            self.buf.reserve_exact(BUF_LEN);
        }
        if self.buf.len() == BUF_LEN {
            self.run(Flush::None)?;
            let dropped = self.encoder.make_room(&mut self.buf);
            self.buf.truncate(BUF_LEN - dropped);
        }
        let n = data.len().min(BUF_LEN - self.buf.len());
        self.buf.extend_from_slice(&data[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.run(Flush::Sync)?;
        self.get_mut().flush()
    }
}
