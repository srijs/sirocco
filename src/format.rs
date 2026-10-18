//! The container formats around a DEFLATE stream.

/// Which wrapper, if any, goes around the DEFLATE stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// A raw DEFLATE stream (RFC 1951), with no header or checksum.
    Deflate,
    /// A zlib stream (RFC 1950): a 2-byte header, DEFLATE, and an Adler-32
    /// checksum. Streams that need a preset dictionary are rejected with
    /// [`PresetDictionary`](crate::inflate::Error::PresetDictionary).
    Zlib,
    /// A gzip member (RFC 1952): a header, DEFLATE, a CRC-32 and the
    /// uncompressed size.
    Gzip,
}
