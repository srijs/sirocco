//! The DEFLATE decode loop.
//!
//! It follows the structure of libdeflate's decoder: a word-sized bit buffer
//! refilled branchlessly, a "fastloop" that runs while the input and output
//! are far enough from their ends that it can work in fixed-size windows of
//! them with few bounds checks, and a generic loop for the tail and the
//! block headers. See libdeflate's `deflate_decompress.c` for the full
//! derivation of the bit-budget constants below.

use super::tables::*;
use super::DecompressError;

type BitBuf = usize;

const WORDBYTES: usize = size_of::<BitBuf>();
const BITBUF_NBITS: u32 = BitBuf::BITS;

/// Most consumable bits. One less than the word size so that refills can set
/// `bitsleft |= MAX_BITSLEFT & !7` and a refill at `MAX_BITSLEFT` doesn't
/// shift by the full word width.
const MAX_BITSLEFT: u32 = BITBUF_NBITS - 1;

/// Fewest consumable bits right after a refill.
const CONSUMABLE_NBITS: u32 = MAX_BITSLEFT - 7;

/// Fewest bits that can be looked at (but not necessarily consumed) right
/// after a fastloop refill.
const FASTLOOP_PRELOADABLE_NBITS: u32 = BITBUF_NBITS;

/// Bits that are preloadable but not consumable after a fastloop refill.
const PRELOAD_SLACK: u32 = FASTLOOP_PRELOADABLE_NBITS - MAX_BITSLEFT;

const fn can_consume(n: u32) -> bool {
    CONSUMABLE_NBITS >= n
}

const fn can_consume_and_then_preload(consume: u32, preload: u32) -> bool {
    CONSUMABLE_NBITS >= consume && FASTLOOP_PRELOADABLE_NBITS >= consume + preload
}

/// Worst-case output bytes written per fastloop iteration: two literals, a
/// max-length match, and the word-copy overrun. (Three literals and no match
/// write three bytes.)
const FASTLOOP_MAX_BYTES_WRITTEN: usize = 2 + MAX_MATCH_LEN + (5 * WORDBYTES) - 1;

/// Output bytes that the tail fastloop needs ahead of it to start an
/// iteration: room for the three literals it may write. It checks that each
/// match fits as it comes up.
const TAIL_MIN_OUT_ROOM: usize = 3;

/// Worst-case input bytes read per fastloop iteration, including the word
/// that a branchless refill reads past `in_next`.
const FASTLOOP_MAX_BYTES_READ: usize =
    (MAX_BITSLEFT + (2 * LITLEN_TABLEBITS) + LENGTH_MAXBITS + OFFSET_MAXBITS).div_ceil(8) as usize
        + WORDBYTES;

/// Whether the fastloop decodes up to two extra literals per iteration.
/// True on 64-bit targets.
const EXTRA_LITERALS: bool =
    can_consume_and_then_preload(2 * LITLEN_TABLEBITS + LENGTH_MAXBITS, OFFSET_TABLEBITS)
        && can_consume_and_then_preload(
            2 * LITLEN_TABLEBITS + MAX_LITLEN_CODEWORD_LEN,
            LITLEN_TABLEBITS,
        );

/// The longest offset that can need a refill before the next litlen preload.
const OFFSET_MAX_REFILL_BITS: u32 = if OFFSET_MAXBITS - OFFSET_TABLEBITS > OFFSET_MAXFASTBITS {
    OFFSET_MAXBITS - OFFSET_TABLEBITS
} else {
    OFFSET_MAXFASTBITS
};

const DEFLATE_BLOCKTYPE_UNCOMPRESSED: BitBuf = 0;
const DEFLATE_BLOCKTYPE_STATIC_HUFFMAN: BitBuf = 1;
const DEFLATE_BLOCKTYPE_DYNAMIC_HUFFMAN: BitBuf = 2;

/// The order in which precode lengths are stored.
const PRECODE_LENS_PERMUTATION: [u8; NUM_PRECODE_SYMS] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

#[inline(always)]
const fn bitmask(n: u32) -> BitBuf {
    (1 << n) - 1
}

/// The value a length or offset entry decodes to: its base plus the extra
/// bits that follow the codeword in `saved`, the bit buffer from before the
/// entry was consumed. With BMI2 enabled the masking compiles to `bzhi`.
#[inline(always)]
fn entry_value(entry: u32, saved: BitBuf) -> usize {
    (entry >> 16) as usize + ((saved & bitmask(entry as u8 as u32)) >> (entry >> 8) as u8)
}

/// The bit buffer: bits read from the input (or implicit zero bytes past its
/// end), lowest bit first.
#[derive(Clone, Copy, Debug)]
struct Bits {
    buf: BitBuf,
    /// The consumable bits in `buf`. Only its low byte is meaningful; the
    /// rest may hold garbage, which lets the decoder subtract whole table
    /// entries from it.
    left: u32,
}

impl Bits {
    /// The number of consumable bits.
    #[inline(always)]
    fn avail(self) -> u32 {
        self.left as u8 as u32
    }

    /// The next `n` bits, without consuming them.
    #[inline(always)]
    fn peek(self, n: u32) -> usize {
        self.buf & bitmask(n)
    }

    #[inline(always)]
    fn consume(&mut self, n: u32) {
        self.buf >>= n;
        self.left = self.left.wrapping_sub(n);
    }

    /// Consumes the bits for a table entry (its low byte).
    #[inline(always)]
    fn consume_entry(&mut self, entry: u32) {
        self.buf >>= entry as u8;
        self.left = self.left.wrapping_sub(entry);
    }

    /// The index of the subtable entry that the subtable pointer `entry`
    /// leads to.
    #[inline(always)]
    fn subtable_index(self, entry: u32) -> usize {
        (entry >> 16) as usize + self.peek((entry >> 8) & 0x3F)
    }

    /// Refills from `word`, the next `WORDBYTES` of input, counting whole
    /// bytes only. That consumes `WORDBYTES - 1` bytes of input, less the
    /// returned number of whole bytes that the buffer already held.
    #[inline(always)]
    fn refill(&mut self, word: BitBuf) -> usize {
        self.buf |= word << (self.left as u8);
        let held = ((self.left >> 3) & 7) as usize;
        self.left |= MAX_BITSLEFT & !7;
        held
    }
}

/// Where the decoder is in the input and output, and its bit buffer.
#[derive(Clone, Copy, Debug)]
struct Cursor {
    in_pos: usize,
    out_pos: usize,
    bits: Bits,
    /// Implicit zero bytes refilled past the end of the input.
    overread: usize,
}

impl Cursor {
    const START: Cursor = Cursor {
        in_pos: 0,
        out_pos: 0,
        bits: Bits { buf: 0, left: 0 },
        overread: 0,
    };

    /// The whole bytes in the bit buffer.
    fn whole_bytes(self) -> usize {
        (self.bits.avail() >> 3) as usize
    }

    /// Whether bits from implicit zero bytes have been consumed.
    fn consumed_overread(self) -> bool {
        self.overread > self.whole_bytes()
    }

    /// Gives back the whole bytes in the bit buffer, so that `in_pos` is the
    /// next byte boundary, and empties it.
    fn align_to_byte(&mut self) {
        self.in_pos -= self.whole_bytes() - self.overread;
        self.overread = 0;
        self.bits = Cursor::START.bits;
    }
}

/// Where a decode that paused stopped, so that it can carry on later. It
/// points just before the symbol (or block header) that didn't fit in the
/// output, or that needed more input than there was, so no output from it was
/// written.
///
/// The bit buffer normally holds fewer than 8 bits, from the byte just before
/// `in_pos`, so all input before `in_pos` is consumed and decoding can carry
/// on from a different buffer that continues the input at `in_pos`.
#[derive(Clone, Debug)]
#[cfg_attr(not(feature = "alloc"), allow(dead_code))]
pub(super) struct Resume {
    /// Holds implicit zero bytes only when some were already consumed, which
    /// only a decode without more input to come carries on from (to the
    /// same error a single call would give).
    at: Cursor,
    /// `Some(is_final_block)` when stopped inside a block, `None` when
    /// stopped at the start of one.
    in_block: Option<bool>,
    /// Inside a stored block, the bytes of it still to copy (at a byte
    /// boundary, with no bits held); 0 inside a Huffman block, whose decode
    /// tables are still in the `Decompressor`.
    stored_left: u16,
    /// Whether it stopped for more input rather than more output space.
    need_input: bool,
    /// Whether carrying on should stop again right after the header of the
    /// next stored block, for the caller to copy its data. Set by the
    /// caller; a pause clears it.
    stop_at_stored: bool,
}

impl Resume {
    /// A resume point at `at`. Unless implicit zero bytes were consumed
    /// already (bad data, which more input never resumes from), it gives
    /// back the whole bytes in the bit buffer, so that it holds at most 7
    /// bits.
    fn new(mut at: Cursor, in_block: Option<bool>, need_input: bool) -> Self {
        if !at.consumed_overread() {
            let partial = at.bits.avail() & 7;
            let buf = at.bits.peek(partial);
            at.align_to_byte();
            at.bits = Bits { buf, left: partial };
        }
        Resume {
            at,
            in_block,
            stored_left: 0,
            need_input,
            stop_at_stored: false,
        }
    }
}

#[cfg(feature = "alloc")]
impl Resume {
    /// A decode that hasn't started: at a block boundary, with no bits held.
    pub(super) fn start() -> Self {
        Resume {
            at: Cursor::START,
            in_block: None,
            stored_left: 0,
            need_input: false,
            stop_at_stored: false,
        }
    }

    /// Input bytes consumed so far.
    pub(super) fn in_pos(&self) -> usize {
        self.at.in_pos
    }

    /// Moves the input position, for carrying on in a buffer that holds the
    /// same input at a different offset.
    pub(super) fn set_in_pos(&mut self, pos: usize) {
        self.at.in_pos = pos;
    }

    /// Output bytes before the stopping point.
    pub(super) fn out_pos(&self) -> usize {
        self.at.out_pos
    }

    /// Moves the output position, for carrying on in a buffer that holds the
    /// same recent output (at least the last 32 KiB) at a different offset.
    pub(super) fn set_out_pos(&mut self, pos: usize) {
        self.at.out_pos = pos;
    }

    /// Whether it stopped because the input ran out.
    pub(super) fn needs_input(&self) -> bool {
        self.need_input
    }

    /// The bytes still to copy when stopped inside a stored block, whose
    /// data starts at the input position.
    pub(super) fn stored_left(&self) -> Option<usize> {
        (self.stored_left > 0).then_some(self.stored_left.into())
    }

    /// Moves past `n` bytes of the stored block it stopped in, which the
    /// caller copied. Returns whether that finished the stream.
    pub(super) fn skip_stored(&mut self, n: usize) -> bool {
        let left = usize::from(self.stored_left);
        assert!(n <= left, "past the stored block");
        self.at.in_pos += n;
        self.at.out_pos += n;
        self.need_input = false;
        self.stored_left = (left - n) as u16;
        if self.stored_left > 0 {
            return false;
        }
        self.in_block.take() == Some(true)
    }

    /// Makes carrying on stop right after the next stored block header,
    /// with [`stored_left`](Self::stored_left) set, instead of copying it.
    pub(super) fn set_stop_at_stored(&mut self, stop: bool) {
        self.stop_at_stored = stop;
    }
}

/// How a decode call ended, short of an error.
#[derive(Debug)]
// Without `alloc` there is no growing output to resume into.
#[cfg_attr(not(feature = "alloc"), allow(dead_code))]
pub(super) enum Status {
    /// The stream ended: input bytes consumed and output bytes written.
    Done(usize, usize),
    /// The output buffer filled up, or (with `more_input`) the input ran
    /// out; [`Resume::needs_input`] says which.
    Full(Resume),
}

/// Why [`fastloop`] returned.
enum FastExit {
    /// The input or output is too near its end to continue safely.
    Margin,
    /// Decoded the end-of-block symbol.
    EndOfBlock,
    /// A match offset reaches back before the start of the output.
    BadOffset,
}

/// Input bytes that the fastloop needs ahead of it to start an iteration:
/// at least what an iteration reads, and enough that the compiler can see
/// that four refills (each reads a word and advances by less than one, and
/// 64-bit iterations do no more) need no bounds checks.
const FASTLOOP_IN_WINDOW: usize = if FASTLOOP_MAX_BYTES_READ > 4 * (WORDBYTES - 1) + WORDBYTES {
    FASTLOOP_MAX_BYTES_READ
} else {
    4 * (WORDBYTES - 1) + WORDBYTES
};

/// The decoder state that [`fastloop`] runs on: a position in an input and
/// an output buffer.
struct FastState<'a> {
    input: &'a [u8],
    output: &'a mut [u8],
    /// The output size: `output.len()`, or without writes (where `output`
    /// is empty) the size limit.
    out_len: usize,
    in_pos: usize,
    out_pos: usize,
    bits: Bits,
}

impl<'a> FastState<'a> {
    #[inline(always)]
    fn new(input: &'a [u8], output: &'a mut [u8], out_len: usize, c: &Cursor) -> Self {
        FastState {
            input,
            output,
            out_len,
            in_pos: c.in_pos,
            out_pos: c.out_pos,
            bits: c.bits,
        }
    }

    /// Moves `c` to where the fastloop stopped.
    #[inline(always)]
    fn finish(self, c: &mut Cursor) {
        c.in_pos = self.in_pos;
        c.out_pos = self.out_pos;
        c.bits = self.bits;
    }
}

/// Decodes the symbols of a Huffman block for as long as the input and
/// output are far enough from their ends that each iteration can work in
/// fixed-size windows of them, which spares it most bounds checks. This is
/// where nearly all the time goes, and it needs nothing from the callers'
/// variants: it never pauses, and it can't read past the end of the input.
/// The bit buffer is refilled and the next litlen entry preloaded before
/// each iteration to hide latency.
///
/// With `TAIL`, it runs to within a few bytes of the end of the output
/// instead of a whole match's worth, copies matches without overrunning
/// them, and stops before one that doesn't fit. That variant finishes the
/// output after the main one stops, and decodes small outputs (such as most
/// git objects) whole.
///
/// `t`'s litlen and offset tables must be built for the current block, or
/// the output is garbage.
#[inline(always)]
fn fastloop<const WRITE: bool, const TAIL: bool>(t: &Tables, s: &mut FastState<'_>) -> FastExit {
    let FastState {
        input,
        ref mut output,
        out_len,
        in_pos,
        mut out_pos,
        mut bits,
    } = *s;
    let mut saved_bitbuf: BitBuf;
    let mut entry: u32;
    let litlen_tablemask = bitmask(LITLEN_TABLEBITS);
    // The input not yet refilled from.
    let mut rest = &input[in_pos..];

    macro_rules! refill {
        () => {
            let held = bits.refill(BitBuf::from_le_bytes(*rest.first_chunk().unwrap()));
            rest = &rest[WORDBYTES - 1 - held..];
        };
    }

    // Writes the literal of the literal entry `$e`.
    macro_rules! write_literal {
        ($e:expr) => {
            let e: u32 = $e;
            if WRITE {
                output[out_pos] = (e >> 8) as u8;
            }
            out_pos += 1;
        };
    }

    let exit = 'done: {
        // Taken from `output` itself when writing, so that the compiler can
        // see the stores are in bounds.
        let out_len = if WRITE { output.len() } else { out_len };
        let margin = if TAIL {
            TAIL_MIN_OUT_ROOM - 1
        } else {
            FASTLOOP_MAX_BYTES_WRITTEN
        };
        let Some(out_end) = out_len.checked_sub(margin) else {
            break 'done FastExit::Margin;
        };
        if rest.len() < FASTLOOP_IN_WINDOW || out_pos >= out_end {
            break 'done FastExit::Margin;
        }
        refill!();
        entry = t.litlen[bits.peek(LITLEN_TABLEBITS)];
        loop {
            // Room for all that an iteration reads and writes. Besides
            // keeping clear of the buffers' ends, this lets the compiler
            // drop the bounds checks on refills and literal stores.
            if rest.len() < FASTLOOP_IN_WINDOW || out_pos >= out_end {
                break 'done FastExit::Margin;
            }
            'next: {
                // Consume the litlen entry, keeping the original bits
                // in case extra length bits must be extracted.
                saved_bitbuf = bits.buf;
                bits.consume_entry(entry);

                if entry & HUFFDEC_LITERAL != 0 {
                    // A literal that needed no subtable. On 64-bit,
                    // decode up to two more right away.
                    if EXTRA_LITERALS {
                        // Up to three literals, written to `run`.
                        // Without writes, a scratch array that the
                        // compiler removes.
                        let mut scratch = [0; 3];
                        let run: &mut [u8; 3] = if WRITE {
                            output[out_pos..].first_chunk_mut().unwrap()
                        } else {
                            &mut scratch
                        };
                        let mut n = 0;
                        macro_rules! write_literal {
                            ($e:expr) => {
                                let e: u32 = $e;
                                run[n] = (e >> 8) as u8;
                                n += 1;
                            };
                        }
                        let lit = entry;
                        entry = t.litlen[bits.buf & litlen_tablemask];
                        saved_bitbuf = bits.buf;
                        bits.consume_entry(entry);
                        write_literal!(lit);
                        if entry & HUFFDEC_LITERAL != 0 {
                            let lit = entry;
                            entry = t.litlen[bits.buf & litlen_tablemask];
                            saved_bitbuf = bits.buf;
                            bits.consume_entry(entry);
                            write_literal!(lit);
                            if entry & HUFFDEC_LITERAL != 0 {
                                // In lieu of the primary item, so it
                                // doesn't count as an extra.
                                let lit = entry;
                                entry = t.litlen[bits.buf & litlen_tablemask];
                                refill!();
                                write_literal!(lit);
                                out_pos += n;
                                break 'next;
                            }
                        }
                        out_pos += n;
                    } else {
                        const _: () = assert!(can_consume_and_then_preload(
                            LITLEN_TABLEBITS,
                            LITLEN_TABLEBITS
                        ));
                        let lit = entry;
                        entry = t.litlen[bits.buf & litlen_tablemask];
                        refill!();
                        write_literal!(lit);
                        break 'next;
                    }
                }

                // Not a literal: a length, a subtable pointer, or end
                // of block.
                if TAIL
                    && (entry & HUFFDEC_SUBTABLE_POINTER != 0
                        || (entry & HUFFDEC_EXCEPTIONAL == 0
                            && entry_value(entry, saved_bitbuf) > out_len - out_pos))
                {
                    // A match that doesn't fit (or that needs a subtable,
                    // which is rare) goes to the generic loop, which can
                    // pause or fail cleanly before it. Nothing since the
                    // entry was consumed, so put its bits back.
                    bits.buf = saved_bitbuf;
                    bits.left = bits.left.wrapping_add(entry);
                    break 'done FastExit::Margin;
                }
                if entry & HUFFDEC_EXCEPTIONAL != 0 {
                    if entry & HUFFDEC_END_OF_BLOCK != 0 {
                        break 'done FastExit::EndOfBlock;
                    }

                    // Subtable pointer. The subtable entry may be a
                    // literal, a length or end of block.
                    entry = t.litlen[bits.subtable_index(entry)];
                    saved_bitbuf = bits.buf;
                    bits.consume_entry(entry);

                    if !can_consume_and_then_preload(MAX_LITLEN_CODEWORD_LEN, LITLEN_TABLEBITS)
                        || !can_consume_and_then_preload(LENGTH_MAXBITS, OFFSET_TABLEBITS)
                    {
                        refill!();
                    }
                    if entry & HUFFDEC_LITERAL != 0 {
                        let lit = entry;
                        entry = t.litlen[bits.buf & litlen_tablemask];
                        refill!();
                        write_literal!(lit);
                        break 'next;
                    }
                    if entry & HUFFDEC_END_OF_BLOCK != 0 {
                        break 'done FastExit::EndOfBlock;
                    }
                }

                // Match length: base from the entry plus extra bits,
                // which were already consumed along with the entry.
                let length = entry_value(entry, saved_bitbuf);

                // Match offset. Enough bits are preloadable to look up
                // the offset entry; a refill may be needed before
                // consuming it.
                const _: () = assert!(can_consume_and_then_preload(
                    LENGTH_MAXFASTBITS,
                    OFFSET_TABLEBITS
                ));
                entry = t.offset[bits.peek(OFFSET_TABLEBITS)];
                if can_consume_and_then_preload(OFFSET_MAXBITS, LITLEN_TABLEBITS) {
                    // 64-bit: at most one refill, then the whole
                    // offset plus the next litlen preload fit.
                    if entry & HUFFDEC_EXCEPTIONAL != 0 {
                        if bits.avail() < OFFSET_MAXBITS + LITLEN_TABLEBITS - PRELOAD_SLACK {
                            refill!();
                        }
                        bits.consume(OFFSET_TABLEBITS);
                        entry = t.offset[bits.subtable_index(entry)];
                    } else if bits.avail() < OFFSET_MAXFASTBITS + LITLEN_TABLEBITS - PRELOAD_SLACK {
                        refill!();
                    }
                } else {
                    // 32-bit.
                    refill!();
                    if entry & HUFFDEC_EXCEPTIONAL != 0 {
                        bits.consume(OFFSET_TABLEBITS);
                        entry = t.offset[bits.subtable_index(entry)];
                        refill!();
                        const _: () = assert!(can_consume(OFFSET_MAXBITS - OFFSET_TABLEBITS));
                    } else {
                        const _: () = assert!(can_consume(OFFSET_MAXFASTBITS));
                    }
                }
                saved_bitbuf = bits.buf;
                bits.consume_entry(entry);
                let offset = entry_value(entry, saved_bitbuf);

                // Validate the offset; needed even in the fastloop.
                if offset > out_pos {
                    break 'done FastExit::BadOffset;
                }

                // Refill and preload the next litlen entry before
                // copying, so their latency overlaps the copy.
                if !can_consume_and_then_preload(OFFSET_MAX_REFILL_BITS, LITLEN_TABLEBITS)
                    && bits.avail() < LITLEN_TABLEBITS - PRELOAD_SLACK
                {
                    refill!();
                }
                entry = t.litlen[bits.buf & litlen_tablemask];
                refill!();

                if WRITE {
                    if TAIL {
                        // It fits, as checked above, but maybe without room
                        // to spare.
                        copy_match_slow(output, out_pos, offset, length);
                    } else {
                        copy_match(output, out_pos, offset, length);
                    }
                }
                out_pos += length;
            }
        }
    };
    (s.in_pos, s.out_pos, s.bits) = (input.len() - rest.len(), out_pos, bits);
    exit
}

/// Output bytes from the start of a match that copying it may touch: the
/// longest match, rounded up to whole chunks of 5 words.
const MATCH_SPAN: usize = MAX_MATCH_LEN + 5 * WORDBYTES - 1;

/// Copies the `length`-byte match at `offset` back to `output[dst..]`, a
/// word at a time, unconditionally copying 5 words first since that covers
/// most matches. It may write up to `5 * WORDBYTES - 1` bytes past the match.
#[inline(always)]
fn copy_match(output: &mut [u8], dst: usize, offset: usize, length: usize) {
    // A no-op for valid lengths, but it tells the compiler how far the
    // copy can reach, so it needs no bounds checks within `w`.
    let length = length.min(MAX_MATCH_LEN);
    // The match's source followed by its destination.
    let w = &mut output[dst - offset..dst + MATCH_SPAN];
    let mut i = 0;
    macro_rules! copy_word {
        ($k:expr) => {
            let x = *w[i + $k..].first_chunk::<WORDBYTES>().unwrap();
            *w[offset + i + $k..].first_chunk_mut().unwrap() = x;
        };
    }
    if offset >= WORDBYTES {
        loop {
            for k in 0..5 {
                copy_word!(k * WORDBYTES);
            }
            i += 5 * WORDBYTES;
            if i >= length {
                break;
            }
        }
    } else if offset == 1 {
        // A run of the previous byte. Keep it a multiple of 16 bytes so
        // it vectorizes.
        let b = w[0];
        loop {
            w[1 + i..][..4 * WORDBYTES].fill(b);
            i += 4 * WORDBYTES;
            if i >= length {
                break;
            }
        }
    } else {
        loop {
            copy_word!(0);
            copy_word!(offset);
            i += 2 * offset;
            if i >= length {
                break;
            }
        }
    }
}

/// Copies the `length`-byte match at `offset` back to `output[dst..]`
/// without writing past it.
#[inline(always)]
fn copy_match_slow(output: &mut [u8], dst: usize, offset: usize, length: usize) {
    if length <= 16 {
        // Most matches are short, and a byte loop is cheaper than a
        // `memmove` call for them.
        for i in dst..dst + length {
            output[i] = output[i - offset];
        }
        return;
    }
    // The bytes from `src` repeat with period `offset`, so each pass can
    // copy everything written so far, which doubles the span each time.
    let src = dst - offset;
    let end = dst + length;
    let mut pos = dst;
    while pos < end {
        let n = (pos - src).min(end - pos);
        output.copy_within(src..src + n, pos);
        pos += n;
    }
}

/// Decompresses one DEFLATE stream from `input` into `output`, with the
/// best compiled version of the decode loop for this CPU.
///
/// Without `WRITE`, nothing is written and `output` must be empty: it only
/// finds where the stream ends and how long its output is, up to `out_len`.
/// With `WRITE`, `out_len` is ignored.
///
/// With `RESUMABLE`, running out of output space returns [`Status::Full`]
/// instead of an error, and `resume` carries on from where an earlier call
/// with the same tables stopped: `input` must continue the stream at the
/// resume point's input position, `output` must hold the output written so
/// far (at least its last 32 KiB) before its output position, and the
/// tables must not have been used since. Without it, `resume` must be
/// `None`; keeping track of where to resume costs a few percent of speed, so
/// the fixed-buffer path doesn't.
///
/// With `RESUMABLE` and `more_input`, `input` may be followed by more of the
/// stream, so instead of reading past its end (as implicit zero bytes) the
/// decode stops at the last symbol or block boundary it could fully decode.
///
/// Building with `RUSTFLAGS="--cfg sirocco_no_bmi2"` skips the BMI2 loops,
/// so benchmarks can time the default loop on a CPU that has BMI2.
#[inline]
pub(super) fn run<const RESUMABLE: bool, const WRITE: bool>(
    t: &mut Tables,
    input: &[u8],
    output: &mut [u8],
    out_len: usize,
    resume: Option<&Resume>,
    more_input: bool,
) -> Result<Status, DecompressError> {
    #[cfg(all(target_arch = "x86_64", not(target_feature = "bmi2"), feature = "std"))]
    {
        if !cfg!(sirocco_no_bmi2) && std::is_x86_feature_detected!("bmi2") {
            // SAFETY: the CPU supports BMI2.
            return unsafe {
                run_bmi2::<RESUMABLE, WRITE>(t, input, output, out_len, resume, more_input)
            };
        }
    }
    decompress::<RESUMABLE, WRITE>(
        t,
        input,
        output,
        out_len,
        resume,
        more_input,
        |t, s, tail| {
            if WRITE && tail {
                fastloop_default::<WRITE, true>(t, s)
            } else {
                fastloop_default::<WRITE, false>(t, s)
            }
        },
    )
}

/// The one compiled copy of each fastloop variant without BMI2, shared by
/// the one-shot and resumable decode loops.
#[inline(never)]
fn fastloop_default<const WRITE: bool, const TAIL: bool>(
    t: &Tables,
    s: &mut FastState<'_>,
) -> FastExit {
    fastloop::<WRITE, TAIL>(t, s)
}

/// [`run`] compiled with BMI2, which turns the variable-width bit extractions
/// into `bzhi` / `shrx`. libdeflate does the same.
#[cfg(all(target_arch = "x86_64", not(target_feature = "bmi2"), feature = "std"))]
#[target_feature(enable = "bmi2")]
fn run_bmi2<const RESUMABLE: bool, const WRITE: bool>(
    t: &mut Tables,
    input: &[u8],
    output: &mut [u8],
    out_len: usize,
    resume: Option<&Resume>,
    more_input: bool,
) -> Result<Status, DecompressError> {
    // The closure inherits BMI2 from this function, so it can call
    // `fastloop_bmi2` safely.
    decompress::<RESUMABLE, WRITE>(
        t,
        input,
        output,
        out_len,
        resume,
        more_input,
        |t, s, tail| {
            if WRITE && tail {
                fastloop_bmi2::<WRITE, true>(t, s)
            } else {
                fastloop_bmi2::<WRITE, false>(t, s)
            }
        },
    )
}

/// The fastloop compiled with BMI2, shared like [`fastloop_default`].
#[cfg(all(target_arch = "x86_64", not(target_feature = "bmi2"), feature = "std"))]
#[target_feature(enable = "bmi2")]
#[inline(never)]
fn fastloop_bmi2<const WRITE: bool, const TAIL: bool>(
    t: &Tables,
    s: &mut FastState<'_>,
) -> FastExit {
    fastloop::<WRITE, TAIL>(t, s)
}

/// The decoder behind [`run`], given the compiled `fastloop` to call, which
/// gets whether to run its `TAIL` variant.
#[inline(always)]
fn decompress<const RESUMABLE: bool, const WRITE: bool>(
    t: &mut Tables,
    input: &[u8],
    output: &mut [u8],
    out_len: usize,
    resume: Option<&Resume>,
    more_input: bool,
    fastloop: impl Fn(&Tables, &mut FastState<'_>, bool) -> FastExit,
) -> Result<Status, DecompressError> {
    use DecompressError::*;

    let resume = if RESUMABLE { resume } else { None };
    let out_len = if WRITE { output.len() } else { out_len };
    let more_input = RESUMABLE && more_input;
    let mut c = resume.map_or(Cursor::START, |r| r.at);
    assert!(c.in_pos <= input.len() && c.out_pos <= out_len);
    let mut resume_in_block = resume.and_then(|r| r.in_block);
    let mut resume_stored = resume.map_or(0, |r| r.stored_left);
    let stop_at_stored = RESUMABLE && WRITE && resume.is_some_and(|r| r.stop_at_stored);

    // Returns a resume point at the cursor `$at`, at the start of the
    // current symbol (or, with `in_block` None, the current block).
    macro_rules! pause {
        ($at:expr, $in_block:expr, $need_input:expr) => {
            return Ok(Status::Full(Resume::new($at, $in_block, $need_input)))
        };
    }

    // Pauses inside a stored block with `$left` bytes of it to go.
    macro_rules! pause_stored {
        ($left:expr, $is_final_block:expr, $need_input:expr) => {
            return Ok(Status::Full(Resume {
                stored_left: $left as u16,
                ..Resume::new(c, Some($is_final_block), $need_input)
            }))
        };
    }

    // Pauses at `$at` when the output is full. Nothing of that symbol or
    // block may have been written yet.
    macro_rules! full {
        ($at:expr, $in_block:expr) => {{
            if !RESUMABLE {
                return Err(InsufficientSpace);
            }
            pause!($at, $in_block, false)
        }};
    }

    // The last symbol or block start, for pausing when more input is
    // needed: decoding past the end of `input` read implicit zero bytes,
    // which may stand in for input yet to come.
    let mut last = (c, resume_in_block);

    // With more input to come, errors seen after reading past the end of
    // `input` may be down to the implicit zero bytes, so pause at the last
    // snapshot instead: the error comes back after resuming if it is real.
    macro_rules! bad {
        () => {{
            if more_input && c.overread > 0 {
                pause!(last.0, last.1, true);
            }
            return Err(BadData);
        }};
    }

    // Refills to at least CONSUMABLE_NBITS consumable bits, reading zeroes
    // past the end of the input.
    macro_rules! refill_bits {
        () => {
            if let Some(word) = input.get(c.in_pos..c.in_pos + WORDBYTES) {
                let held = c
                    .bits
                    .refill(BitBuf::from_le_bytes(word.try_into().unwrap()));
                c.in_pos += WORDBYTES - 1 - held;
            } else {
                while c.bits.avail() < CONSUMABLE_NBITS {
                    if let Some(&b) = input.get(c.in_pos) {
                        c.bits.buf |= (b as BitBuf) << c.bits.avail();
                        c.in_pos += 1;
                    } else {
                        c.overread += 1;
                        if c.overread > WORDBYTES {
                            bad!();
                        }
                    }
                    c.bits.left = c.bits.left.wrapping_add(8);
                }
            }
        };
    }

    loop {
        // Start of a block. With more input to come, the previous block may
        // have ended in bits that aren't there yet.
        if more_input && c.consumed_overread() {
            pause!(last.0, last.1, true);
        }
        let block_start = c;
        last = (c, resume_in_block);
        let is_final_block;
        // With `RESUMABLE`, the bytes of a stored block still to copy.
        let mut stored = None;
        if let Some(f) = resume_in_block.take() {
            // Resuming inside a block: a stored one, or a Huffman one whose
            // tables are already built.
            is_final_block = f;
            if resume_stored > 0 {
                stored = Some(usize::from(core::mem::take(&mut resume_stored)));
            }
        } else {
            const _: () = assert!(can_consume(1 + 2 + 5 + 5 + 4));
            refill_bits!();

            is_final_block = c.bits.buf & 1 != 0;
            let block_type = (c.bits.buf >> 1) & bitmask(2);

            if block_type == DEFLATE_BLOCKTYPE_DYNAMIC_HUFFMAN {
                let num_litlen_syms = 257 + ((c.bits.buf >> 3) & bitmask(5)) as usize;
                let num_offset_syms = 1 + ((c.bits.buf >> 8) & bitmask(5)) as usize;
                let num_explicit_precode_lens = 4 + ((c.bits.buf >> 13) & bitmask(4)) as usize;
                c.bits.consume(17);

                // Read the precode codeword lengths.
                let (explicit, implicit) =
                    PRECODE_LENS_PERMUTATION.split_at(num_explicit_precode_lens);
                for &sym in explicit {
                    if c.bits.avail() < 3 {
                        refill_bits!();
                    }
                    t.precode_lens[sym as usize] = c.bits.peek(3) as u8;
                    c.bits.consume(3);
                }
                for &sym in implicit {
                    t.precode_lens[sym as usize] = 0;
                }

                if !t.build_precode() {
                    bad!();
                }

                // Decode the litlen and offset codeword lengths.
                let num_lens = num_litlen_syms + num_offset_syms;
                let lens = &mut t.lens;
                let mut i = 0;
                while i < num_lens {
                    if c.bits.avail() < MAX_PRE_CODEWORD_LEN + 7 {
                        refill_bits!();
                    }

                    // The precode table has no subtables.
                    const _: () = assert!(PRECODE_TABLEBITS == MAX_PRE_CODEWORD_LEN);
                    let entry = t.precode[c.bits.peek(MAX_PRE_CODEWORD_LEN)];
                    c.bits.consume_entry(entry);
                    let presym = entry >> 16;

                    if presym < 16 {
                        // Explicit codeword length.
                        lens[i] = presym as u8;
                        i += 1;
                    } else if presym == 16 {
                        // Repeat the previous length 3 - 6 times. `lens` has room
                        // for the worst-case overrun, so always write 6.
                        if i == 0 {
                            bad!();
                        }
                        let rep_val = lens[i - 1];
                        let rep_count = 3 + c.bits.peek(2);
                        c.bits.consume(2);
                        lens[i..i + 6].fill(rep_val);
                        i += rep_count;
                    } else if presym == 17 {
                        // Repeat zero 3 - 10 times.
                        let rep_count = 3 + c.bits.peek(3);
                        c.bits.consume(3);
                        lens[i..i + 10].fill(0);
                        i += rep_count;
                    } else {
                        // Repeat zero 11 - 138 times.
                        let rep_count = 11 + c.bits.peek(7);
                        c.bits.consume(7);
                        lens[i..i + rep_count].fill(0);
                        i += rep_count;
                    }
                }

                // Unnecessary, but checked for consistency with zlib.
                if i != num_lens {
                    bad!();
                }

                if !t.build_dynamic(num_litlen_syms, num_offset_syms) {
                    bad!();
                }
            } else if block_type == DEFLATE_BLOCKTYPE_UNCOMPRESSED {
                // Uncompressed block: align to the next byte boundary as if
                // reading a byte at a time.
                c.bits.consume(3);
                if c.consumed_overread() {
                    bad!();
                }
                c.align_to_byte();

                let Some(header) = input.get(c.in_pos..c.in_pos + 4) else {
                    if more_input {
                        pause!(block_start, None, true);
                    }
                    bad!();
                };
                let len = u16::from_le_bytes([header[0], header[1]]);
                let nlen = u16::from_le_bytes([header[2], header[3]]);
                if len != !nlen {
                    bad!();
                }
                let len = len as usize;
                let data = input.get(c.in_pos + 4..c.in_pos + 4 + len);
                match data {
                    Some(data) if len <= out_len - c.out_pos && !stop_at_stored => {
                        if WRITE {
                            output[c.out_pos..c.out_pos + len].copy_from_slice(data);
                        }
                        c.in_pos += 4 + len;
                        c.out_pos += len;

                        if is_final_block {
                            break;
                        }
                        continue;
                    }
                    // Short of input or room, or asked to stop: copied below
                    // as far as there is input and room for, if at all.
                    _ if RESUMABLE => {
                        c.in_pos += 4;
                        stored = Some(len);
                        if stop_at_stored && len > 0 {
                            pause_stored!(len, is_final_block, false);
                        }
                    }
                    _ => {
                        if len > out_len - c.out_pos {
                            // Nothing of this block has been written yet.
                            full!(block_start, None);
                        }
                        bad!();
                    }
                }
            } else {
                if block_type != DEFLATE_BLOCKTYPE_STATIC_HUFFMAN {
                    bad!();
                }
                c.bits.consume(3);

                t.load_static();
            }
        }

        if let Some(left) = stored {
            // A stored block, when resumable: copy what there is input and
            // room for, and pause inside the block if that isn't all of it.
            let n = left.min(input.len() - c.in_pos).min(out_len - c.out_pos);
            if WRITE {
                output[c.out_pos..c.out_pos + n].copy_from_slice(&input[c.in_pos..c.in_pos + n]);
            }
            c.in_pos += n;
            c.out_pos += n;
            if n < left {
                if c.out_pos < out_len {
                    if !more_input {
                        bad!();
                    }
                    pause_stored!(left - n, is_final_block, true);
                }
                pause_stored!(left - n, is_final_block, false);
            }
            if is_final_block {
                break;
            }
            continue;
        }

        'block_done: {
            // The fastloop, while the input and output are far from their
            // ends. Every variant of this function calls the same compiled
            // copy of it.
            let mut s = FastState::new(input, output, out_len, &c);
            // Near the end of the output, carry on in (or go straight to)
            // the tail variant, which checks that each match fits instead.
            let near_end =
                |s: &FastState<'_>| WRITE && s.out_len - s.out_pos < FASTLOOP_MAX_BYTES_WRITTEN;
            let mut exit = if near_end(&s) {
                FastExit::Margin
            } else {
                fastloop(t, &mut s, false)
            };
            if matches!(exit, FastExit::Margin) && near_end(&s) {
                exit = fastloop(t, &mut s, true);
            }
            s.finish(&mut c);
            match exit {
                FastExit::EndOfBlock => break 'block_done,
                FastExit::BadOffset => bad!(),
                FastExit::Margin => {}
            }

            // The generic loop, for when the input or output is near its end.
            // Not usually performance-critical.
            loop {
                if more_input && c.consumed_overread() {
                    pause!(last.0, last.1, true);
                }
                let symbol_start = c;
                if RESUMABLE {
                    last = (c, Some(is_final_block));
                }
                refill_bits!();
                let mut entry = t.litlen[c.bits.peek(LITLEN_TABLEBITS)];
                let mut saved_bitbuf = c.bits.buf;
                c.bits.consume_entry(entry);
                // Literal entries keep the literal in the bits that would
                // hold the subtable pointer flag.
                if entry & (HUFFDEC_LITERAL | HUFFDEC_SUBTABLE_POINTER) == HUFFDEC_SUBTABLE_POINTER
                {
                    entry = t.litlen[c.bits.subtable_index(entry)];
                    saved_bitbuf = c.bits.buf;
                    c.bits.consume_entry(entry);
                }
                if entry & HUFFDEC_LITERAL != 0 {
                    if c.out_pos == out_len {
                        full!(symbol_start, Some(is_final_block));
                    }
                    if WRITE {
                        output[c.out_pos] = (entry >> 8) as u8;
                    }
                    c.out_pos += 1;
                    continue;
                }
                if entry & HUFFDEC_END_OF_BLOCK != 0 {
                    break 'block_done;
                }
                let length = entry_value(entry, saved_bitbuf);
                if length > out_len - c.out_pos {
                    full!(symbol_start, Some(is_final_block));
                }

                if !can_consume(LENGTH_MAXBITS + OFFSET_MAXBITS) {
                    refill_bits!();
                }
                entry = t.offset[c.bits.peek(OFFSET_TABLEBITS)];
                if entry & HUFFDEC_EXCEPTIONAL != 0 {
                    c.bits.consume(OFFSET_TABLEBITS);
                    entry = t.offset[c.bits.subtable_index(entry)];
                    if !can_consume(OFFSET_MAXBITS) {
                        refill_bits!();
                    }
                }
                let offset = entry_value(entry, c.bits.buf);
                c.bits.consume_entry(entry);

                if offset > c.out_pos {
                    bad!();
                }
                if WRITE {
                    copy_match_slow(output, c.out_pos, offset, length);
                }
                c.out_pos += length;
            }
        }

        // Finished a block.
        if is_final_block {
            break;
        }
    }

    // That was the last block. If any implicit zero bytes were consumed (not
    // just refilled) before the end of the stream, the data is bad (or, with
    // more input to come, incomplete). Otherwise don't count bytes that were
    // refilled but not consumed.
    if c.consumed_overread() {
        bad!();
    }
    c.align_to_byte();
    Ok(Status::Done(c.in_pos, c.out_pos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn copy_match_slow_matches_byte_copy() {
        let start: Vec<u8> = (0..300u32).map(|i| (i * 7 + 3) as u8).collect();
        for offset in 1..=70 {
            for length in 3..=MAX_MATCH_LEN {
                let dst = 100;
                let mut want = start.clone();
                want.resize(dst + length + 8, 0);
                let mut got = want.clone();
                for i in dst..dst + length {
                    want[i] = want[i - offset];
                }
                copy_match_slow(&mut got, dst, offset, length);
                assert_eq!(got, want, "offset {offset}, length {length}");
            }
        }
    }
}
