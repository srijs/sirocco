//! The parsers, which choose the literals and matches: libdeflate's
//! `deflate_compress_fastest`, `_greedy` and `_lazy`, made to stop
//! when the input runs out and carry on with more.
//!
//! Each call parses until the block should end or there's too little input
//! left to look ahead into. Without a flush, the last [`LOOKAHEAD`] bytes
//! wait for more input, so that matches and lazy evaluation see as much as
//! they would in one call. At a flush or the end, everything is parsed.

use super::block::{
    calculate_min_len, carve_seqs, Block, Seqs, MIN_BLOCK_LENGTH, SEQS_LEN, SEQ_STORE_LENGTH,
    SOFT_MAX_BLOCK_LENGTH,
};
use super::lz::{Limits, LzTables, MatchFinder, LZ_TABLES_LEN, SEARCH_NBYTES};
use super::tables::{MAX_MATCH, MIN_MATCH};

/// The input a parser wants after a position, unless there is no more: a
/// longest match from each of the 2 positions lazy parsing looks at, plus
/// the 4 bytes a hash reads.
pub(super) const LOOKAHEAD: usize = MAX_MATCH as usize + 1 + 4;

/// The fastest parser's blocks: shorter, and without the block splitter
/// (as libdeflate).
const FAST_SOFT_MAX_BLOCK_LENGTH: usize = 0xFFFF;
const FAST_SEQ_STORE_LENGTH: usize = 8192;
const _: () = assert!(FAST_SEQ_STORE_LENGTH <= SEQ_STORE_LENGTH);
const _: () = assert!(FAST_SEQ_STORE_LENGTH * 4 >= MIN_BLOCK_LENGTH);

/// How a level parses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Strategy {
    /// Stored blocks only.
    Stored,
    /// Greedy, with the hash table matchfinder, `depth` entries per bucket
    /// (1 or 2).
    Fastest,
    /// Greedy, with the hash chains.
    Greedy,
    /// Takes a match unless the next position has a better one.
    Lazy,
}

/// A level's parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Params {
    pub(super) strategy: Strategy,
    /// How many hash chain candidates to try.
    pub(super) depth: u32,
    /// Stop searching at a match this long.
    pub(super) nice: u32,
    /// Fastest and greedy: the positions inside a match longer than this,
    /// but for its last, aren't searched later, as zlib's
    /// `max_insert_length`.
    pub(super) insert: u32,
    /// Fastest: after `2^accel` literals in a row, search every other
    /// position, then every third, and so on, as lz4 does. 0 for never.
    pub(super) accel: u32,
}

/// The `u16`s of the encoder's tables.
pub(super) const TABLES_LEN: usize = LZ_TABLES_LEN + SEQS_LEN;

/// The encoder's tables, borrowed for a call: the match finder's and the
/// sequence store.
pub(super) struct Tables<'a> {
    pub(super) lz: LzTables<'a>,
    pub(super) seqs: &'a mut Seqs,
}

impl<'a> Tables<'a> {
    /// The tables in `mem`, which holds at least [`TABLES_LEN`] `u16`s.
    pub(super) fn carve(mem: &'a mut [u16]) -> Self {
        assert!(
            mem.len() >= TABLES_LEN,
            "an encoder's tables hold TABLES_LEN u16s"
        );
        let (lz, rest) = LzTables::carve(mem);
        Tables {
            lz,
            seqs: carve_seqs(rest),
        }
    }
}

/// The parser's state between calls.
pub(super) struct Parser {
    pub(super) mf: MatchFinder,
    pub(super) block: Block,
    /// The next position to parse.
    pub(super) pos: usize,
    /// Whether `block` holds a block being built (rather than one being
    /// written, or none).
    pub(super) open: bool,
    min_len: u32,
    /// When to recalculate `min_len`, as an offset into the block.
    next_recalc: usize,
}

/// The index of an offset's highest set bit.
#[inline(always)]
fn bsr(x: u32) -> i32 {
    31 - x.leading_zeros() as i32
}

impl Parser {
    pub(super) const fn new() -> Self {
        Parser {
            mf: MatchFinder::new(),
            block: Block::new(),
            pos: 0,
            open: false,
            min_len: MIN_MATCH,
            next_recalc: 0,
        }
    }

    /// Opens a block at `pos`. `end` is the end of the input, if no more
    /// follows.
    pub(super) fn begin_block(&mut self, p: Params, buf: &[u8], end: Option<usize>) {
        self.block.begin(self.pos);
        self.open = true;
        let avail = &buf[self.pos..];
        let len = end.map_or(SOFT_MAX_BLOCK_LENGTH, |e| {
            (e - self.pos).min(SOFT_MAX_BLOCK_LENGTH)
        });
        // Only the hash chain parsers use the minimum match length.
        if matches!(p.strategy, Strategy::Greedy | Strategy::Lazy) {
            self.min_len = calculate_min_len(avail, len, p.depth);
            self.next_recalc = avail.len().min(10_000);
        }
    }

    /// The position the block can't go past, of a soft maximum `soft`.
    fn max_block_end(&self, soft: usize, end: Option<usize>) -> usize {
        let start = self.block.start;
        match end {
            // Rather than leave a short last block, grow this one.
            Some(end) if end - start < soft + MIN_BLOCK_LENGTH => end,
            _ => start + soft,
        }
    }

    /// Parses `buf[self.pos..]` until the block should end (returning
    /// `true`) or `stop`. `end` is the end of the input, if no more follows.
    pub(super) fn parse(
        &mut self,
        t: &mut Tables<'_>,
        p: Params,
        buf: &[u8],
        stop: usize,
        end: Option<usize>,
    ) -> bool {
        // The match finder's state in a local, which the loops can keep in
        // registers.
        let mut mf = self.mf;
        let ended = match p.strategy {
            Strategy::Stored => self.stored(stop, end),
            Strategy::Fastest if p.depth == 1 => self.fastest::<1>(&mut mf, t, p, buf, stop, end),
            Strategy::Fastest => self.fastest::<2>(&mut mf, t, p, buf, stop, end),
            Strategy::Greedy => self.greedy(&mut mf, t, p, buf, stop, end),
            Strategy::Lazy => self.lazy(&mut mf, t, p, buf, stop, end),
        };
        self.mf = mf;
        ended
    }

    fn stored(&mut self, stop: usize, end: Option<usize>) -> bool {
        let max_end = self.block.start + 0xFFFF;
        self.pos = stop.min(max_end);
        self.pos == max_end && end != Some(max_end)
    }

    #[inline(always)]
    fn fastest<const B: usize>(
        &mut self,
        mf: &mut MatchFinder,
        t: &mut Tables<'_>,
        p: Params,
        buf: &[u8],
        stop: usize,
        end: Option<usize>,
    ) -> bool {
        let max_end = self.max_block_end(FAST_SOFT_MAX_BLOCK_LENGTH, end);
        let limit = stop.min(max_end);
        let block = &mut self.block;
        let (lz, seqs) = (&mut t.lz, &mut *t.seqs);
        let mut pos = self.pos;
        let mut misses = 0u32;
        while pos < limit {
            // Computed without branching, so that the search's bounds
            // checks fold away.
            let lim = Limits::at(buf.len(), pos, p.nice);
            if lim.max < SEARCH_NBYTES {
                // Too near the end to search.
                for &b in &buf[pos..limit] {
                    block.literal::<false>(b);
                }
                pos = limit;
                break;
            }
            let (len, offset) = mf.ht_longest::<B>(lz, buf, pos, lim);
            if len > 0 {
                // Only a match can fill the store.
                if !block.has_room(FAST_SEQ_STORE_LENGTH) {
                    self.pos = pos;
                    return true;
                }
                block.matched::<false>(seqs, len, offset);
                if len <= p.insert {
                    mf.ht_skip::<B>(lz, buf, pos + 1, len - 1);
                } else {
                    // Only the match's last position, so that runs still
                    // find a close match next.
                    let last = pos + len as usize - 1;
                    mf.jump(lz, buf, last);
                    mf.ht_skip::<B>(lz, buf, last, 1);
                }
                pos += len as usize;
                misses = 0;
            } else {
                block.literal::<false>(buf[pos]);
                pos += 1;
                if p.accel > 0 {
                    misses += 1;
                    let step = misses >> p.accel;
                    if step > 0 {
                        // Skip searching the next few positions.
                        let end = (pos + step as usize).min(limit);
                        for &b in &buf[pos..end] {
                            block.literal::<false>(b);
                        }
                        pos = end;
                        mf.jump(lz, buf, pos);
                    }
                }
            }
        }
        self.pos = pos;
        pos >= max_end && end != Some(pos)
    }

    #[inline(always)]
    fn greedy(
        &mut self,
        mf: &mut MatchFinder,
        t: &mut Tables<'_>,
        p: Params,
        buf: &[u8],
        stop: usize,
        end: Option<usize>,
    ) -> bool {
        let max_end = self.max_block_end(SOFT_MAX_BLOCK_LENGTH, end);
        let limit = stop.min(max_end);
        let block = &mut self.block;
        let (lz, seqs) = (&mut t.lz, &mut *t.seqs);
        let min_len = self.min_len;
        let mut pos = self.pos;
        while pos < limit {
            let lim = Limits::at(buf.len(), pos, p.nice);
            let (len, offset) = mf.hc_longest(lz, buf, pos, min_len - 1, lim, p.depth);
            if len >= min_len && (len > MIN_MATCH || offset <= 4096) {
                block.matched::<true>(seqs, len, offset);
                if len <= p.insert {
                    mf.hc_skip(lz, buf, pos + 1, len - 1);
                } else {
                    let last = pos + len as usize - 1;
                    mf.jump(lz, buf, last);
                    mf.hc_skip(lz, buf, last, 1);
                }
                pos += len as usize;
            } else {
                block.literal::<true>(buf[pos]);
                pos += 1;
            }
            if !block.has_room(SEQ_STORE_LENGTH) || block.should_end(pos, end.map(|e| e - pos)) {
                self.pos = pos;
                return true;
            }
        }
        self.pos = pos;
        pos >= max_end && end != Some(pos)
    }

    #[inline(always)]
    fn lazy(
        &mut self,
        mf: &mut MatchFinder,
        t: &mut Tables<'_>,
        p: Params,
        buf: &[u8],
        stop: usize,
        end: Option<usize>,
    ) -> bool {
        let max_end = self.max_block_end(SOFT_MAX_BLOCK_LENGTH, end);
        let limit = stop.min(max_end);
        let block = &mut self.block;
        let (lz, seqs) = (&mut t.lz, &mut *t.seqs);
        let mut pos = self.pos;
        while pos < limit {
            // Recalculate the minimum match length now and then, at
            // doubling intervals.
            if pos - block.start >= self.next_recalc {
                self.min_len = block.recalculate_min_len(p.depth);
                self.next_recalc += (pos - block.start).max(1);
            }
            let min_len = self.min_len;
            let lim = Limits::at(buf.len(), pos, p.nice);
            let (mut cur_len, mut cur_offset) =
                mf.hc_longest(lz, buf, pos, min_len - 1, lim, p.depth);
            if cur_len < min_len || (cur_len == MIN_MATCH && cur_offset > 8192) {
                block.literal::<true>(buf[pos]);
                pos += 1;
            } else {
                pos += 1;
                loop {
                    // A match at `pos - 1`: take it if it's long enough...
                    if cur_len >= lim.nice {
                        block.matched::<true>(seqs, cur_len, cur_offset);
                        mf.hc_skip(lz, buf, pos, cur_len - 1);
                        pos += cur_len as usize - 1;
                        break;
                    }
                    // ...or a better one at the next position, with half
                    // the search depth.
                    let lim = Limits::at(buf.len(), pos, p.nice);
                    let depth = (p.depth >> 1).max(1);
                    let (next_len, next_offset) =
                        mf.hc_longest(lz, buf, pos, cur_len - 1, lim, depth);
                    pos += 1;
                    if next_len >= cur_len
                        && 4 * (next_len - cur_len) as i32 + bsr(cur_offset) - bsr(next_offset) > 2
                    {
                        block.literal::<true>(buf[pos - 2]);
                        (cur_len, cur_offset) = (next_len, next_offset);
                        continue;
                    }
                    block.matched::<true>(seqs, cur_len, cur_offset);
                    mf.hc_skip(lz, buf, pos, cur_len - 2);
                    pos += cur_len as usize - 2;
                    break;
                }
            }
            if !block.has_room(SEQ_STORE_LENGTH) || block.should_end(pos, end.map(|e| e - pos)) {
                self.pos = pos;
                return true;
            }
        }
        self.pos = pos;
        pos >= max_end && end != Some(pos)
    }
}
