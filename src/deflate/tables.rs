//! DEFLATE's constants and symbol tables, and the static Huffman codes.

use super::huffman::canonical_codewords;

/// How far back a match can reach.
pub(super) const WINDOW: usize = 32 * 1024;

pub(super) const MIN_MATCH: u32 = 3;
pub(super) const MAX_MATCH: u32 = 258;

pub(super) const NUM_LITLEN_SYMS: usize = 288;
pub(super) const NUM_OFFSET_SYMS: usize = 32;
pub(super) const NUM_PRECODE_SYMS: usize = 19;
pub(super) const END_OF_BLOCK: usize = 256;
pub(super) const FIRST_LEN_SYM: usize = 257;

/// The longest litlen codeword the encoder makes. DEFLATE allows 15, but 14
/// costs next to nothing and lets four literals go in a 64-bit buffer at once
/// (libdeflate does the same).
pub(super) const MAX_LITLEN_CODEWORD_LEN: u32 = 14;
pub(super) const MAX_OFFSET_CODEWORD_LEN: u32 = 15;
pub(super) const MAX_PRE_CODEWORD_LEN: u32 = 7;

/// The most bits a match takes: litlen codeword, extra length bits, offset
/// codeword, extra offset bits.
pub(super) const MAX_MATCH_BITS: u32 = MAX_LITLEN_CODEWORD_LEN + 5 + MAX_OFFSET_CODEWORD_LEN + 13;

/// Each length slot's base and extra bits.
pub(super) const LENGTH_BASE: [u32; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
pub(super) const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// Each offset slot's base and extra bits.
pub(super) const OFFSET_BASE: [u32; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
pub(super) const OFFSET_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The order the precode's codeword lengths are written in.
pub(super) const PRECODE_PERMUTATION: [u8; NUM_PRECODE_SYMS] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Each precode symbol's extra bits.
pub(super) const PRECODE_EXTRA: [u8; NUM_PRECODE_SYMS] =
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 3, 7];

/// Match length to length slot.
pub(super) static LENGTH_SLOT: [u8; MAX_MATCH as usize + 1] = {
    let mut t = [0; MAX_MATCH as usize + 1];
    let mut slot = 0;
    while slot < LENGTH_BASE.len() {
        let base = LENGTH_BASE[slot] as usize;
        let mut i = 0;
        while i < 1 << LENGTH_EXTRA[slot] && base + i <= MAX_MATCH as usize {
            t[base + i] = slot as u8;
            i += 1;
        }
        slot += 1;
    }
    t
};

/// `offset - 1` to offset slot, for offsets up to 256.
static OFFSET_SLOT: [u8; 256] = {
    let mut t = [0; 256];
    let mut slot = 0;
    while slot < 16 {
        let base = OFFSET_BASE[slot] as usize - 1;
        let mut i = 0;
        while i < 1 << OFFSET_EXTRA[slot] {
            t[base + i] = slot as u8;
            i += 1;
        }
        slot += 1;
    }
    t
};

/// The slot of an offset from 1 to 32768.
///
/// From libdeflate: slots 16 and up are 128 times the size of the slots 14
/// lower, so offsets over 256 look up `(offset - 1) >> 7` and add 14.
#[inline(always)]
pub(super) fn offset_slot(offset: u32) -> u32 {
    debug_assert!((1..=WINDOW as u32).contains(&offset));
    // 7 if offset > 256, else 0.
    let n = (256u32.wrapping_sub(offset) >> 29) & 7;
    u32::from(OFFSET_SLOT[((offset - 1) >> n) as usize & 0xFF]) + (n << 1)
}

/// Codewords (bit-reversed, as written) and their lengths for a block's
/// litlen and offset codes. Length 0 means unused.
#[derive(Clone, Debug)]
pub(super) struct Codes {
    pub(super) litlen: [u32; NUM_LITLEN_SYMS],
    pub(super) litlen_lens: [u8; NUM_LITLEN_SYMS],
    pub(super) offset: [u32; NUM_OFFSET_SYMS],
    pub(super) offset_lens: [u8; NUM_OFFSET_SYMS],
}

impl Codes {
    pub(super) const EMPTY: Codes = Codes {
        litlen: [0; NUM_LITLEN_SYMS],
        litlen_lens: [0; NUM_LITLEN_SYMS],
        offset: [0; NUM_OFFSET_SYMS],
        offset_lens: [0; NUM_OFFSET_SYMS],
    };
}

const STATIC_LITLEN_LENS: [u8; NUM_LITLEN_SYMS] = {
    let mut t = [8; NUM_LITLEN_SYMS];
    let mut i = 144;
    while i < 256 {
        t[i] = 9;
        i += 1;
    }
    while i < 280 {
        t[i] = 7;
        i += 1;
    }
    t
};

/// The static Huffman codes (RFC 1951, 3.2.6).
pub(super) static STATIC_CODES: Codes = Codes {
    litlen: canonical_codewords(&STATIC_LITLEN_LENS),
    litlen_lens: STATIC_LITLEN_LENS,
    offset: canonical_codewords(&[5; NUM_OFFSET_SYMS]),
    offset_lens: [5; NUM_OFFSET_SYMS],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_cover_their_ranges() {
        for len in MIN_MATCH..=MAX_MATCH {
            let s = LENGTH_SLOT[len as usize] as usize;
            let (base, extra) = (LENGTH_BASE[s], LENGTH_EXTRA[s]);
            assert!(len >= base && len - base < 1 << extra, "length {len}");
        }
        // 258 has its own slot rather than 227 + 31.
        assert_eq!(LENGTH_SLOT[258], 28);
        for offset in 1..=WINDOW as u32 {
            let s = offset_slot(offset) as usize;
            let (base, extra) = (OFFSET_BASE[s], OFFSET_EXTRA[s]);
            assert!(
                offset >= base && offset - base < 1 << extra,
                "offset {offset}"
            );
        }
    }

    #[test]
    fn static_codes() {
        // 256 is 0000000, 0 is 00110000 and 144 is 110010000 (MSB first).
        let rev = |cw: u32, len: u32| cw.reverse_bits() >> (32 - len);
        assert_eq!(rev(STATIC_CODES.litlen[256], 7), 0);
        assert_eq!(rev(STATIC_CODES.litlen[0], 8), 0b0011_0000);
        assert_eq!(rev(STATIC_CODES.litlen[144], 9), 0b1_1001_0000);
        assert_eq!(rev(STATIC_CODES.litlen[280], 8), 0b1100_0000);
        assert_eq!(rev(STATIC_CODES.offset[3], 5), 3);
    }
}
