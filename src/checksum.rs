//! The checksums in the zlib and gzip trailers.
//!
//! Adler-32 (zlib) delegates to `adler32furious` and CRC-32 (gzip, the
//! IEEE 802.3 polynomial) to `crc32fast`. Both pick a SIMD implementation at
//! runtime with `std`, or from the enabled target features without it.

/// Updates a running Adler-32 checksum with `data`.
///
/// Pass `1` as the initial value to start a new checksum.
#[inline]
#[must_use]
pub fn adler32(adler: u32, data: &[u8]) -> u32 {
    let mut a = adler32furious::Adler32::from_checksum(adler);
    a.update(data);
    a.checksum()
}

/// Updates a running CRC-32 with `data`.
///
/// Pass `0` as the initial value to start a new checksum.
#[must_use]
pub fn crc32(crc: u32, data: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new_with_initial(crc);
    hasher.update(data);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adler32_known_values() {
        assert_eq!(adler32(1, b""), 1);
        assert_eq!(adler32(1, b"Wikipedia"), 0x11E6_0398);
        assert_eq!(adler32(adler32(1, b"Wiki"), b"pedia"), 0x11E6_0398);
    }

    #[test]
    fn known_values() {
        assert_eq!(crc32(0, b""), 0);
        assert_eq!(crc32(0, b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn incremental_matches_whole() {
        let data: [u8; 1000] = core::array::from_fn(|i| (i * 31 + 7) as u8);
        for split in [0, 1, 7, 8, 9, 500, 999, 1000] {
            let inc = crc32(crc32(0, &data[..split]), &data[split..]);
            assert_eq!(inc, crc32(0, &data), "split at {split}");
        }
    }
}
