//! Huffman decode tables, ported from libdeflate's `deflate_decompress.c`.
//!
//! See libdeflate for the full rationale; the comments here cover the entry
//! formats and the parts that differ.

/// Number of symbols in each DEFLATE code.
pub(super) const NUM_PRECODE_SYMS: usize = 19;
pub(super) const NUM_LITLEN_SYMS: usize = 288;
pub(super) const NUM_OFFSET_SYMS: usize = 32;
const MAX_NUM_SYMS: usize = 288;

pub(super) const MAX_PRE_CODEWORD_LEN: u32 = 7;
pub(super) const MAX_LITLEN_CODEWORD_LEN: u32 = 15;
const MAX_OFFSET_CODEWORD_LEN: u32 = 15;
const MAX_CODEWORD_LEN: usize = 15;

const MAX_EXTRA_LENGTH_BITS: u32 = 5;
const MAX_EXTRA_OFFSET_BITS: u32 = 13;
pub(super) const MAX_MATCH_LEN: usize = 258;

/// The precode can repeat a zero up to 138 times when only one length is
/// left, so the lengths array has this much slack at the end.
const MAX_LENS_OVERRUN: usize = 137;

/// Main table bits and worst-case table sizes (main table plus subtables),
/// computed with zlib's `enough` utility.
pub(super) const PRECODE_TABLEBITS: u32 = 7;
const PRECODE_ENOUGH: usize = 128;
pub(super) const LITLEN_TABLEBITS: u32 = 11;
const LITLEN_ENOUGH: usize = 2342;
pub(super) const OFFSET_TABLEBITS: u32 = 8;
const OFFSET_ENOUGH: usize = 402;

/// Set on literal entries in the litlen table. The high bit, so testing it is
/// a sign test on most CPUs.
pub(super) const HUFFDEC_LITERAL: u32 = 0x8000_0000;
/// Set when either `HUFFDEC_SUBTABLE_POINTER` or `HUFFDEC_END_OF_BLOCK` is.
pub(super) const HUFFDEC_EXCEPTIONAL: u32 = 0x0000_8000;
pub(super) const HUFFDEC_SUBTABLE_POINTER: u32 = 0x0000_4000;
pub(super) const HUFFDEC_END_OF_BLOCK: u32 = 0x0000_2000;

/// Most bits a match length can take: codeword plus extra bits.
pub(super) const LENGTH_MAXBITS: u32 = MAX_LITLEN_CODEWORD_LEN + MAX_EXTRA_LENGTH_BITS;
/// Most bits a match length can take when it doesn't need a subtable.
pub(super) const LENGTH_MAXFASTBITS: u32 = LITLEN_TABLEBITS + MAX_EXTRA_LENGTH_BITS;
pub(super) const OFFSET_MAXBITS: u32 = MAX_OFFSET_CODEWORD_LEN + MAX_EXTRA_OFFSET_BITS;
pub(super) const OFFSET_MAXFASTBITS: u32 = OFFSET_TABLEBITS + MAX_EXTRA_OFFSET_BITS;

/// Precode entries:
///
/// ```text
/// Bit 20-16:  presym
/// Bit 10-8:   codeword length [not used]
/// Bit 2-0:    codeword length
/// ```
static PRECODE_DECODE_RESULTS: [u32; NUM_PRECODE_SYMS] = {
    let mut t = [0; NUM_PRECODE_SYMS];
    let mut i = 0;
    while i < NUM_PRECODE_SYMS {
        t[i] = (i as u32) << 16;
        i += 1;
    }
    t
};

/// Litlen entries:
///
/// ```text
/// Literals:
///     Bit 31:     1 (HUFFDEC_LITERAL)
///     Bit 15-8:   literal value
///     Bit 3-0:    remaining codeword length
/// Lengths:
///     Bit 31:     0
///     Bit 24-16:  length base value
///     Bit 15-13:  0
///     Bit 11-8:   remaining codeword length
///     Bit 4-0:    remaining codeword length + number of extra bits
/// End of block:
///     Bit 31:     0
///     Bit 15:     1 (HUFFDEC_EXCEPTIONAL)
///     Bit 14:     0
///     Bit 13:     1 (HUFFDEC_END_OF_BLOCK)
///     Bit 11-8:   remaining codeword length [not used]
///     Bit 3-0:    remaining codeword length
/// Subtable pointer:
///     Bit 31:     0
///     Bit 30-16:  index of start of subtable
///     Bit 15:     1 (HUFFDEC_EXCEPTIONAL)
///     Bit 14:     1 (HUFFDEC_SUBTABLE_POINTER)
///     Bit 13:     0
///     Bit 11-8:   number of subtable bits
///     Bit 3-0:    number of main table bits
/// ```
///
/// The low byte is always the number of bits to consume, so the decoder can
/// do `bitsleft -= entry`; the high bits of `bitsleft` are allowed to hold
/// garbage.
static LITLEN_DECODE_RESULTS: [u32; NUM_LITLEN_SYMS] = {
    // Base and extra bits of length symbols 257 - 287. 286 and 287 are
    // invalid; like libdeflate, decode them as 285.
    const BASE: [u32; 31] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258, 258, 258,
    ];
    const EXTRA: [u32; 31] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0, 0, 0,
    ];
    let mut t = [0; NUM_LITLEN_SYMS];
    let mut i = 0;
    while i < 256 {
        t[i] = HUFFDEC_LITERAL | ((i as u32) << 8);
        i += 1;
    }
    t[256] = HUFFDEC_EXCEPTIONAL | HUFFDEC_END_OF_BLOCK;
    let mut j = 0;
    while j < BASE.len() {
        t[257 + j] = (BASE[j] << 16) | EXTRA[j];
        j += 1;
    }
    t
};

/// Offset entries:
///
/// ```text
/// Offsets:
///     Bit 31-16:  offset base value
///     Bit 15-14:  0
///     Bit 11-8:   remaining codeword length
///     Bit 4-0:    remaining codeword length + number of extra bits
/// Subtable pointer:
///     Bit 31-16:  index of start of subtable
///     Bit 15:     1 (HUFFDEC_EXCEPTIONAL)
///     Bit 14:     1 (HUFFDEC_SUBTABLE_POINTER)
///     Bit 11-8:   number of subtable bits
///     Bit 3-0:    number of main table bits
/// ```
static OFFSET_DECODE_RESULTS: [u32; NUM_OFFSET_SYMS] = {
    // Base and extra bits of offset symbols 0 - 31. 30 and 31 are invalid;
    // like libdeflate, decode them as 29.
    const BASE: [u32; NUM_OFFSET_SYMS] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
        2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577, 24577, 24577,
    ];
    const EXTRA: [u32; NUM_OFFSET_SYMS] = [
        0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
        13, 13, 13, 13,
    ];
    let mut t = [0; NUM_OFFSET_SYMS];
    let mut i = 0;
    while i < NUM_OFFSET_SYMS {
        t[i] = (BASE[i] << 16) | EXTRA[i];
        i += 1;
    }
    t
};

/// Combines the static part of an entry with the remaining codeword length,
/// which goes into both of the two low bytes (just the lowest for literals,
/// whose second byte holds the literal).
#[inline(always)]
fn make_decode_table_entry(decode_results: &[u32], sym: usize, len: u32) -> u32 {
    let r = decode_results[sym];
    let hi = if r & HUFFDEC_LITERAL != 0 {
        0
    } else {
        len << 8
    };
    r + hi + len
}

/// Builds a decode table for the canonical Huffman code given by `lens`,
/// indexed by bit-reversed codewords (DEFLATE sends the first bit of a
/// codeword in the lowest bit).
///
/// Returns `false` if `lens` isn't a valid code.
///
/// Every value in `lens` must be at most `max_codeword_len`.
#[allow(clippy::too_many_arguments)]
fn build_decode_table(
    decode_table: &mut [u32],
    lens: &[u8],
    decode_results: &[u32],
    table_bits: u32,
    mut max_codeword_len: u32,
    sorted_syms: &mut [u16; MAX_NUM_SYMS],
) -> bool {
    let mut len_counts = [0u32; MAX_CODEWORD_LEN + 1];
    let mut offsets = [0u32; MAX_CODEWORD_LEN + 1];

    // Lengths are at most `MAX_CODEWORD_LEN` (15), so masking them changes
    // nothing but spares these loops their bounds checks.
    const _: () = assert!(MAX_CODEWORD_LEN == 15);
    // Codes for small inputs leave most symbols unused, in long runs, and a
    // loop over every length would spend most of its time in a dependency
    // chain through the zero count. So skip eight unused symbols at a time.
    let (chunks, rest) = lens.as_chunks::<8>();
    for chunk in chunks {
        if u64::from_ne_bytes(*chunk) != 0 {
            for &l in chunk {
                len_counts[l as usize & 15] += 1;
            }
        }
    }
    for &l in rest {
        len_counts[l as usize & 15] += 1;
    }

    while max_codeword_len > 1 && len_counts[max_codeword_len as usize] == 0 {
        max_codeword_len -= 1;
    }

    // Sort symbols by codeword length then symbol value, i.e. by codeword,
    // and total up the codespace used on the way. The unused symbols that
    // weren't skipped go after the used ones, out of the way.
    offsets[1] = 0;
    let mut codespace_used: u32 = 0;
    let mut len = 1;
    while len < max_codeword_len {
        offsets[len as usize + 1] = offsets[len as usize] + len_counts[len as usize];
        codespace_used = (codespace_used << 1) + len_counts[len as usize];
        len += 1;
    }
    codespace_used = (codespace_used << 1) + len_counts[len as usize];
    offsets[0] = offsets[len as usize] + len_counts[len as usize];

    let mut sort = |sym: usize, l: u8| {
        let l = l as usize & 15;
        sorted_syms[offsets[l] as usize] = sym as u16;
        offsets[l] += 1;
    };
    for (i, chunk) in chunks.iter().enumerate() {
        if u64::from_ne_bytes(*chunk) != 0 {
            for (j, &l) in chunk.iter().enumerate() {
                sort(i * 8 + j, l);
            }
        }
    }
    for (j, &l) in rest.iter().enumerate() {
        sort(chunks.len() * 8 + j, l);
    }
    let sorted = &sorted_syms[..];
    let mut si = 0;

    // Overfull code.
    if codespace_used > (1 << max_codeword_len) {
        return false;
    }

    // Incomplete code. Like libdeflate, accept the empty code and a code with
    // a single codeword of length 1, and map both to a complete code by giving
    // one symbol both 1-bit codewords.
    if codespace_used < (1 << max_codeword_len) {
        let sym = if codespace_used == 0 {
            0
        } else {
            if codespace_used != (1 << (max_codeword_len - 1)) || len_counts[1] != 1 {
                return false;
            }
            sorted[0] as usize
        };
        let entry = make_decode_table_entry(decode_results, sym, 1);
        decode_table[..1 << table_bits].fill(entry);
        return true;
    }

    // The code is complete. Fill main table entries for codewords of at most
    // `table_bits` bits, growing the table by doubling so that each codeword
    // only needs a single store per length (bit-reversed codewords would
    // otherwise need strided stores).
    let mut codeword: u32 = 0;
    let mut len: u32 = 1;
    let mut count;
    loop {
        count = len_counts[len as usize];
        if count != 0 {
            break;
        }
        len += 1;
    }
    let mut cur_table_end: u32 = 1 << len;
    while len <= table_bits {
        loop {
            decode_table[codeword as usize] =
                make_decode_table_entry(decode_results, sorted[si] as usize, len);
            si += 1;

            if codeword == cur_table_end - 1 {
                // Last codeword (all 1s).
                while len < table_bits {
                    let end = cur_table_end as usize;
                    decode_table.copy_within(..end, end);
                    cur_table_end <<= 1;
                    len += 1;
                }
                return true;
            }
            // Advance to the next codeword in canonical order: increment the
            // bit-reversed codeword by setting its highest 0 bit and clearing
            // the 1 bits above it.
            let bit = 1 << bsr32(codeword ^ (cur_table_end - 1));
            codeword &= bit - 1;
            codeword |= bit;

            count -= 1;
            if count == 0 {
                break;
            }
        }

        // Advance to the next codeword length.
        loop {
            len += 1;
            if len <= table_bits {
                let end = cur_table_end as usize;
                decode_table.copy_within(..end, end);
                cur_table_end <<= 1;
            }
            count = len_counts[len as usize];
            if count != 0 {
                break;
            }
        }
    }

    // Codewords longer than `table_bits` go into subtables.
    let table_mask = (1u32 << table_bits) - 1;
    cur_table_end = 1 << table_bits;
    let mut subtable_prefix: u32 = u32::MAX;
    let mut subtable_start: u32 = 0;
    loop {
        if codeword & table_mask != subtable_prefix {
            subtable_prefix = codeword & table_mask;
            subtable_start = cur_table_end;
            // The subtable needs 2^n entries for a codeword of length
            // `table_bits + n`, or more if fewer than 2^n codewords of that
            // length remain.
            let mut subtable_bits = len - table_bits;
            let mut used = count;
            while used < (1 << subtable_bits) {
                subtable_bits += 1;
                used = (used << 1) + len_counts[(table_bits + subtable_bits) as usize];
            }
            cur_table_end = subtable_start + (1 << subtable_bits);

            decode_table[subtable_prefix as usize] = (subtable_start << 16)
                | HUFFDEC_EXCEPTIONAL
                | HUFFDEC_SUBTABLE_POINTER
                | (subtable_bits << 8)
                | table_bits;
        }

        let entry = make_decode_table_entry(decode_results, sorted[si] as usize, len - table_bits);
        si += 1;
        let stride = 1usize << (len - table_bits);
        let mut i = (subtable_start + (codeword >> table_bits)) as usize;
        let end = cur_table_end as usize;
        loop {
            decode_table[i] = entry;
            i += stride;
            if i >= end {
                break;
            }
        }

        if codeword == (1 << len) - 1 {
            return true;
        }
        let bit = 1 << bsr32(codeword ^ ((1 << len) - 1));
        codeword &= bit - 1;
        codeword |= bit;
        count -= 1;
        while count == 0 {
            len += 1;
            count = len_counts[len as usize];
        }
    }
}

// `ilog2` would add a zero check; callers never pass zero.
#[allow(clippy::manual_ilog2)]
#[inline(always)]
fn bsr32(v: u32) -> u32 {
    31 - v.leading_zeros()
}

/// The decode tables for the current block, and the scratch space to build
/// them: about 13 KiB.
#[derive(Clone)]
pub(super) struct Tables {
    /// The precode codeword lengths of a dynamic block header.
    pub(super) precode_lens: [u8; NUM_PRECODE_SYMS],
    pub(super) precode: [u32; PRECODE_ENOUGH],
    /// The litlen then offset codeword lengths of a dynamic block, with
    /// room for the precode to overrun them.
    pub(super) lens: [u8; NUM_LITLEN_SYMS + NUM_OFFSET_SYMS + MAX_LENS_OVERRUN],
    pub(super) litlen: [u32; LITLEN_ENOUGH],
    pub(super) offset: [u32; OFFSET_ENOUGH],
    sorted_syms: [u16; MAX_NUM_SYMS],
    /// Whether `litlen` and `offset` hold the static codes.
    static_loaded: bool,
}

impl Tables {
    pub(super) const fn new() -> Self {
        Tables {
            precode_lens: [0; NUM_PRECODE_SYMS],
            precode: [0; PRECODE_ENOUGH],
            lens: [0; NUM_LITLEN_SYMS + NUM_OFFSET_SYMS + MAX_LENS_OVERRUN],
            litlen: [0; LITLEN_ENOUGH],
            offset: [0; OFFSET_ENOUGH],
            sorted_syms: [0; MAX_NUM_SYMS],
            static_loaded: false,
        }
    }

    /// Builds the precode table from `precode_lens`. Returns whether they
    /// make a valid code.
    // This and `build_dynamic` are inlined so that the out-of-line builds
    // get only the fields they touch: passing them all of `self` costs the
    // decode loop a few percent on small streams.
    #[inline(always)]
    pub(super) fn build_precode(&mut self) -> bool {
        build_precode_table(&mut self.precode, &self.precode_lens, &mut self.sorted_syms)
    }

    /// Builds the litlen and offset tables from the first `num_litlen_syms`
    /// and the next `num_offset_syms` of `lens`. Returns whether they make
    /// valid codes.
    #[inline(always)]
    pub(super) fn build_dynamic(&mut self, num_litlen_syms: usize, num_offset_syms: usize) -> bool {
        self.static_loaded = false;
        let (litlen_lens, offset_lens) =
            self.lens[..num_litlen_syms + num_offset_syms].split_at(num_litlen_syms);
        build_offset_table(&mut self.offset, offset_lens, &mut self.sorted_syms)
            && build_litlen_table(&mut self.litlen, litlen_lens, &mut self.sorted_syms)
    }

    /// Loads the static Huffman codes, which stay loaded until the next
    /// dynamic block. Their tables are built at compile time: copying them
    /// is much cheaper than building them, and streams of small blocks (such
    /// as git objects) switch between static and dynamic codes often.
    #[inline(always)]
    pub(super) fn load_static(&mut self) {
        if !self.static_loaded {
            self.static_loaded = true;
            self.litlen[..STATIC_LITLEN_TABLE.len()].copy_from_slice(&STATIC_LITLEN_TABLE);
            self.offset[..STATIC_OFFSET_TABLE.len()].copy_from_slice(&STATIC_OFFSET_TABLE);
        }
    }
}

// Once per block: keep it out of the decode loop's register allocation.
#[inline(never)]
fn build_precode_table(
    table: &mut [u32; PRECODE_ENOUGH],
    lens: &[u8; NUM_PRECODE_SYMS],
    sorted_syms: &mut [u16; MAX_NUM_SYMS],
) -> bool {
    build_decode_table(
        table,
        lens,
        &PRECODE_DECODE_RESULTS,
        PRECODE_TABLEBITS,
        MAX_PRE_CODEWORD_LEN,
        sorted_syms,
    )
}

/// Builds the litlen table. The main table always has `LITLEN_TABLEBITS`
/// bits, so the decoder can mask with a constant.
#[inline(never)]
fn build_litlen_table(
    table: &mut [u32; LITLEN_ENOUGH],
    lens: &[u8],
    sorted_syms: &mut [u16; MAX_NUM_SYMS],
) -> bool {
    build_decode_table(
        table,
        lens,
        &LITLEN_DECODE_RESULTS,
        LITLEN_TABLEBITS,
        MAX_LITLEN_CODEWORD_LEN,
        sorted_syms,
    )
}

#[inline(never)]
fn build_offset_table(
    table: &mut [u32; OFFSET_ENOUGH],
    lens: &[u8],
    sorted_syms: &mut [u16; MAX_NUM_SYMS],
) -> bool {
    build_decode_table(
        table,
        lens,
        &OFFSET_DECODE_RESULTS,
        OFFSET_TABLEBITS,
        MAX_OFFSET_CODEWORD_LEN,
        sorted_syms,
    )
}

/// The main table of `table_bits` bits for a code with no codeword longer
/// than that, built at compile time: what [`build_decode_table`] makes of
/// the same lengths.
const fn build_short_code_table<const N: usize>(
    lens: &[u8],
    decode_results: &[u32],
    table_bits: u32,
) -> [u32; N] {
    let mut table = [0; N];
    let mut len_counts = [0u32; MAX_CODEWORD_LEN + 1];
    let mut i = 0;
    while i < lens.len() {
        len_counts[lens[i] as usize] += 1;
        i += 1;
    }
    // The first canonical codeword of each length.
    let mut next_code = [0u32; MAX_CODEWORD_LEN + 1];
    let mut code = 0;
    let mut len = 1;
    while len <= MAX_CODEWORD_LEN {
        code = (code + len_counts[len - 1]) << 1;
        next_code[len] = code;
        len += 1;
    }
    let mut sym = 0;
    while sym < lens.len() {
        let len = lens[sym] as u32;
        if len != 0 {
            assert!(len <= table_bits);
            let codeword = next_code[len as usize];
            next_code[len as usize] += 1;
            let reversed = codeword.reverse_bits() >> (32 - len);
            let r = decode_results[sym];
            let hi = if r & HUFFDEC_LITERAL != 0 {
                0
            } else {
                len << 8
            };
            let entry = r + hi + len;
            let mut j = reversed as usize;
            while j < 1 << table_bits {
                table[j] = entry;
                j += 1 << len;
            }
        }
        sym += 1;
    }
    table
}

/// The codeword lengths of the static Huffman codes: litlen, then offset.
const STATIC_LENS: [u8; NUM_LITLEN_SYMS + NUM_OFFSET_SYMS] = {
    let mut lens = [0; NUM_LITLEN_SYMS + NUM_OFFSET_SYMS];
    let mut i = 0;
    while i < lens.len() {
        lens[i] = match i {
            0..144 | 280..288 => 8,
            144..256 => 9,
            256..280 => 7,
            _ => 5,
        };
        i += 1;
    }
    lens
};

/// The litlen main table of the static Huffman code, which needs no
/// subtables.
static STATIC_LITLEN_TABLE: [u32; 1 << LITLEN_TABLEBITS] = build_short_code_table(
    STATIC_LENS.split_at(NUM_LITLEN_SYMS).0,
    &LITLEN_DECODE_RESULTS,
    LITLEN_TABLEBITS,
);

/// The offset main table of the static Huffman code, likewise.
static STATIC_OFFSET_TABLE: [u32; 1 << OFFSET_TABLEBITS] = build_short_code_table(
    STATIC_LENS.split_at(NUM_LITLEN_SYMS).1,
    &OFFSET_DECODE_RESULTS,
    OFFSET_TABLEBITS,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_tables_match_built_tables() {
        let mut t = Tables::new();
        t.lens[..STATIC_LENS.len()].copy_from_slice(&STATIC_LENS);
        assert!(t.build_dynamic(NUM_LITLEN_SYMS, NUM_OFFSET_SYMS));
        assert_eq!(t.litlen[..STATIC_LITLEN_TABLE.len()], STATIC_LITLEN_TABLE);
        assert_eq!(t.offset[..STATIC_OFFSET_TABLE.len()], STATIC_OFFSET_TABLE);
    }
}
