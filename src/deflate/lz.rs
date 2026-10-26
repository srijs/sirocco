//! Match finding, ported from libdeflate's `hc_matchfinder.h` and
//! `ht_matchfinder.h`.
//!
//! Positions are stored as `i16`s relative to [`MatchFinder::base`], a
//! position in the caller's buffer. Every 32 KiB the base moves up a window
//! and the tables are rebased, with saturation: anything that falls out of
//! the window becomes [`EMPTY`], which is never in range again.
//!
//! - The hash chains (`hc`) keep, for each 4-byte hash, a chain of earlier
//!   positions through `next`, searched to a depth, plus one position per
//!   3-byte hash for close length-3 matches.
//! - The hash table (`ht`) keeps the last one or two positions per 4-byte
//!   hash in `hash4`, and finds matches of 4 bytes or more. It is for the
//!   fastest levels.
//!
//! Small inputs given whole use fewer hash bits, so that clearing the tables
//! doesn't cost more than compressing.

use super::tables::{MAX_MATCH, WINDOW};

/// A position before the window: never a match.
const EMPTY: i16 = i16::MIN;
const EMPTY_BITS: u16 = EMPTY as u16;

const HASH4_ORDER: u32 = 16;
const HASH3_ORDER: u32 = 15;
/// The ht matchfinder's buckets live in `hash4`, two entries each.
const HT_ORDER: u32 = HASH4_ORDER - 1;
/// The fewest hash bits used, however small the input.
const MIN_ORDER: u32 = 10;
/// The most hash bits that small inputs get an extra bit up to.
const SMALL_ORDER: u32 = 14;

/// The minimum bytes at a position for a match search: the matchfinders
/// read 4 bytes at the next position.
pub(super) const SEARCH_NBYTES: u32 = 5;

/// How long a match may be at a position, and the length to stop searching
/// at.
#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    pub(super) max: u32,
    pub(super) nice: u32,
}

impl Limits {
    /// The limits at `pos` with `end` bytes of input: matches can't run past
    /// the end.
    #[inline(always)]
    pub(super) fn at(end: usize, pos: usize, nice: u32) -> Self {
        let max = (end - pos).min(MAX_MATCH as usize) as u32;
        Limits {
            max,
            nice: nice.min(max),
        }
    }
}

/// The match-finding tables, in the encoder's tables: positions as `i16`
/// bits.
pub(super) struct LzTables<'a> {
    /// hc: the first position of each length-4 hash chain. ht: the
    /// buckets, one or two positions each.
    pub(super) hash4: &'a mut [u16; 1 << HASH4_ORDER],
    /// hc: the last position of each length-3 hash.
    pub(super) hash3: &'a mut [u16; 1 << HASH3_ORDER],
    /// hc: the next older position in the chain of the position `i` (mod
    /// the window).
    pub(super) next: &'a mut [u16; WINDOW],
}

/// The `u16`s of tables [`LzTables`] takes.
pub(super) const LZ_TABLES_LEN: usize = (1 << HASH4_ORDER) + (1 << HASH3_ORDER) + WINDOW;

impl<'a> LzTables<'a> {
    /// Takes the tables from the front of `mem`, returning the rest.
    pub(super) fn carve(mem: &'a mut [u16]) -> (Self, &'a mut [u16]) {
        let (hash4, mem) = mem.split_first_chunk_mut().unwrap();
        let (hash3, mem) = mem.split_first_chunk_mut().unwrap();
        let (next, mem) = mem.split_first_chunk_mut().unwrap();
        (LzTables { hash4, hash3, next }, mem)
    }
}

/// The match finder's state between calls; its tables are [`LzTables`].
#[derive(Clone, Copy)]
pub(super) struct MatchFinder {
    /// The buffer position that stored positions are relative to.
    pub(super) base: usize,
    /// Whether the base has moved since the reset: the tables have seen a
    /// window of input.
    pub(super) warm: bool,
    /// The ht matchfinder's bucket size, 1 or 2, or 0 for hc.
    ht: usize,
    /// The hashes' shifts: 32 less their bits.
    shift4: u32,
    shift3: u32,
    /// The hashes of the next position, precomputed as libdeflate does: hc's
    /// length-3 and length-4 hashes, or ht's bucket.
    next_hashes: [usize; 2],
    /// The table entries at `next_hashes`, loaded early: hc's two heads, or
    /// ht's bucket. That stands in for libdeflate's prefetches: the next
    /// search finds them in cache, and ht's uses the bucket as loaded.
    next_heads: [u16; 2],
}

const MASK4: usize = (1 << HASH4_ORDER) - 1;
const MASK3: usize = (1 << HASH3_ORDER) - 1;

/// The hash of `seq`: the high bits of a multiplicative hash, `32 - shift`
/// of them.
#[inline(always)]
fn hash(seq: u32, shift: u32) -> usize {
    (seq.wrapping_mul(0x1E35_A7BD) >> shift) as usize
}

#[inline(always)]
pub(super) fn load32(buf: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes(*buf[pos..].first_chunk().unwrap())
}

/// The length of the match between `a` and `b`, which have the same length,
/// knowing their first `start` bytes match.
#[inline(always)]
pub(super) fn extend(a: &[u8], b: &[u8], start: usize) -> u32 {
    let b = &b[..a.len()];
    let mut len = start;
    while len + 8 <= a.len() {
        let x = u64::from_le_bytes(a[len..len + 8].try_into().unwrap());
        let y = u64::from_le_bytes(b[len..len + 8].try_into().unwrap());
        if x != y {
            return (len + ((x ^ y).trailing_zeros() >> 3) as usize) as u32;
        }
        len += 8;
    }
    while len < a.len() && a[len] == b[len] {
        len += 1;
    }
    len as u32
}

/// Moves a position back a window, saturating at [`EMPTY`], as libdeflate
/// does (and in one instruction per vector).
#[inline(always)]
fn rebase_one(x: u16) -> u16 {
    (x as i16).saturating_add(i16::MIN) as u16
}

/// Moves every position in `t` back a window. One copy, out of line: it is
/// cold, and vectorized it is big.
#[inline(never)]
fn rebase(t: &mut [u16]) {
    for x in t {
        *x = rebase_one(*x);
    }
}

/// Moves the positions in the tables back `windows` windows (2 or more
/// leave none).
#[cold]
#[inline(never)]
fn slide(heads: &mut [u16], hash3: &mut [u16], next: &mut [u16], windows: usize) {
    for _ in 0..windows.min(2) {
        rebase(heads);
        rebase(hash3);
        rebase(next);
    }
}

/// The input around a search at `cur`: the bytes before it, which matches
/// come from, and `max` bytes from it. Taking candidates as `max`-byte
/// slices of `win` that start before `cur` lets the compiler drop the
/// bounds checks of everything read from them.
struct Search<'a> {
    win: &'a [u8],
    cur: usize,
    max: usize,
    base: usize,
}

impl<'a> Search<'a> {
    #[inline(always)]
    fn new(buf: &'a [u8], cur: usize, max: usize, base: usize) -> Self {
        // Clamped (it never is) to `buf`, which shows the compiler that the
        // window fits in it.
        let win = &buf[..(cur + max).min(buf.len())];
        let max = win[cur..].len();
        Search {
            win,
            cur,
            max,
            base,
        }
    }

    /// The bytes being matched.
    #[inline(always)]
    fn here(&self) -> &'a [u8] {
        &self.win[self.cur..]
    }

    /// The candidate at stored position `node` and its buffer position, or
    /// `None` if it isn't before `cur` (which only a corrupt table could
    /// make it).
    #[inline(always)]
    fn candidate(&self, node: i16) -> Option<(&'a [u8], usize)> {
        let m = self.base.wrapping_add_signed(node as isize);
        if m >= self.cur {
            return None;
        }
        Some((&self.win[m..][..self.max], m))
    }
}

impl MatchFinder {
    pub(super) const fn new() -> Self {
        MatchFinder {
            base: 0,
            warm: false,
            ht: 0,
            shift4: 32 - HASH4_ORDER,
            shift3: 32 - HASH3_ORDER,
            next_hashes: [0; 2],
            next_heads: [EMPTY_BITS; 2],
        }
    }

    /// Forgets all positions; `base` becomes `pos`. With `len`, the whole
    /// input that will be searched is that long, so the tables can be
    /// smaller.
    /// `ht` is the ht matchfinder's bucket size, or 0 for hc.
    pub(super) fn reset(
        &mut self,
        tables: &mut LzTables<'_>,
        pos: usize,
        ht: usize,
        len: Option<usize>,
    ) {
        let order = if ht == 2 { HT_ORDER } else { HASH4_ORDER };
        let bits = match len {
            Some(n) => {
                // A table about the input's size, or twice it while that
                // stays in L1 cache: fewer collisions cost less than
                // clearing the extra entries.
                let bits = usize::BITS - n.leading_zeros();
                bits.max((bits + 1).min(SMALL_ORDER))
                    .clamp(MIN_ORDER, order)
            }
            None => order,
        };
        self.ht = ht;
        self.shift4 = 32 - bits;
        self.shift3 = 32 - bits.min(HASH3_ORDER);
        self.base = pos;
        self.warm = false;
        self.next_hashes = [0; 2];
        self.next_heads = [EMPTY_BITS; 2];
        self.heads(tables).fill(EMPTY_BITS);
        if ht == 0 {
            let n3 = 1 << (32 - self.shift3);
            tables.hash3[..n3].fill(EMPTY_BITS);
        }
        // `next` needs no clearing: entries are only read through chains
        // from the heads, which write them first.
    }

    /// The part of `hash4` in use.
    fn heads<'t>(&self, tables: &'t mut LzTables<'_>) -> &'t mut [u16] {
        let n = (1 << (32 - self.shift4)) * self.ht.max(1);
        &mut tables.hash4[..n]
    }

    /// The position relative to the base of buffer position `cur`, rebasing
    /// first if it's past the window.
    #[inline(always)]
    fn relative(&mut self, tables: &mut LzTables<'_>, cur: usize) -> usize {
        let rel = cur - self.base;
        if rel < WINDOW {
            return rel;
        }
        // The cold call gets only the tables, so that the match finder's
        // state (and the rest of the parser's) can stay in registers
        // through the loops that inline this.
        let windows = rel / WINDOW;
        let n4 = (1 << (32 - self.shift4)) * self.ht.max(1);
        let n3 = if self.ht == 0 {
            1 << (32 - self.shift3)
        } else {
            0
        };
        let next = if self.ht == 0 {
            &mut tables.next[..]
        } else {
            &mut []
        };
        slide(
            &mut tables.hash4[..n4],
            &mut tables.hash3[..n3],
            next,
            windows,
        );
        for _ in 0..windows.min(2) {
            self.next_heads = self.next_heads.map(rebase_one);
        }
        self.base += windows * WINDOW;
        self.warm = true;
        cur - self.base
    }

    /// The position relative to the base of `cur`, for inserting `count`
    /// positions from it: rebasing first if the last would be past the
    /// window, as libdeflate does, rather than checking at each. The first
    /// positions are then before the base, wrapped below zero.
    #[inline(always)]
    fn relative_span(&mut self, tables: &mut LzTables<'_>, cur: usize, count: usize) -> usize {
        let rel = self.relative(tables, cur);
        if rel + count <= WINDOW {
            return rel;
        }
        self.relative(tables, cur + count - 1);
        cur.wrapping_sub(self.base)
    }

    /// Rebases if `cur` is a window or more past the base, so that the base
    /// is within a window of it.
    pub(super) fn catch_up(&mut self, tables: &mut LzTables<'_>, cur: usize) {
        self.relative(tables, cur);
    }

    /// Moves to `cur` without inserting the positions skipped.
    #[inline(always)]
    pub(super) fn jump(&mut self, tables: &mut LzTables<'_>, buf: &[u8], cur: usize) {
        let Some(next) = buf.get(cur..cur + 4) else {
            return;
        };
        let seq = u32::from_le_bytes(next.try_into().unwrap());
        if self.ht > 0 {
            let b = (hash(seq, self.shift4) * self.ht) & MASK4 & !(self.ht - 1);
            self.next_hashes[0] = b;
            self.next_heads = if self.ht == 1 {
                ht_bucket::<1>(tables, b)
            } else {
                ht_bucket::<2>(tables, b)
            };
        } else {
            let h3 = hash(seq & 0xFF_FFFF, self.shift3) & MASK3;
            let h4 = hash(seq, self.shift4) & MASK4;
            self.next_hashes = [h3, h4];
            self.next_heads = [tables.hash3[h3], tables.hash4[h4]];
        }
    }

    /// hc: finds the longest match at `cur` longer than `best_len`, up to
    /// `max_len`, stopping at one of `nice_len` or after `depth` candidates,
    /// and inserts `cur`. Returns the length (`best_len` if none longer) and
    /// offset (0 if none).
    ///
    /// `cur` must be the position after the last one inserted or searched.
    #[inline(always)]
    pub(super) fn hc_longest(
        &mut self,
        tables: &mut LzTables<'_>,
        buf: &[u8],
        cur: usize,
        mut best_len: u32,
        limits: Limits,
        depth: u32,
    ) -> (u32, u32) {
        let Limits {
            max: max_len,
            nice: nice_len,
        } = limits;
        let rel = self.relative(tables, cur);
        let cutoff = rel as i32 - WINDOW as i32;
        if max_len < SEARCH_NBYTES {
            return (best_len, 0);
        }
        let search = Search::new(buf, cur, max_len as usize, self.base);
        let here = search.here();
        let [h3, h4] = self.next_hashes;
        let [node3, mut node4] = self.next_heads.map(|n| n as i16);
        tables.hash3[h3 & MASK3] = rel as u16;
        tables.hash4[h4 & MASK4] = rel as u16;
        tables.next[rel & (WINDOW - 1)] = node4 as u16;
        let next_seq = load32(here, 1);
        let n3 = hash(next_seq & 0xFF_FFFF, self.shift3) & MASK3;
        let n4 = hash(next_seq, self.shift4) & MASK4;
        self.next_hashes = [n3, n4];
        self.next_heads = [tables.hash3[n3], tables.hash4[n4]];

        let in_window = |node: i16| i32::from(node) > cutoff;
        let mut depth = depth;
        let mut best = cur;
        let seq4 = load32(here, 0);
        'out: {
            if best_len < 4 {
                if !in_window(node3) {
                    break 'out;
                }
                if best_len < 3 {
                    if let Some((c, m)) = search.candidate(node3) {
                        if load32(c, 0) << 8 == seq4 << 8 {
                            best_len = 3;
                            best = m;
                        }
                    }
                }
                if !in_window(node4) {
                    break 'out;
                }
                // The first length-4 match.
                let (c, m) = loop {
                    let Some((c, m)) = search.candidate(node4) else {
                        break 'out;
                    };
                    if load32(c, 0) == seq4 {
                        break (c, m);
                    }
                    node4 = tables.next[node4 as usize & (WINDOW - 1)] as i16;
                    depth -= 1;
                    if !in_window(node4) || depth == 0 {
                        break 'out;
                    }
                };
                best = m;
                best_len = extend(here, c, 4);
                if best_len >= nice_len {
                    break 'out;
                }
                node4 = tables.next[node4 as usize & (WINDOW - 1)] as i16;
                depth -= 1;
                if !in_window(node4) || depth == 0 {
                    break 'out;
                }
            } else if !in_window(node4) || best_len >= nice_len {
                break 'out;
            }
            // Longer matches: check the 4 bytes ending one past the best
            // length first, then the first 4.
            loop {
                let tail = best_len as usize - 3;
                let here_tail = load32(here, tail);
                let (c, m) = loop {
                    let Some((c, m)) = search.candidate(node4) else {
                        break 'out;
                    };
                    if load32(c, tail) == here_tail && load32(c, 0) == seq4 {
                        break (c, m);
                    }
                    node4 = tables.next[node4 as usize & (WINDOW - 1)] as i16;
                    depth -= 1;
                    if !in_window(node4) || depth == 0 {
                        break 'out;
                    }
                };
                let len = extend(here, c, 4);
                if len > best_len {
                    best_len = len;
                    best = m;
                    if best_len >= nice_len {
                        break 'out;
                    }
                }
                node4 = tables.next[node4 as usize & (WINDOW - 1)] as i16;
                depth -= 1;
                if !in_window(node4) || depth == 0 {
                    break 'out;
                }
            }
        }
        (best_len, (cur - best) as u32)
    }

    /// hc: inserts the `count` positions from `cur` without searching, unless
    /// they're too near `buf`'s end.
    #[inline(always)]
    pub(super) fn hc_skip(
        &mut self,
        tables: &mut LzTables<'_>,
        buf: &[u8],
        cur: usize,
        count: u32,
    ) {
        let count = count as usize;
        if count + SEARCH_NBYTES as usize > buf.len() - cur {
            return;
        }
        let [mut h3, mut h4] = self.next_hashes;
        let mut rel = self.relative_span(tables, cur, count);
        // The 4 bytes at each position after one inserted.
        for next in buf[cur + 1..cur + count + 4].windows(4) {
            tables.hash3[h3 & MASK3] = rel as u16;
            tables.next[rel & (WINDOW - 1)] = tables.hash4[h4 & MASK4];
            tables.hash4[h4 & MASK4] = rel as u16;
            let seq = u32::from_le_bytes(next.try_into().unwrap());
            h3 = hash(seq & 0xFF_FFFF, self.shift3) & MASK3;
            h4 = hash(seq, self.shift4) & MASK4;
            rel = rel.wrapping_add(1);
        }
        self.next_hashes = [h3, h4];
        self.next_heads = [tables.hash3[h3], tables.hash4[h4]];
    }

    /// ht: finds the longest match of 4 or more at `cur`, up to `max_len`
    /// (at least [`SEARCH_NBYTES`]), and inserts `cur`. Returns the length
    /// and offset of the match, or `(0, 0)` for none.
    #[inline(always)]
    pub(super) fn ht_longest<const B: usize>(
        &mut self,
        tables: &mut LzTables<'_>,
        buf: &[u8],
        cur: usize,
        limits: Limits,
    ) -> (u32, u32) {
        let Limits {
            max: max_len,
            nice: nice_len,
        } = limits;
        let rel = self.relative(tables, cur);
        let cutoff = rel as i32 - WINDOW as i32;
        let search = Search::new(buf, cur, max_len as usize, self.base);
        let here = search.here();
        let mask = MASK4 & !(B - 1);
        let b = self.next_hashes[0] & mask;
        let nb = (hash(load32(here, 1), self.shift4) * B) & mask;
        self.next_hashes[0] = nb;
        let seq = load32(here, 0);
        // The bucket as loaded early, so that the search doesn't wait for it.
        let [first, second] = self.next_heads;
        if B == 1 {
            tables.hash4[b] = rel as u16;
        } else {
            let bucket: &mut [u16; 2] = (&mut tables.hash4[b..b + 2]).try_into().unwrap();
            *bucket = [rel as u16, first];
        }
        let (first, second) = (first as i16, if B == 1 { EMPTY } else { second as i16 });
        self.next_heads = ht_bucket::<B>(tables, nb);

        let in_window = |node: i16| i32::from(node) > cutoff;
        let (mut best_len, mut best) = (0, cur);
        if !in_window(first) {
            return (0, 0);
        }
        let Some((c, m)) = search.candidate(first) else {
            return (0, 0);
        };
        if load32(c, 0) == seq {
            best_len = extend(here, c, 4);
            best = m;
            if B == 1 || !in_window(second) || best_len >= nice_len {
                return (best_len, (cur - best) as u32);
            }
            let tail = best_len as usize - 3;
            if let Some((c, m)) = search.candidate(second) {
                if load32(c, 0) == seq && load32(c, tail) == load32(here, tail) {
                    let len = extend(here, c, 4);
                    if len > best_len {
                        best_len = len;
                        best = m;
                    }
                }
            }
        } else if B == 2 && in_window(second) {
            if let Some((c, m)) = search.candidate(second) {
                if load32(c, 0) == seq {
                    best_len = extend(here, c, 4);
                    best = m;
                }
            }
        }
        (best_len, (cur - best) as u32)
    }

    /// ht: as [`ht_longest`](Self::ht_longest), but only finds a match
    /// longer than `min`, which is 0 or from 4 to less than `max_len`.
    #[inline(always)]
    pub(super) fn ht_longer<const B: usize>(
        &mut self,
        tables: &mut LzTables<'_>,
        buf: &[u8],
        cur: usize,
        limits: Limits,
        min: u32,
    ) -> (u32, u32) {
        let Limits {
            max: max_len,
            nice: nice_len,
        } = limits;
        let rel = self.relative(tables, cur);
        let cutoff = rel as i32 - WINDOW as i32;
        let search = Search::new(buf, cur, max_len as usize, self.base);
        let here = search.here();
        let mask = MASK4 & !(B - 1);
        let b = self.next_hashes[0] & mask;
        let nb = (hash(load32(here, 1), self.shift4) * B) & mask;
        self.next_hashes[0] = nb;
        let seq = load32(here, 0);
        // The bucket as loaded early, so that the search doesn't wait for it.
        let [first, second] = self.next_heads;
        if B == 1 {
            tables.hash4[b] = rel as u16;
        } else {
            let bucket: &mut [u16; 2] = (&mut tables.hash4[b..b + 2]).try_into().unwrap();
            *bucket = [rel as u16, first];
        }
        let (first, second) = (first as i16, if B == 1 { EMPTY } else { second as i16 });
        self.next_heads = ht_bucket::<B>(tables, nb);

        let in_window = |node: i16| i32::from(node) > cutoff;
        // Whether candidate `c` can be longer than `len`: it matches the
        // first 4 bytes, and the 4 ending one past `len`.
        let may_beat = |c: &[u8], len: u32| {
            load32(c, 0) == seq && {
                let tail = len.max(4) as usize - 3;
                len < 4 || load32(c, tail) == load32(here, tail)
            }
        };
        let (mut best_len, mut best) = (min, cur);
        if !in_window(first) {
            return (0, 0);
        }
        let Some((c, m)) = search.candidate(first) else {
            return (0, 0);
        };
        if may_beat(c, best_len) {
            let len = extend(here, c, 4);
            if len > best_len {
                best_len = len;
                best = m;
                if best_len >= nice_len {
                    return (best_len, (cur - best) as u32);
                }
            }
        }
        if B == 2 && in_window(second) {
            if let Some((c, m)) = search.candidate(second) {
                if may_beat(c, best_len) {
                    let len = extend(here, c, 4);
                    if len > best_len {
                        best_len = len;
                        best = m;
                    }
                }
            }
        }
        if best == cur {
            return (0, 0);
        }
        (best_len, (cur - best) as u32)
    }

    /// ht: inserts the `count` positions from `cur` without searching, unless
    /// they're too near `buf`'s end.
    #[inline(always)]
    pub(super) fn ht_skip<const B: usize>(
        &mut self,
        tables: &mut LzTables<'_>,
        buf: &[u8],
        cur: usize,
        count: u32,
    ) {
        let count = count as usize;
        if count + SEARCH_NBYTES as usize > buf.len() - cur {
            return;
        }
        let mask = MASK4 & !(B - 1);
        let mut b = self.next_hashes[0] & mask;
        let mut rel = self.relative_span(tables, cur, count);
        for next in buf[cur + 1..cur + count + 4].windows(4) {
            if B == 2 {
                tables.hash4[(b + 1) & MASK4] = tables.hash4[b];
            }
            tables.hash4[b] = rel as u16;
            let seq = u32::from_le_bytes(next.try_into().unwrap());
            b = (hash(seq, self.shift4) * B) & mask;
            rel = rel.wrapping_add(1);
        }
        self.next_hashes[0] = b;
        self.next_heads = ht_bucket::<B>(tables, b);
    }
}

/// ht: the bucket at `b`, loaded early for the next search.
#[inline(always)]
fn ht_bucket<const B: usize>(tables: &LzTables<'_>, b: usize) -> [u16; 2] {
    if B == 1 {
        [tables.hash4[b], EMPTY_BITS]
    } else {
        [tables.hash4[b], tables.hash4[(b + 1) & MASK4]]
    }
}

#[cfg(all(test, feature = "alloc"))]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::*;

    #[test]
    fn extend_counts_matching_bytes() {
        let buf: Vec<u8> = (0..100).map(|i| (i % 7) as u8).collect();
        let ext = |buf: &[u8], start, max| extend(&buf[7..7 + max], &buf[..max], start);
        assert_eq!(ext(&buf, 0, 93), 93);
        assert_eq!(ext(&buf, 4, 20), 20);
        let mut buf2 = buf.clone();
        buf2[7 + 13] = 99;
        assert_eq!(ext(&buf2, 0, 93), 13);
        buf2[7 + 3] = 99;
        assert_eq!(ext(&buf2, 0, 93), 3);
    }

    #[test]
    fn rebase_saturates() {
        let mut t = [EMPTY, -1, 0, 1, 32767, -32767].map(|x| x as u16);
        rebase(&mut t);
        assert_eq!(
            t.map(|x| x as i16),
            [EMPTY, EMPTY, EMPTY, -32767, -1, EMPTY]
        );
    }

    /// Every match the finders report is real and within the window, across
    /// several rebases.
    #[test]
    fn matches_are_real() {
        let mut data = Vec::new();
        let mut x = 1u32;
        while data.len() < 200_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            // Repeat recent chunks often, at varied distances.
            if x.is_multiple_of(3) && data.len() > 40_000 {
                let back = (x as usize >> 4) % 40_000 + 1;
                let start = data.len() - back;
                for i in 0..(x as usize >> 20) % 300 {
                    data.push(data[start + i]);
                }
            } else {
                data.push((x >> 24) as u8);
            }
        }
        for ht in [0, 1, 2] {
            let mut mem = vec![0; LZ_TABLES_LEN];
            let (mut t, _) = LzTables::carve(&mut mem);
            let mut mf = MatchFinder::new();
            mf.reset(&mut t, 0, ht, None);
            let mut cur = 0;
            let (mut found, mut far) = (0, 0);
            while cur + 300 < data.len() {
                let (len, off) = if ht == 1 {
                    mf.ht_longest::<1>(
                        &mut t,
                        &data,
                        cur,
                        Limits {
                            max: 258,
                            nice: 258,
                        },
                    )
                } else if ht == 2 {
                    mf.ht_longest::<2>(
                        &mut t,
                        &data,
                        cur,
                        Limits {
                            max: 258,
                            nice: 258,
                        },
                    )
                } else {
                    mf.hc_longest(
                        &mut t,
                        &data,
                        cur,
                        2,
                        Limits {
                            max: 258,
                            nice: 258,
                        },
                        50,
                    )
                };
                // A length of 0 or (hc) 2 is no match, whatever the offset.
                if len >= 3 {
                    let off = off as usize;
                    assert!((1..WINDOW).contains(&off), "offset {off}");
                    assert_eq!(
                        data[cur - off..][..len as usize],
                        data[cur..][..len as usize]
                    );
                    assert!(len >= 3);
                    found += 1;
                    far += usize::from(off > 16_000);
                }
                let step = (len as usize).max(1);
                if step > 1 {
                    if ht == 1 {
                        mf.ht_skip::<1>(&mut t, &data, cur + 1, step as u32 - 1);
                    } else if ht == 2 {
                        mf.ht_skip::<2>(&mut t, &data, cur + 1, step as u32 - 1);
                    } else {
                        mf.hc_skip(&mut t, &data, cur + 1, step as u32 - 1);
                    }
                }
                cur += step;
            }
            assert!(found > 1000 && far > 10, "{found} matches, {far} far");
        }
    }
}
