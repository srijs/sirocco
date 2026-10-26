//! The block being built: the parser's literals and matches, the symbol
//! frequencies they make, and when to end the block. From libdeflate's
//! `deflate_compress.c`.

use super::tables::{
    offset_slot, FIRST_LEN_SYM, LENGTH_SLOT, MIN_MATCH, NUM_LITLEN_SYMS, NUM_OFFSET_SYMS,
};

/// Blocks are at least this long (bar the last before a flush or the end),
/// so that the Huffman codes pay for themselves; the block splitter also
/// doesn't work on less.
pub(super) const MIN_BLOCK_LENGTH: usize = 5000;

/// Blocks end at about this length. libdeflate uses 300000; past 64 KiB the
/// gain is under 0.02% (on Silesia), and the input of the block being built
/// has to stay in the caller's buffer.
pub(super) const SOFT_MAX_BLOCK_LENGTH: usize = 1 << 16;

/// The most matches in a block; a full store ends the block. libdeflate's
/// ratio of one per 6 bytes of `SOFT_MAX_BLOCK_LENGTH` rarely fills up.
pub(super) const SEQ_STORE_LENGTH: usize = SOFT_MAX_BLOCK_LENGTH / 6;

/// The longest block: one that reaches past the soft maximum with a lazy
/// match, or a last block allowed to grow rather than leave a short one.
pub(super) const MAX_BLOCK_LENGTH: usize = SOFT_MAX_BLOCK_LENGTH
    + if MIN_BLOCK_LENGTH > 3 + 258 {
        MIN_BLOCK_LENGTH
    } else {
        3 + 258
    };

const _: () = assert!(SEQ_STORE_LENGTH * MIN_MATCH as usize >= MIN_BLOCK_LENGTH);
// Literal runs and frequencies fit their fields (`Seq` and the Huffman
// builder's 22 bits).
const _: () = assert!(MAX_BLOCK_LENGTH < 1 << 22);

/// A run of literals and the match after it (none in the block's last).
/// The literals themselves are read from the buffer when the block is
/// written.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Seq {
    /// Bits 0-22: the literal run's length. Bits 23-31: the match's length,
    /// or 0 for none.
    litrun_len: u32,
    pub(super) offset: u16,
    pub(super) slot: u16,
}

const LEN_SHIFT: u32 = 23;

/// The sequence store, in the encoder's tables: each sequence as 4 `u16`s.
pub(super) type Seqs = [[u16; 4]; SEQ_STORE_LENGTH + 1];

/// The `u16`s of the sequence store.
pub(super) const SEQS_LEN: usize = 4 * (SEQ_STORE_LENGTH + 1);

/// Takes the sequence store from the front of `mem`.
pub(super) fn carve_seqs(mem: &mut [u16]) -> &mut Seqs {
    let (seqs, _) = mem.as_chunks_mut::<4>();
    (&mut seqs[..=SEQ_STORE_LENGTH]).try_into().unwrap()
}

impl Seq {
    pub(super) fn load(w: [u16; 4]) -> Self {
        Seq {
            litrun_len: u32::from(w[0]) | u32::from(w[1]) << 16,
            offset: w[2],
            slot: w[3],
        }
    }

    fn store(self) -> [u16; 4] {
        [
            self.litrun_len as u16,
            (self.litrun_len >> 16) as u16,
            self.offset,
            self.slot,
        ]
    }

    pub(super) fn litrun(self) -> u32 {
        self.litrun_len & ((1 << LEN_SHIFT) - 1)
    }

    /// The match length, 0 for none.
    pub(super) fn len(self) -> u32 {
        self.litrun_len >> LEN_SHIFT
    }
}

/// libdeflate's block splitter: count "observations" of literals (by their
/// top two and lowest bits) and matches (short or long), and end the block
/// when the recent distribution differs enough from the block's so far.
#[derive(Clone, Debug)]
struct SplitStats {
    new_observations: [u32; NUM_OBSERVATION_TYPES],
    observations: [u32; NUM_OBSERVATION_TYPES],
    num_new_observations: u32,
    num_observations: u32,
}

const NUM_LITERAL_OBSERVATION_TYPES: usize = 8;
const NUM_OBSERVATION_TYPES: usize = NUM_LITERAL_OBSERVATION_TYPES + 2;
const NUM_OBSERVATIONS_PER_BLOCK_CHECK: u32 = 512;

impl SplitStats {
    const NEW: SplitStats = SplitStats {
        new_observations: [0; NUM_OBSERVATION_TYPES],
        observations: [0; NUM_OBSERVATION_TYPES],
        num_new_observations: 0,
        num_observations: 0,
    };

    /// Whether the block of `len` bytes so far should end, once enough new
    /// observations are in.
    fn should_end(&mut self, len: usize) -> bool {
        if self.num_observations > 0 {
            // The sum of absolute differences of the probabilities, all
            // scaled by both observation counts.
            let mut delta = 0u32;
            for (&old, &new) in self.observations.iter().zip(&self.new_observations) {
                let expected = old * self.num_new_observations;
                let actual = new * self.num_observations;
                delta += expected.abs_diff(actual);
            }
            let items = self.num_observations + self.num_new_observations;
            // Split at a difference of 200/512, with a penalty for short
            // blocks.
            let mut cutoff = self.num_new_observations * 200 / 512 * self.num_observations;
            if len < 10000 && items < 8192 {
                cutoff += (u64::from(cutoff) * u64::from(8192 - items) / 8192) as u32;
            }
            if delta + (len as u32 / 4096) * self.num_observations >= cutoff {
                return true;
            }
        }
        for (old, new) in self.observations.iter_mut().zip(&mut self.new_observations) {
            *old += core::mem::take(new);
        }
        self.num_observations += core::mem::take(&mut self.num_new_observations);
        false
    }
}

/// The block being built.
pub(super) struct Block {
    /// The buffer position of the block's first byte.
    pub(super) start: usize,
    /// The number of sequences in the store before the open one, whose
    /// literal run is in `lits` until [`close`](Self::close).
    pub(super) n: usize,
    lits: u32,
    pub(super) litlen_freqs: [u32; NUM_LITLEN_SYMS],
    pub(super) offset_freqs: [u32; NUM_OFFSET_SYMS],
    split: SplitStats,
}

impl Block {
    pub(super) const fn new() -> Self {
        Block {
            start: 0,
            n: 0,
            lits: 0,
            litlen_freqs: [0; NUM_LITLEN_SYMS],
            offset_freqs: [0; NUM_OFFSET_SYMS],
            split: SplitStats::NEW,
        }
    }

    /// Starts a new block at buffer position `start`.
    pub(super) fn begin(&mut self, start: usize) {
        self.start = start;
        self.n = 0;
        self.lits = 0;
        self.litlen_freqs = [0; NUM_LITLEN_SYMS];
        self.offset_freqs = [0; NUM_OFFSET_SYMS];
        self.split = SplitStats::NEW;
    }

    /// Ends the open sequence's literal run, for the block to be written.
    pub(super) fn close(&mut self, seqs: &mut Seqs) {
        seqs[self.n] = Seq {
            litrun_len: self.lits,
            offset: 0,
            slot: 0,
        }
        .store();
    }

    /// The block's sequences, the open one (once [closed](Self::close))
    /// last.
    pub(super) fn seqs<'s>(&self, seqs: &'s Seqs) -> &'s [[u16; 4]] {
        &seqs[..=self.n]
    }

    /// Whether the store has room for another match.
    #[inline(always)]
    pub(super) fn has_room(&self, limit: usize) -> bool {
        self.n < limit
    }

    /// The open sequence, for a parser to keep in registers: the number of
    /// sequences before it and its literal run.
    #[inline(always)]
    pub(super) fn open_seq(&self) -> (usize, u32) {
        (self.n, self.lits)
    }

    #[inline(always)]
    pub(super) fn set_open_seq(&mut self, (n, lits): (usize, u32)) {
        (self.n, self.lits) = (n, lits);
    }

    #[inline(always)]
    pub(super) fn literal<const OBSERVE: bool>(&mut self, lit: u8) {
        self.count_literal::<OBSERVE>(lit);
        self.lits += 1;
    }

    /// Counts a literal, leaving the open sequence (as [`open_seq`]) to the
    /// caller.
    #[inline(always)]
    pub(super) fn count_literal<const OBSERVE: bool>(&mut self, lit: u8) {
        self.litlen_freqs[lit as usize] += 1;
        if OBSERVE {
            self.split.new_observations[(((lit >> 5) & 6) | (lit & 1)) as usize] += 1;
            self.split.num_new_observations += 1;
        }
    }

    #[inline(always)]
    pub(super) fn matched<const OBSERVE: bool>(&mut self, seqs: &mut Seqs, len: u32, offset: u32) {
        self.store_match::<OBSERVE>(seqs, self.n, self.lits, len, offset);
        self.n += 1;
        self.lits = 0;
    }

    /// Counts a match and stores it as sequence `n`, after a literal run of
    /// `lits`, leaving the open sequence to the caller.
    #[inline(always)]
    pub(super) fn store_match<const OBSERVE: bool>(
        &mut self,
        seqs: &mut Seqs,
        n: usize,
        lits: u32,
        len: u32,
        offset: u32,
    ) {
        let slot = offset_slot(offset);
        self.litlen_freqs[FIRST_LEN_SYM + LENGTH_SLOT[len as usize] as usize] += 1;
        self.offset_freqs[slot as usize] += 1;
        if OBSERVE {
            self.split.new_observations[NUM_LITERAL_OBSERVATION_TYPES + usize::from(len >= 9)] += 1;
            self.split.num_new_observations += 1;
        }
        seqs[n] = Seq {
            litrun_len: lits | len << LEN_SHIFT,
            offset: offset as u16,
            slot: slot as u16,
        }
        .store();
    }

    /// Whether the block should end at `pos`, from its statistics. `rest` is
    /// how much input follows, if known: the block doesn't end if what's
    /// left would make too short a block.
    #[inline(always)]
    pub(super) fn should_end(&mut self, pos: usize, rest: Option<usize>) -> bool {
        self.split.num_new_observations >= NUM_OBSERVATIONS_PER_BLOCK_CHECK
            && pos - self.start >= MIN_BLOCK_LENGTH
            && rest.is_none_or(|r| r >= MIN_BLOCK_LENGTH)
            && self.split.should_end(pos - self.start)
    }

    /// The minimum match length worth using, from how many distinct literals
    /// the block uses (libdeflate's `recalculate_min_match_len`).
    pub(super) fn recalculate_min_len(&self, depth: u32) -> u32 {
        let lits = &self.litlen_freqs[..256];
        let total: u32 = lits.iter().sum();
        // Ignore literals used very rarely.
        let cutoff = total >> 10;
        let used = lits.iter().filter(|&&f| f > cutoff).count();
        choose_min_len(used, depth)
    }
}

/// The minimum match length worth using for the greedy and lazy parsers,
/// from libdeflate. With few distinct literals, literals are cheap and
/// short matches don't pay (DNA, for one, gains a lot).
fn choose_min_len(used_literals: usize, depth: u32) -> u32 {
    const MIN_LENS: [u8; 80] = [
        9, 9, 9, 9, 9, 9, 8, 8, 7, 7, 6, 6, 6, 6, 6, 6, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
        5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
        4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
    ];
    let Some(&min_len) = MIN_LENS.get(used_literals) else {
        return MIN_MATCH;
    };
    let min_len = u32::from(min_len);
    // With a shallow search, long matches are hard to find.
    match depth {
        0..5 => min_len.min(4),
        5..10 => min_len.min(5),
        10..16 => min_len.min(7),
        _ => min_len,
    }
}

/// The minimum match length for a block starting with `data`, from the
/// distinct literals in its first 4 KiB (libdeflate's
/// `calculate_min_match_len`). `len` is the block's expected length.
pub(super) fn calculate_min_len(data: &[u8], len: usize, depth: u32) -> u32 {
    // Small inputs likely use the static code, where short matches pay.
    if len < 512 {
        return MIN_MATCH;
    }
    let mut used = [false; 256];
    for &b in &data[..data.len().min(4096)] {
        used[b as usize] = true;
    }
    choose_min_len(used.iter().filter(|&&u| u).count(), depth)
}
