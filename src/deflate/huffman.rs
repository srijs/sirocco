//! Length-limited canonical Huffman codes, ported from libdeflate's
//! `deflate_make_huffman_code` (which credits 7-Zip's `HuffEnc.c`).
//!
//! See libdeflate for the full rationale. In short: sort the symbols by
//! frequency, build only the non-leaf nodes of the tree, in place, then read
//! the codeword lengths off the tree and assign canonical codewords.

/// The low bits of a work entry hold a symbol, the rest a frequency, a
/// parent index or a depth.
const NUM_SYMBOL_BITS: u32 = 10;
const SYMBOL_MASK: u32 = (1 << NUM_SYMBOL_BITS) - 1;
const FREQ_MASK: u32 = !SYMBOL_MASK;

/// The longest codeword DEFLATE allows.
const MAX_CODEWORD_LEN: usize = 15;

/// The largest alphabet: litlen.
const MAX_NUM_SYMS: usize = 288;

/// Builds a canonical Huffman code for `freqs` with no codeword longer than
/// `max_len` bits: each symbol's codeword length in `lens` (0 if unused)
/// and its codeword, bit-reversed as DEFLATE writes it, in `codewords`.
///
/// The frequencies must sum to less than 2^22. At least two codewords are
/// always made, as zlib and other compressors do: some decompressors reject
/// a code with fewer.
pub(super) fn make_code(freqs: &[u32], max_len: u32, lens: &mut [u8], codewords: &mut [u32]) {
    let n = freqs.len();
    debug_assert!(n <= MAX_NUM_SYMS && lens.len() == n && codewords.len() == n);
    debug_assert!(max_len as usize <= MAX_CODEWORD_LEN && (1 << max_len) >= n);
    // `codewords` doubles as the work array.
    let a = codewords;
    let used = sort_symbols(freqs, lens, a);
    if used < 2 {
        // Codeword 0 for symbol 0 and 1 for the used symbol, or symbol 1.
        let sym = if used == 1 {
            (a[0] & SYMBOL_MASK) as usize
        } else {
            0
        };
        let other = if sym == 0 { 1 } else { sym };
        a[0] = 0;
        lens[0] = 1;
        a[other] = 1;
        lens[other] = 1;
        return;
    }
    build_tree(&mut a[..used]);
    let mut len_counts = [0u32; MAX_CODEWORD_LEN + 1];
    compute_length_counts(&mut a[..used - 1], &mut len_counts, max_len);
    gen_codewords(a, lens, &len_counts, max_len, used);
}

/// Sorts the used symbols by frequency, then symbol, into `a` as
/// `symbol | freq << NUM_SYMBOL_BITS`, and sets the unused symbols' lengths
/// (and later codewords) to 0. Returns the number of used symbols.
///
/// Most frequencies are low, so this is a counting sort with one counter per
/// symbol, and only the frequencies that land in the last counter are heap
/// sorted.
fn sort_symbols(freqs: &[u32], lens: &mut [u8], a: &mut [u32]) -> usize {
    let n = freqs.len();
    let mut counters = [0u32; MAX_NUM_SYMS];
    let counters = &mut counters[..n];
    // Unused symbols, most of them in small blocks, are skipped rather than
    // counted: incrementing counter 0 for each would chain every increment
    // on the last.
    for &f in freqs {
        if f != 0 {
            counters[(f as usize).min(n - 1)] += 1;
        }
    }
    // Cumulative counts, skipping the unused symbols in counter 0.
    let mut used = 0;
    for c in &mut counters[1..] {
        let count = *c;
        *c = used;
        used += count;
    }
    for (sym, (&f, len)) in freqs.iter().zip(lens.iter_mut()).enumerate() {
        if f == 0 {
            *len = 0;
        } else {
            let c = &mut counters[(f as usize).min(n - 1)];
            a[*c as usize] = sym as u32 | (f << NUM_SYMBOL_BITS);
            *c += 1;
        }
    }
    // Counter n - 1 counted the highest frequencies, which need sorting.
    let (lo, hi) = (counters[n - 2] as usize, counters[n - 1] as usize);
    a[lo..hi].sort_unstable();
    used as usize
}

/// Builds the non-leaf nodes of a Huffman tree from the sorted frequencies
/// in `a`, in place: entry `i` becomes the `i`th non-leaf node, holding the
/// index of its parent (the root, at `a.len() - 2`, holds garbage). The low
/// symbol bits are kept as they were.
fn build_tree(a: &mut [u32]) {
    let last = a.len() - 1;
    // The next leaf, the next unparented non-leaf, and the next free slot.
    let (mut i, mut b, mut e) = (0, 0, 0);
    loop {
        let new_freq;
        if i < last && (b == e || (a[i + 1] & FREQ_MASK) <= (a[b] & FREQ_MASK)) {
            // Two leaves.
            new_freq = (a[i] & FREQ_MASK) + (a[i + 1] & FREQ_MASK);
            i += 2;
        } else if b + 2 <= e && (i > last || (a[b + 1] & FREQ_MASK) < (a[i] & FREQ_MASK)) {
            // Two non-leaves.
            new_freq = (a[b] & FREQ_MASK) + (a[b + 1] & FREQ_MASK);
            a[b] = ((e as u32) << NUM_SYMBOL_BITS) | (a[b] & SYMBOL_MASK);
            a[b + 1] = ((e as u32) << NUM_SYMBOL_BITS) | (a[b + 1] & SYMBOL_MASK);
            b += 2;
        } else {
            // A leaf and a non-leaf.
            new_freq = (a[i] & FREQ_MASK) + (a[b] & FREQ_MASK);
            a[b] = ((e as u32) << NUM_SYMBOL_BITS) | (a[b] & SYMBOL_MASK);
            i += 1;
            b += 1;
        }
        a[e] = new_freq | (a[e] & SYMBOL_MASK);
        e += 1;
        if e >= last {
            return;
        }
    }
}

/// Counts the codewords of each length from the non-leaf nodes `a` (root
/// last), limiting lengths to `max_len`.
///
/// Walking the nodes root first, each node turns one codeword at its depth
/// into two at the next. A node deeper than allowed takes the deepest
/// length that still has a codeword to split instead, which is not optimal
/// but close.
fn compute_length_counts(a: &mut [u32], len_counts: &mut [u32], max_len: u32) {
    let root = a.len() - 1;
    let max_len = max_len as usize;
    len_counts[1] = 2;
    // Depths overwrite the parent indices.
    a[root] &= SYMBOL_MASK;
    for node in (0..root).rev() {
        let parent = (a[node] >> NUM_SYMBOL_BITS) as usize;
        let mut depth = (a[parent] >> NUM_SYMBOL_BITS) as usize + 1;
        a[node] = (a[node] & SYMBOL_MASK) | ((depth as u32) << NUM_SYMBOL_BITS);
        if depth >= max_len {
            depth = max_len - 1;
            while len_counts[depth] == 0 {
                depth -= 1;
            }
        }
        len_counts[depth] -= 1;
        len_counts[depth + 1] += 2;
    }
}

/// Assigns lengths to the symbols, longest to the least frequent, then
/// canonical codewords in symbol order. `a` holds the `used` symbols sorted
/// by frequency in its low bits on entry and the codewords on return.
fn gen_codewords(a: &mut [u32], lens: &mut [u8], len_counts: &[u32], max_len: u32, used: usize) {
    let mut i = 0;
    for len in (1..=max_len).rev() {
        for _ in 0..len_counts[len as usize] {
            lens[(a[i] & SYMBOL_MASK) as usize] = len as u8;
            i += 1;
        }
    }
    debug_assert_eq!(i, used);
    let mut next = [0u32; MAX_CODEWORD_LEN + 1];
    for len in 2..=max_len as usize {
        next[len] = (next[len - 1] + len_counts[len - 1]) << 1;
    }
    for (cw, &len) in a.iter_mut().zip(lens.iter()) {
        // As in sort_symbols, unused symbols skip the counter.
        if len == 0 {
            *cw = 0;
            continue;
        }
        *cw = reverse(next[len as usize], len);
        next[len as usize] += 1;
    }
}

/// `codeword`'s low `len` bits, reversed, as DEFLATE writes codewords.
#[inline]
const fn reverse(codeword: u32, len: u8) -> u32 {
    // Unused symbols have length 0; their codeword doesn't matter.
    codeword.reverse_bits().wrapping_shr(32 - len as u32)
}

/// The canonical codewords for `lens`, at compile time.
pub(super) const fn canonical_codewords<const N: usize>(lens: &[u8; N]) -> [u32; N] {
    let mut counts = [0u32; MAX_CODEWORD_LEN + 1];
    let mut i = 0;
    while i < N {
        counts[lens[i] as usize] += 1;
        i += 1;
    }
    counts[0] = 0;
    let mut next = [0u32; MAX_CODEWORD_LEN + 1];
    let mut len = 1;
    while len <= MAX_CODEWORD_LEN {
        next[len] = (next[len - 1] + counts[len - 1]) << 1;
        len += 1;
    }
    let mut out = [0u32; N];
    i = 0;
    while i < N {
        let len = lens[i];
        out[i] = reverse(next[len as usize], len);
        next[len as usize] += 1;
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kraft's sum of the code, in units of 2^-15: exactly 2^15 for a
    /// complete code.
    fn kraft(lens: &[u8]) -> u32 {
        lens.iter()
            .filter(|&&l| l > 0)
            .map(|&l| 1 << (15 - l))
            .sum()
    }

    #[test]
    fn codes_are_complete_and_limited() {
        let mut rng = 0x1234_5678_9abc_def0_u64;
        for round in 0..2000 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let n = [19, 30, 32, 288][round % 4];
            let max_len = [7, 15, 15, 14][round % 4];
            // Skewed frequencies, with many zeros, to hit the length limit.
            // They must sum to under 2^22.
            let mut freqs = [0u32; MAX_NUM_SYMS];
            for (i, f) in freqs[..n].iter_mut().enumerate() {
                let r = rng.rotate_left(i as u32 * 7);
                *f = match r % 4 {
                    0 => 0,
                    1 => 1 << ((r >> 8) % 13),
                    _ => (r >> 16) as u32 % 50,
                };
            }
            let (mut lens, mut cws) = ([0u8; MAX_NUM_SYMS], [0u32; MAX_NUM_SYMS]);
            make_code(&freqs[..n], max_len, &mut lens[..n], &mut cws[..n]);
            let used = freqs[..n].iter().filter(|&&f| f > 0).count();
            assert!(lens[..n].iter().all(|&l| u32::from(l) <= max_len));
            assert_eq!(kraft(&lens[..n]), 1 << 15, "round {round}");
            for s in 0..n {
                assert!(freqs[s] == 0 || lens[s] > 0 || used < 2);
            }
            // Prefix-free: no codeword is a prefix of another (reversed, so
            // a prefix is a suffix of the low bits).
            for s in 0..n {
                for t in 0..n {
                    let (ls, lt) = (lens[s], lens[t]);
                    if s != t && ls > 0 && lt > 0 && ls <= lt {
                        assert_ne!(cws[s], cws[t] & ((1 << ls) - 1), "{s} prefixes {t}");
                    }
                }
            }
        }
    }

    #[test]
    fn optimal_when_unconstrained() {
        // Frequencies 1, 1, 2, 4, 8: lengths 4, 4, 3, 2, 1.
        let freqs = [8, 1, 4, 1, 2];
        let (mut lens, mut cws) = ([0; 5], [0; 5]);
        make_code(&freqs, 15, &mut lens, &mut cws);
        assert_eq!(lens, [1, 4, 2, 4, 3]);
    }

    #[test]
    fn fewer_than_two_symbols() {
        let (mut lens, mut cws) = ([9; 4], [0; 4]);
        make_code(&[0, 0, 0, 0], 15, &mut lens, &mut cws);
        assert_eq!(lens, [1, 1, 0, 0]);
        make_code(&[0, 0, 5, 0], 15, &mut lens, &mut cws);
        assert_eq!(lens, [1, 0, 1, 0]);
        make_code(&[5, 0, 0, 0], 15, &mut lens, &mut cws);
        assert_eq!(lens, [1, 1, 0, 0]);
    }

    #[test]
    fn canonical_matches_built() {
        let lens = [3u8, 3, 3, 3, 3, 2, 4, 4];
        let built = canonical_codewords(&lens);
        // RFC 1951's example: F=00, A..E=010..110, G=1110, H=1111.
        let expect = [0b010, 0b011, 0b100, 0b101, 0b110, 0b00, 0b1110, 0b1111];
        for i in 0..8 {
            assert_eq!(built[i], reverse(expect[i], lens[i]));
        }
    }
}
