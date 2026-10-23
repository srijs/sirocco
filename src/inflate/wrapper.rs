//! The zlib (RFC 1950) and gzip (RFC 1952) headers and trailers around a
//! DEFLATE stream, shared by the streaming and `Vec` decoders.

use super::Error;
use crate::Format;

const FHCRC: u8 = 0x02;
const FEXTRA: u8 = 0x04;
const FNAME: u8 = 0x08;
const FCOMMENT: u8 = 0x10;
const FRESERVED: u8 = 0xE0;

/// Where the header parser is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    /// The fixed part: zlib's CMF and FLG, or gzip's ID1, ID2, CM, FLG,
    /// MTIME, XFL and OS.
    Fixed,
    /// zlib's DICTID, which zlib reads before it reports that the stream
    /// needs a preset dictionary.
    DictId,
    /// The 2-byte FEXTRA length.
    ExtraLen,
    /// FEXTRA bytes left to skip.
    Extra(usize),
    Name,
    Comment,
    /// The 2-byte header CRC.
    HeaderCrc,
    Done,
}

/// Parses a zlib or gzip header that may arrive in pieces.
#[derive(Clone, Debug)]
pub(super) struct HeaderParser {
    field: Field,
    /// Bytes of a fixed-size field collected so far.
    buf: [u8; 10],
    buf_len: usize,
    gzip_flags: u8,
    /// The CRC-32 of the gzip header so far, which FHCRC records the low
    /// half of.
    crc: u32,
    /// Whether `crc` covers all of the header it should.
    crc_done: bool,
}

impl HeaderParser {
    pub(super) const fn new() -> Self {
        HeaderParser {
            field: Field::Fixed,
            buf: [0; 10],
            buf_len: 0,
            gzip_flags: 0,
            crc: 0,
            crc_done: false,
        }
    }

    /// Moves bytes from `input` into `buf` until it holds `need`. Returns
    /// whether it does.
    fn collect(&mut self, input: &[u8], ip: &mut usize, need: usize) -> bool {
        let n = (need - self.buf_len).min(input.len() - *ip);
        self.buf[self.buf_len..self.buf_len + n].copy_from_slice(&input[*ip..*ip + n]);
        self.buf_len += n;
        *ip += n;
        self.buf_len == need
    }

    /// Parses as much of the header of `format` as `input[*ip..]` holds,
    /// advancing `ip` past it. Returns whether the header is complete.
    pub(super) fn parse(
        &mut self,
        format: Format,
        input: &[u8],
        ip: &mut usize,
    ) -> Result<bool, Error> {
        let start = *ip;
        let r = self.parse_fields(format, input, ip, start);
        if format == Format::Gzip {
            self.update_crc(&input[start..*ip]);
        }
        r
    }

    fn update_crc(&mut self, header: &[u8]) {
        if !self.crc_done {
            self.crc = crate::checksum::crc32(self.crc, header);
        }
    }

    /// [`parse`](Self::parse), where `input[start..*ip]` is the header this
    /// call has read, not yet in `crc`.
    fn parse_fields(
        &mut self,
        format: Format,
        input: &[u8],
        ip: &mut usize,
        start: usize,
    ) -> Result<bool, Error> {
        let bad = Err(Error::BadData);
        loop {
            match self.field {
                Field::Done => return Ok(true),
                Field::Fixed => match format {
                    Format::Deflate => self.field = Field::Done,
                    Format::Zlib => {
                        if !self.collect(input, ip, 2) {
                            return Ok(false);
                        }
                        match zlib_header([self.buf[0], self.buf[1]]) {
                            Err(Error::PresetDictionary) => self.field = Field::DictId,
                            r => {
                                r?;
                                self.field = Field::Done;
                            }
                        }
                    }
                    Format::Gzip => {
                        // As zlib, check each part once it is in: the magic
                        // number, then CM and FLG.
                        let complete = self.collect(input, ip, 10);
                        let b = &self.buf[..self.buf_len];
                        if b.len() >= 2 && (b[0] != 0x1F || b[1] != 0x8B)
                            || b.len() >= 4 && (b[2] != 8 || b[3] & FRESERVED != 0)
                        {
                            return bad;
                        }
                        if !complete {
                            return Ok(false);
                        }
                        self.gzip_flags = self.buf[3];
                        self.buf_len = 0;
                        self.field = Field::ExtraLen;
                    }
                },
                Field::DictId => {
                    if !self.collect(input, ip, 6) {
                        return Ok(false);
                    }
                    return Err(Error::PresetDictionary);
                }
                Field::ExtraLen => {
                    if self.gzip_flags & FEXTRA == 0 {
                        self.field = Field::Name;
                        continue;
                    }
                    if !self.collect(input, ip, 2) {
                        return Ok(false);
                    }
                    let xlen = u16::from_le_bytes([self.buf[0], self.buf[1]]);
                    self.buf_len = 0;
                    self.field = Field::Extra(xlen as usize);
                }
                Field::Extra(n) => {
                    let skip = n.min(input.len() - *ip);
                    *ip += skip;
                    if skip < n {
                        self.field = Field::Extra(n - skip);
                        return Ok(false);
                    }
                    self.field = Field::Name;
                }
                Field::Name | Field::Comment => {
                    let (flag, next) = if self.field == Field::Name {
                        (FNAME, Field::Comment)
                    } else {
                        (FCOMMENT, Field::HeaderCrc)
                    };
                    if self.gzip_flags & flag != 0 {
                        // Zero-terminated.
                        let Some(i) = input[*ip..].iter().position(|&b| b == 0) else {
                            *ip = input.len();
                            return Ok(false);
                        };
                        *ip += i + 1;
                    }
                    self.field = next;
                }
                Field::HeaderCrc => {
                    if self.gzip_flags & FHCRC != 0 {
                        // As zlib, check it: the low half of the CRC-32 of
                        // the header before it.
                        if !self.crc_done {
                            self.update_crc(&input[start..*ip]);
                            self.crc_done = true;
                        }
                        if !self.collect(input, ip, 2) {
                            return Ok(false);
                        }
                        if u16::from_le_bytes([self.buf[0], self.buf[1]]) != self.crc as u16 {
                            return bad;
                        }
                    }
                    self.field = Field::Done;
                }
            }
        }
    }
}

/// Checks a zlib header (CMF and FLG): Deflate with at most a 32 KiB window,
/// and no preset dictionary, which isn't supported.
pub(super) fn zlib_header(hdr: [u8; 2]) -> Result<(), Error> {
    let hdr = u16::from_be_bytes(hdr);
    let cm = (hdr >> 8) & 0xF;
    let cinfo = hdr >> 12;
    let fdict = (hdr >> 5) & 1;
    if !hdr.is_multiple_of(31) || cm != 8 || cinfo > 7 {
        Err(Error::BadData)
    } else if fdict != 0 {
        Err(Error::PresetDictionary)
    } else {
        Ok(())
    }
}

/// Where the DEFLATE body of the zlib stream at the start of `input`
/// starts. zlib's fixed 2 bytes skip the general parser, which matters for
/// streams as small as most git objects.
fn zlib_body_start(input: &[u8]) -> Result<usize, Error> {
    let Some(&hdr) = input.first_chunk() else {
        return Err(Error::Truncated);
    };
    match zlib_header(hdr) {
        // zlib reads the dictionary's ID before it asks for the dictionary.
        Err(Error::PresetDictionary) if input.len() < 6 => Err(Error::Truncated),
        r => r.map(|()| 2),
    }
}

/// Where the DEFLATE body of the `format` stream at the start of `input`
/// starts.
#[inline]
pub(super) fn body_start(format: Format, input: &[u8]) -> Result<usize, Error> {
    match format {
        Format::Deflate => Ok(0),
        Format::Zlib => zlib_body_start(input),
        Format::Gzip => {
            let mut pos = 0;
            if !HeaderParser::new().parse(format, input, &mut pos)? {
                return Err(Error::Truncated);
            }
            Ok(pos)
        }
    }
}

/// The trailer's size in bytes.
pub(super) const fn trailer_len(format: Format) -> usize {
    match format {
        Format::Deflate => 0,
        Format::Zlib => 4,
        Format::Gzip => 8,
    }
}

/// The running checksum of the output that the trailer records: Adler-32
/// for zlib, CRC-32 and the length mod 2^32 for gzip.
#[derive(Clone, Copy, Debug)]
pub(super) struct Check {
    format: Format,
    /// Counts the output without seeing it, so only the length can be
    /// checked.
    len_only: bool,
    value: u32,
    len: u64,
}

impl Check {
    pub(super) const fn new(format: Format) -> Self {
        let value = match format {
            Format::Zlib => 1,
            _ => 0,
        };
        Check {
            format,
            len_only: false,
            value,
            len: 0,
        }
    }

    /// A check of only the length field of the trailer (gzip's ISIZE), for
    /// output that is counted with [`count`](Self::count) rather than seen.
    pub(super) const fn len_only(format: Format) -> Self {
        Check {
            len_only: true,
            ..Check::new(format)
        }
    }

    /// Adds `data` to the checksum.
    #[inline]
    pub(super) fn update(&mut self, data: &[u8]) {
        self.len += data.len() as u64;
        self.value = match self.format {
            Format::Deflate => 0,
            Format::Zlib => crate::checksum::adler32(self.value, data),
            Format::Gzip => crate::checksum::crc32(self.value, data),
        };
    }

    /// Counts `n` bytes of output for a [`len_only`](Self::len_only) check.
    pub(super) fn count(&mut self, n: usize) {
        self.len += n as u64;
    }

    /// Checks the trailer at `input[pos..]`. Returns where it ends.
    pub(super) fn verify_at(&self, input: &[u8], pos: usize) -> Result<usize, Error> {
        let end = pos + trailer_len(self.format);
        let t = &input[pos..end.min(input.len())];
        self.verify(t)?;
        if t.len() < trailer_len(self.format) {
            return Err(Error::Truncated);
        }
        Ok(end)
    }

    /// Checks the trailer, or as much of it as `t` holds: as zlib, each
    /// 4-byte field once it is in.
    pub(super) fn verify(&self, t: &[u8]) -> Result<(), Error> {
        let word = |i: usize| t.get(i..i + 4).map(|w| [w[0], w[1], w[2], w[3]]);
        let ok = match self.format {
            Format::Deflate => true,
            Format::Zlib => {
                self.len_only || word(0).is_none_or(|w| u32::from_be_bytes(w) == self.value)
            }
            Format::Gzip => {
                (self.len_only || word(0).is_none_or(|w| u32::from_le_bytes(w) == self.value))
                    && word(4).is_none_or(|w| u32::from_le_bytes(w) == self.len as u32)
            }
        };
        if ok {
            Ok(())
        } else {
            Err(Error::ChecksumMismatch)
        }
    }
}
