//! Writing blocks into the caller's output buffer: choosing each block's
//! type, building its header, and emitting it, in as many calls as the
//! output buffer takes. From libdeflate's `deflate_flush_block`.
//!
//! What is being written is an [`Emit`]: bits staged in a small buffer (a
//! block header, a stream header or trailer, a flush marker), a block's
//! literals and matches, or a stored block's bytes. Each can stop when the
//! output is full and carry on in the next call. While there's room for the
//! worst case, literals and matches are written with whole-word stores and
//! no other checks, as libdeflate does.

use super::block::{Block, Seq, Seqs};
use super::huffman::make_code;
use super::tables::{
    Codes, END_OF_BLOCK, FIRST_LEN_SYM, LENGTH_BASE, LENGTH_EXTRA, MAX_LITLEN_CODEWORD_LEN,
    MAX_MATCH, MAX_MATCH_BITS, MAX_OFFSET_CODEWORD_LEN, MAX_PRE_CODEWORD_LEN, NUM_LITLEN_SYMS,
    NUM_OFFSET_SYMS, NUM_PRECODE_SYMS, OFFSET_BASE, OFFSET_EXTRA, PRECODE_EXTRA,
    PRECODE_PERMUTATION, STATIC_CODES,
};

/// The bits not yet written out, lowest first.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Bits {
    buf: u64,
    /// At most 63.
    pub(super) count: u32,
}

impl Bits {
    pub(super) const EMPTY: Bits = Bits { buf: 0, count: 0 };

    #[inline(always)]
    fn add(&mut self, v: u64, n: u32) {
        debug_assert!(self.count + n <= 63 && v >> n == 0);
        self.buf |= v << self.count;
        self.count += n;
    }

    /// Writes the whole bytes out with one 8-byte store, which needs room.
    #[inline(always)]
    fn flush_word(&mut self, out: &mut [u8], op: &mut usize) {
        *out[*op..].first_chunk_mut().unwrap() = self.buf.to_le_bytes();
        let bytes = self.count >> 3;
        *op += bytes as usize;
        self.buf >>= bytes * 8;
        self.count &= 7;
    }

    /// Writes out whole bytes while there is room.
    pub(super) fn flush_bytes(&mut self, out: &mut [u8], op: &mut usize) {
        while self.count >= 8 && *op < out.len() {
            out[*op] = self.buf as u8;
            *op += 1;
            self.buf >>= 8;
            self.count -= 8;
        }
    }

    /// Adds `n` bits if they fit once what fits is written out.
    #[inline]
    fn push(&mut self, out: &mut [u8], op: &mut usize, v: u64, n: u32) -> bool {
        self.flush_bytes(out, op);
        if self.count + n > 63 {
            return false;
        }
        self.add(v, n);
        true
    }
}

/// Room [`Emit::Body`]'s fast path keeps: an 8-byte store, up to 7 bytes
/// already in the bit buffer, and a match.
const SLACK: usize = 16;
const _: () = assert!(SLACK >= 8 + (7 + MAX_MATCH_BITS as usize).div_ceil(8));

/// The staging buffer's size: the longest dynamic block header is 17 bits,
/// 19 3-bit precode lengths, and at most 7 bits per codeword length (a
/// run item takes at most 14 bits for 3 or more).
const STAGE_LEN: usize = (17 + 19 * 3 + 7 * (NUM_LITLEN_SYMS + NUM_OFFSET_SYMS)).div_ceil(8);

/// What is being written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Emit {
    Idle,
    /// The staged bits, then the block's body if `body_next`.
    Staged {
        body_next: bool,
    },
    /// A Huffman block's literals and matches.
    Body(Cursor),
    /// A stored block, `at` bytes in, with `sub_left` bytes left of the
    /// current (at most 65535-byte) piece, whose header is written.
    Stored {
        at: u32,
        sub_left: u32,
    },
}

pub(super) struct Writer {
    pub(super) bits: Bits,
    pub(super) emit: Emit,
    /// The block being written: its length and whether it is the last.
    block_len: u32,
    /// The Huffman block's cost in bits, header included.
    cost: u32,
    last: bool,
    stage: [u8; STAGE_LEN + 8],
    /// The staged bits, and how many are written out.
    stage_len: u32,
    stage_at: u32,
    /// The block's dynamic codes, if it uses them rather than the static.
    codes: Codes,
    use_static: bool,
    /// The block's codes packed for writing, each a codeword (and extra
    /// bits) in the low 24 bits and its bit count in the top 8.
    packed: Packed,
}

/// A dynamic block's precode: how the codeword lengths are written.
struct Precode {
    freqs: [u32; NUM_PRECODE_SYMS],
    lens: [u8; NUM_PRECODE_SYMS],
    codewords: [u32; NUM_PRECODE_SYMS],
    /// Each item is a precode symbol and, from bit 5, its extra bits.
    items: [u16; NUM_LITLEN_SYMS + NUM_OFFSET_SYMS],
    num_items: usize,
    num_litlen: usize,
    num_offset: usize,
    num_explicit_lens: usize,
}

impl Writer {
    pub(super) const fn new() -> Self {
        Writer {
            bits: Bits::EMPTY,
            emit: Emit::Idle,
            block_len: 0,
            cost: 0,
            last: false,
            stage: [0; STAGE_LEN + 8],
            stage_len: 0,
            stage_at: 0,
            codes: Codes::EMPTY,
            use_static: false,
            packed: Packed {
                litlen: [0; NUM_LITLEN_SYMS],
                lengths: [0; LENGTHS_LEN],
                offsets: [0; NUM_OFFSET_SYMS],
            },
        }
    }

    pub(super) fn reset(&mut self) {
        self.bits = Bits::EMPTY;
        self.emit = Emit::Idle;
    }

    /// Whether nothing is being written (but perhaps the bit buffer).
    pub(super) fn is_idle(&self) -> bool {
        self.emit == Emit::Idle
    }

    /// Starts staging bits.
    fn stage_start(&mut self) {
        debug_assert_eq!(self.emit, Emit::Idle);
        self.stage.fill(0);
        self.stage_len = 0;
        self.stage_at = 0;
    }

    fn stage_put(&mut self, v: u32, n: u32) {
        debug_assert!(n <= 32 && u64::from(v) >> n == 0);
        let i = (self.stage_len / 8) as usize;
        let w: &mut [u8; 8] = self.stage[i..].first_chunk_mut().unwrap();
        *w = (u64::from_le_bytes(*w) | u64::from(v) << (self.stage_len % 8)).to_le_bytes();
        self.stage_len += n;
    }

    /// Zero bits up to the next byte boundary of the stream.
    fn stage_align(&mut self) {
        let pad = (8 - (self.bits.count + self.stage_len) % 8) % 8;
        self.stage_put(0, pad);
    }

    /// Stages `bytes` at the next byte boundary.
    pub(super) fn stage_bytes(&mut self, bytes: &[u8]) {
        self.stage_start();
        self.stage_align();
        for &b in bytes {
            self.stage_put(u32::from(b), 8);
        }
        self.emit = Emit::Staged { body_next: false };
    }

    /// Stages an empty stored block, which ends on a byte boundary with
    /// `00 00 FF FF`: a sync flush's marker, or an empty last block.
    pub(super) fn stage_marker(&mut self, last: bool) {
        self.stage_start();
        self.stage_put(u32::from(last), 3);
        self.stage_align();
        self.stage_put(0xFFFF_0000, 32);
        self.emit = Emit::Staged { body_next: false };
    }

    /// Starts writing a stored block of the `len` bytes from `block.start`.
    pub(super) fn begin_stored(&mut self, len: usize, last: bool) {
        debug_assert!(len > 0);
        self.block_len = len as u32;
        self.last = last;
        self.emit = Emit::Stored { at: 0, sub_left: 0 };
    }

    /// Ends `block`, `len` bytes long: builds its codes, picks the cheapest
    /// block type, and starts writing it.
    pub(super) fn begin_block(
        &mut self,
        block: &mut Block,
        seqs: &mut Seqs,
        len: usize,
        last: bool,
    ) {
        debug_assert_eq!(self.emit, Emit::Idle);
        self.block_len = len as u32;
        self.last = last;
        block.close(seqs);
        block.litlen_freqs[END_OF_BLOCK] += 1;
        let c = &mut self.codes;
        make_code(
            &block.litlen_freqs,
            MAX_LITLEN_CODEWORD_LEN,
            &mut c.litlen_lens,
            &mut c.litlen,
        );
        make_code(
            &block.offset_freqs,
            MAX_OFFSET_CODEWORD_LEN,
            &mut c.offset_lens,
            &mut c.offset,
        );
        let pre = precode(c);

        // Each type's cost in bits, as libdeflate counts them.
        let mut dynamic = 3 + 5 + 5 + 4 + 3 * pre.num_explicit_lens as u32;
        for ((&f, &extra), &len) in pre.freqs.iter().zip(&PRECODE_EXTRA).zip(&pre.lens) {
            dynamic += f * (u32::from(extra) + u32::from(len));
        }
        let mut fixed = 3;
        for sym in 0..FIRST_LEN_SYM {
            let f = block.litlen_freqs[sym];
            dynamic += f * u32::from(c.litlen_lens[sym]);
            fixed += f * u32::from(STATIC_CODES.litlen_lens[sym]);
        }
        for (slot, &extra) in LENGTH_EXTRA.iter().enumerate() {
            let sym = FIRST_LEN_SYM + slot;
            let f = block.litlen_freqs[sym];
            dynamic += f * (u32::from(extra) + u32::from(c.litlen_lens[sym]));
            fixed += f * (u32::from(extra) + u32::from(STATIC_CODES.litlen_lens[sym]));
        }
        for (slot, &extra) in OFFSET_EXTRA.iter().enumerate() {
            let f = block.offset_freqs[slot];
            dynamic += f * (u32::from(extra) + u32::from(c.offset_lens[slot]));
            fixed += f * (u32::from(extra) + 5);
        }
        let stored = stored_cost(self.bits.count, len);

        // Ties go to stored, then static.
        if stored <= dynamic.min(fixed) {
            self.begin_stored(len, last);
            return;
        }
        self.use_static = fixed <= dynamic;
        self.cost = fixed.min(dynamic);
        self.stage_start();
        self.stage_put(u32::from(last), 1);
        if self.use_static {
            self.stage_put(1, 2);
        } else {
            self.stage_put(2, 2);
            self.stage_dynamic_header(&pre);
        }
        self.pack(&block.litlen_freqs);
        self.emit = Emit::Staged { body_next: true };
    }

    fn stage_dynamic_header(&mut self, pre: &Precode) {
        self.stage_put(pre.num_litlen as u32 - 257, 5);
        self.stage_put(pre.num_offset as u32 - 1, 5);
        self.stage_put(pre.num_explicit_lens as u32 - 4, 4);
        for &sym in &PRECODE_PERMUTATION[..pre.num_explicit_lens] {
            self.stage_put(u32::from(pre.lens[sym as usize]), 3);
        }
        for &item in &pre.items[..pre.num_items] {
            let sym = (item & 0x1F) as usize;
            self.stage_put(pre.codewords[sym], u32::from(pre.lens[sym]));
            self.stage_put(u32::from(item >> 5), u32::from(PRECODE_EXTRA[sym]));
        }
    }

    /// Packs the block's codes for writing, given its litlen frequencies.
    fn pack(&mut self, freqs: &[u32; NUM_LITLEN_SYMS]) {
        let c = if self.use_static {
            &STATIC_CODES
        } else {
            &self.codes
        };
        let p = &mut self.packed;
        for ((e, &cw), &len) in p.litlen.iter_mut().zip(&c.litlen).zip(&c.litlen_lens) {
            *e = cw | u32::from(len) << 24;
        }
        // Lengths: the codeword and extra bits together, for the slots the
        // block uses (small blocks use few).
        for slot in 0..LENGTH_BASE.len() {
            let sym = FIRST_LEN_SYM + slot;
            if freqs[sym] == 0 {
                continue;
            }
            let base = LENGTH_BASE[slot];
            let end = LENGTH_BASE.get(slot + 1).map_or(MAX_MATCH + 1, |&b| b);
            let n = u32::from(c.litlen_lens[sym] + LENGTH_EXTRA[slot]) << 24;
            let cw = c.litlen[sym];
            for (extra, e) in p.lengths[base as usize..end as usize]
                .iter_mut()
                .enumerate()
            {
                *e = (cw | (extra as u32) << c.litlen_lens[sym]) | n;
            }
        }
        // Offsets: the codeword, its length, and the extra bits' count.
        let offsets = c.offset.iter().zip(&c.offset_lens).zip(&OFFSET_EXTRA);
        for (e, ((&cw, &len), &extra)) in p.offsets.iter_mut().zip(offsets) {
            *e = cw | u32::from(len) << 16 | u32::from(extra) << 24;
        }
    }

    /// Writes what can be written into `out[*op..]`. Returns whether it all
    /// was, but for up to 7 bits left in the bit buffer. `block` and `buf`
    /// must be as when the block began, but for the block's position in
    /// `buf`.
    pub(super) fn drain(
        &mut self,
        block: &Block,
        seqs: &Seqs,
        buf: &[u8],
        out: &mut [u8],
        op: &mut usize,
    ) -> bool {
        loop {
            let ok = match self.emit {
                Emit::Idle => {
                    self.bits.flush_bytes(out, op);
                    return self.bits.count < 8;
                }
                Emit::Staged { body_next } => {
                    let ok = self.drain_stage(out, op);
                    if ok && body_next {
                        self.emit = Emit::Body(Cursor::START);
                        continue;
                    }
                    ok
                }
                Emit::Body(_) => {
                    let data = &buf[block.start..block.start + self.block_len as usize];
                    self.drain_body(block.seqs(seqs), data, out, op)
                }
                Emit::Stored { .. } => {
                    let data = &buf[block.start..block.start + self.block_len as usize];
                    self.drain_stored(data, out, op)
                }
            };
            if !ok {
                return false;
            }
            self.emit = Emit::Idle;
        }
    }

    fn drain_stage(&mut self, out: &mut [u8], op: &mut usize) -> bool {
        while self.stage_at < self.stage_len {
            let n = (self.stage_len - self.stage_at).min(32);
            let i = (self.stage_at / 8) as usize;
            let w = u64::from_le_bytes(*self.stage[i..].first_chunk().unwrap());
            let v = (w >> (self.stage_at % 8)) & ((1 << n) - 1);
            if !self.bits.push(out, op, v, n) {
                return false;
            }
            self.stage_at += n;
        }
        true
    }

    fn drain_stored(&mut self, data: &[u8], out: &mut [u8], op: &mut usize) -> bool {
        let Emit::Stored {
            mut at,
            mut sub_left,
        } = self.emit
        else {
            unreachable!()
        };
        let done = loop {
            if sub_left == 0 {
                let left = data.len() as u32 - at;
                if left == 0 {
                    break true;
                }
                // BFINAL, BTYPE 00, padding, LEN and NLEN.
                let n = left.min(0xFFFF);
                let last = self.last && n == left;
                let pad = (8 - (self.bits.count + 3) % 8) % 8;
                let v = u64::from(last) | u64::from(n | (!n & 0xFFFF) << 16) << (3 + pad);
                if !self.bits.push(out, op, v, 35 + pad) {
                    break false;
                }
                sub_left = n;
            }
            self.bits.flush_bytes(out, op);
            if self.bits.count > 0 {
                break false;
            }
            let n = (sub_left as usize).min(out.len() - *op);
            out[*op..*op + n].copy_from_slice(&data[at as usize..at as usize + n]);
            *op += n;
            at += n as u32;
            sub_left -= n as u32;
            if sub_left > 0 {
                break false;
            }
        };
        self.emit = Emit::Stored { at, sub_left };
        done
    }

    fn drain_body(
        &mut self,
        seqs: &[[u16; 4]],
        data: &[u8],
        out: &mut [u8],
        op: &mut usize,
    ) -> bool {
        let Emit::Body(mut cur) = self.emit else {
            unreachable!()
        };
        let body = Body {
            packed: &self.packed,
            seqs,
            data,
        };
        let mut bits = self.bits;
        // With room for the whole body, as libdeflate checks for, it is
        // written with no further checks of the room.
        let whole =
            cur == Cursor::START && out.len() - *op >= self.cost.div_ceil(8) as usize + SLACK;
        let done = (whole && body.whole(&mut bits, out, op))
            || body.fast(&mut bits, &mut cur, out, op)
            || body.slow(&mut bits, &mut cur, out, op);
        self.bits = bits;
        self.emit = Emit::Body(cur);
        done
    }
}

/// Where a Huffman block's body is: at sequence `seq`, with `lits` of its
/// literals written, `at` bytes into the block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Cursor {
    seq: u32,
    lits: u32,
    at: u32,
}

impl Cursor {
    const START: Cursor = Cursor {
        seq: 0,
        lits: 0,
        at: 0,
    };
}

/// What writing a Huffman block's body needs.
struct Body<'a> {
    packed: &'a Packed,
    seqs: &'a [[u16; 4]],
    /// The block's input.
    data: &'a [u8],
}

const LENGTHS_LEN: usize = MAX_MATCH as usize + 1;

/// A block's codes packed for writing.
#[derive(Clone, Debug)]
pub(super) struct Packed {
    /// Codeword | bits << 24.
    litlen: [u32; NUM_LITLEN_SYMS],
    /// A length's codeword and extra bits | bits << 24.
    lengths: [u32; LENGTHS_LEN],
    /// Codeword | codeword bits << 16 | extra bits << 24.
    offsets: [u32; NUM_OFFSET_SYMS],
}

/// Each offset slot's base, padded to 32 slots so that a masked index
/// needs no bounds check.
const OFFSET_BASE_32: [u32; NUM_OFFSET_SYMS] = {
    let mut t = [0; NUM_OFFSET_SYMS];
    let mut i = 0;
    while i < OFFSET_BASE.len() {
        t[i] = OFFSET_BASE[i];
        i += 1;
    }
    t
};

impl Body<'_> {
    #[inline(always)]
    fn literal(&self, b: u8) -> (u64, u32) {
        let e = self.packed.litlen[b as usize];
        (u64::from(e & 0xFF_FFFF), e >> 24)
    }

    #[inline(always)]
    fn end_of_block(&self) -> (u64, u32) {
        let e = self.packed.litlen[END_OF_BLOCK];
        (u64::from(e & 0xFF_FFFF), e >> 24)
    }

    /// A match's codewords and extra bits, combined.
    #[inline(always)]
    fn matched(&self, s: Seq) -> (u64, u32) {
        // Clamped rather than checked: lengths are at most 258.
        let l = self.packed.lengths[(s.len() as usize).min(MAX_MATCH as usize)];
        let slot = usize::from(s.slot) & (NUM_OFFSET_SYMS - 1);
        let o = self.packed.offsets[slot];
        let (l1, l2) = (l >> 24, (o >> 16) & 0xFF);
        let extra = u64::from(u32::from(s.offset).wrapping_sub(OFFSET_BASE_32[slot]));
        let v = u64::from(l & 0xFF_FFFF) | u64::from(o & 0xFFFF) << l1 | extra << (l1 + l2);
        (v, l1 + l2 + (o >> 24))
    }

    /// Writes the whole body into room for it (its cost and [`SLACK`]).
    /// Returns `true`, for being done.
    #[inline(always)]
    fn whole(&self, bits: &mut Bits, out: &mut [u8], op: &mut usize) -> bool {
        let mut pos = *op;
        let mut at = 0;
        bits.flush_word(out, &mut pos);
        for &w in self.seqs {
            let s = Seq::load(w);
            let lits = s.litrun() as usize;
            self.run(bits, out, &mut pos, at, lits);
            at += lits;
            let (v, n) = if s.len() == 0 {
                self.end_of_block()
            } else {
                self.matched(s)
            };
            bits.add(v, n);
            bits.flush_word(out, &mut pos);
            at += s.len() as usize;
        }
        *op = pos;
        true
    }

    /// Writes while there's room for the worst case: whole groups of
    /// literals, flushed four at a time, and whole matches. Returns whether
    /// the body is done.
    #[inline(always)]
    fn fast(&self, bits: &mut Bits, cur: &mut Cursor, out: &mut [u8], op: &mut usize) -> bool {
        let Cursor {
            mut seq,
            mut lits,
            mut at,
        } = *cur;
        // The output position, in a register.
        let mut pos = *op;
        let done = 'fast: {
            if out.len() - pos < SLACK {
                break 'fast false;
            }
            bits.flush_word(out, &mut pos);
            loop {
                let s = Seq::load(self.seqs[seq as usize]);
                let left = s.litrun() - lits;
                // Room for the worst case of the rest of the sequence: its
                // literals at 14 bits, and the match or end of block.
                let need = (left as usize * MAX_LITLEN_CODEWORD_LEN as usize).div_ceil(8) + SLACK;
                if out.len() - pos < need {
                    // As many literals as fit, then check again.
                    let room = out.len() - pos;
                    let fit =
                        (room.saturating_sub(SLACK) * 8 / MAX_LITLEN_CODEWORD_LEN as usize) as u32;
                    let n = left.min(fit);
                    if n == 0 {
                        break 'fast false;
                    }
                    self.literals(
                        bits,
                        out,
                        &mut pos,
                        &self.data[at as usize..(at + n) as usize],
                    );
                    lits += n;
                    at += n;
                    continue;
                }
                self.run(bits, out, &mut pos, at as usize, left as usize);
                at += left;
                let (v, n) = if s.len() == 0 {
                    self.end_of_block()
                } else {
                    self.matched(s)
                };
                bits.add(v, n);
                bits.flush_word(out, &mut pos);
                if s.len() == 0 {
                    break 'fast true;
                }
                at += s.len();
                seq += 1;
                lits = 0;
            }
        };
        *op = pos;
        *cur = Cursor { seq, lits, at };
        done
    }

    /// Writes `lits`, flushing four at a time, into room for them.
    #[inline(always)]
    fn literals(&self, bits: &mut Bits, out: &mut [u8], pos: &mut usize, lits: &[u8]) {
        let (quads, rest) = lits.as_chunks::<4>();
        for q in quads {
            for &b in q {
                let (v, n) = self.literal(b);
                bits.add(v, n);
            }
            bits.flush_word(out, pos);
        }
        if !rest.is_empty() {
            for &b in rest {
                let (v, n) = self.literal(b);
                bits.add(v, n);
            }
            bits.flush_word(out, pos);
        }
    }

    /// Writes the `n` literals at `at` like [`literals`](Self::literals),
    /// but the last 0 to 3 (most runs are that short) without branching on
    /// how many: three are always looked up, and those past the run add no
    /// bits.
    #[inline(always)]
    fn run(&self, bits: &mut Bits, out: &mut [u8], pos: &mut usize, at: usize, n: usize) {
        let (quads, k) = (n & !3, n & 3);
        let start = at + quads;
        let Some(&tail) = self.data.get(start..).and_then(<[u8]>::first_chunk::<3>) else {
            // The block's last few bytes.
            self.literals(bits, out, pos, &self.data[at..at + n]);
            return;
        };
        self.literals(bits, out, pos, &self.data[at..start]);
        for (i, b) in tail.into_iter().enumerate() {
            let (v, len) = self.literal(b);
            let keep = u32::from(i < k).wrapping_neg();
            bits.add(v & u64::from(keep), len & keep);
        }
        bits.flush_word(out, pos);
    }

    /// Writes an item at a time, near the end of `out`. Returns whether the
    /// body is done (else `out` is full).
    fn slow(&self, bits: &mut Bits, cur: &mut Cursor, out: &mut [u8], op: &mut usize) -> bool {
        loop {
            let s = Seq::load(self.seqs[cur.seq as usize]);
            let (v, n) = if cur.lits < s.litrun() {
                self.literal(self.data[cur.at as usize])
            } else if s.len() == 0 {
                self.end_of_block()
            } else {
                self.matched(s)
            };
            if !bits.push(out, op, v, n) {
                return false;
            }
            if cur.lits < s.litrun() {
                cur.lits += 1;
                cur.at += 1;
            } else if s.len() == 0 {
                return true;
            } else {
                *cur = Cursor {
                    seq: cur.seq + 1,
                    lits: 0,
                    at: cur.at + s.len(),
                };
            }
        }
    }
}

/// The bits a stored block of `len` bytes takes after `count` bits: a
/// 3-bit header, padding to a byte, and LEN and NLEN for each 65535 bytes.
fn stored_cost(count: u32, len: usize) -> u32 {
    let pieces = len.div_ceil(0xFFFF).max(1) as u32;
    3 + (8 - (count + 3) % 8) % 8 + 32 + 40 * (pieces - 1) + 8 * len as u32
}

/// Builds the precode for `c`'s codeword lengths (libdeflate's
/// `deflate_precompute_huffman_header`).
fn precode(c: &Codes) -> Precode {
    let num_litlen = 257
        + c.litlen_lens[257..]
            .iter()
            .rposition(|&l| l != 0)
            .map_or(0, |i| i + 1);
    let num_offset = 1 + c.offset_lens[1..]
        .iter()
        .rposition(|&l| l != 0)
        .map_or(0, |i| i + 1);
    let mut lens = [0u8; NUM_LITLEN_SYMS + NUM_OFFSET_SYMS];
    lens[..num_litlen].copy_from_slice(&c.litlen_lens[..num_litlen]);
    lens[num_litlen..num_litlen + num_offset].copy_from_slice(&c.offset_lens[..num_offset]);
    let lens = &lens[..num_litlen + num_offset];

    let mut pre = Precode {
        freqs: [0; NUM_PRECODE_SYMS],
        lens: [0; NUM_PRECODE_SYMS],
        codewords: [0; NUM_PRECODE_SYMS],
        items: [0; NUM_LITLEN_SYMS + NUM_OFFSET_SYMS],
        num_items: 0,
        num_litlen,
        num_offset,
        num_explicit_lens: 0,
    };
    let item = |pre: &mut Precode, sym: usize, extra: usize| {
        pre.freqs[sym] += 1;
        pre.items[pre.num_items] = (sym | extra << 5) as u16;
        pre.num_items += 1;
    };
    let mut start = 0;
    while start < lens.len() {
        let len = lens[start];
        let end = start
            + lens[start..]
                .iter()
                .position(|&l| l != len)
                .unwrap_or(lens.len() - start);
        if len == 0 {
            // Symbol 18: 11 to 138 zeros. Symbol 17: 3 to 10.
            while end - start >= 11 {
                let extra = (end - start - 11).min(0x7F);
                item(&mut pre, 18, extra);
                start += 11 + extra;
            }
            if end - start >= 3 {
                let extra = (end - start - 3).min(0x7);
                item(&mut pre, 17, extra);
                start += 3 + extra;
            }
        } else if end - start >= 4 {
            // The length, then symbol 16: 3 to 6 repeats of it.
            item(&mut pre, len as usize, 0);
            start += 1;
            while end - start >= 3 {
                let extra = (end - start - 3).min(0x3);
                item(&mut pre, 16, extra);
                start += 3 + extra;
            }
        }
        while start < end {
            item(&mut pre, len as usize, 0);
            start += 1;
        }
    }
    make_code(
        &pre.freqs,
        MAX_PRE_CODEWORD_LEN,
        &mut pre.lens,
        &mut pre.codewords,
    );
    pre.num_explicit_lens = 4 + PRECODE_PERMUTATION[4..]
        .iter()
        .rposition(|&sym| pre.lens[sym as usize] != 0)
        .map_or(0, |i| i + 1);
    pre
}
