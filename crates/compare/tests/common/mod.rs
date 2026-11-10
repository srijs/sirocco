//! Helpers shared by the differential tests.

use proptest::prelude::*;

/// Data with a mix of literals, short-offset matches and long runs, so that
/// all of the match copy paths get used: up to `max_chunks` pieces, the
/// text-like ones over `abcde`, and a space too if `space`.
pub(crate) fn mixed_data(max_chunks: usize, space: bool) -> impl Strategy<Value = Vec<u8>> {
    let alphabet: &[u8] = if space { b"abcde " } else { b"abcde" };
    prop::collection::vec(
        prop_oneof![
            prop::collection::vec(any::<u8>(), 0..64),
            (any::<u8>(), 1usize..600).prop_map(|(b, n)| vec![b; n]),
            (prop::collection::vec(any::<u8>(), 1..9), 1usize..80).prop_map(|(p, n)| p.repeat(n)),
            prop::collection::vec(prop::sample::select(alphabet.to_vec()), 0..300),
        ],
        0..max_chunks,
    )
    .prop_map(|chunks| chunks.concat())
}

/// Flips one bit of `data` per `(byte, bit)` in `flips`.
#[allow(dead_code)] // Not every test corrupts streams.
pub(crate) fn flip_bits(data: &mut [u8], flips: Vec<(prop::sample::Index, u8)>) {
    for (i, bit) in flips {
        let i = i.index(data.len());
        data[i] ^= 1 << bit;
    }
}
