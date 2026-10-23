//! Differential tests for streaming: fed in pieces, through a buffer smaller
//! than the output, sirocco must agree with zlib-rs's one-shot decoder.

use compare::{
    assert_agree, reference_compress, sirocco_stream as stream, Format, Outcome,
    ReferenceDecompressor,
};
use proptest::prelude::*;

fn check_same(format: Format, input: &[u8], ins: &[usize], buf_len: usize) {
    let ours = stream(format, input, ins, buf_len);
    let theirs = ReferenceDecompressor::new().decompress(format, input, 4 << 20);
    let what = format!(
        "{format:?}, {} input bytes, ins {ins:?}, buffer {buf_len}",
        input.len()
    );
    assert_agree(&ours, &theirs, &what);
}

/// From the smallest streaming buffer up.
const BUF_LENS: [usize; 3] = [64 << 10, (64 << 10) + 4099, 1 << 17];

#[test]
fn corpus_streams_at_every_level() {
    for s in compare::corpus() {
        for level in 0..=12 {
            for format in Format::ALL {
                let mut c = reference_compress(format, level, &s.data);
                c.extend_from_slice(&[0xA5; 37]);
                for (ins, buf_len) in [
                    (&[usize::MAX][..], BUF_LENS[2]),
                    (&[32768][..], BUF_LENS[0]),
                    (&[13, 4096][..], BUF_LENS[1]),
                ] {
                    let r = stream(format, &c, ins, buf_len);
                    let what = format!(
                        "{} level {level} {format:?} ins {ins:?} buffer {buf_len}",
                        s.name
                    );
                    assert_eq!(r, Outcome::Ok(c.len() - 37, s.data.clone()), "{what}");
                }
            }
        }
    }
}

#[test]
fn truncated_streams_are_errors() {
    let data = &compare::synthetic_corpus()[2].data; // text, 4 KiB
    for level in [0, 1, 6, 12] {
        for format in Format::ALL {
            let c = reference_compress(format, level, data);
            for len in 0..c.len() {
                assert_eq!(
                    stream(format, &c[..len], &[97], 1 << 16),
                    Outcome::Truncated
                );
            }
        }
    }
}

fn mixed_data() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop_oneof![
            prop::collection::vec(any::<u8>(), 0..64),
            (any::<u8>(), 1usize..600).prop_map(|(b, n)| vec![b; n]),
            (prop::collection::vec(any::<u8>(), 1..9), 1usize..80).prop_map(|(p, n)| p.repeat(n)),
            prop::collection::vec(prop::sample::select(b"abcde".to_vec()), 0..300),
        ],
        0..120,
    )
    .prop_map(|chunks| chunks.concat())
}

fn sizes(max: usize) -> impl Strategy<Value = Vec<usize>> {
    prop::collection::vec(prop_oneof![1usize..16, 1..max], 1..8)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn chunked_matches_zlib(
        data in mixed_data(),
        level in 0i32..=12,
        f in 0usize..3,
        ins in sizes(5000),
        buf in 0usize..3,
    ) {
        check_same(Format::ALL[f], &reference_compress(Format::ALL[f], level, &data), &ins, BUF_LENS[buf]);
    }

    #[test]
    fn corrupted_chunked_matches_zlib(
        data in mixed_data(),
        level in 0i32..=12,
        f in 0usize..3,
        flips in prop::collection::vec((any::<prop::sample::Index>(), 0u8..8), 1..4),
        ins in sizes(5000),
        buf in 0usize..3,
    ) {
        let format = Format::ALL[f];
        let mut c = reference_compress(format, level, &data);
        for (i, bit) in flips {
            let i = i.index(c.len());
            c[i] ^= 1 << bit;
        }
        check_same(format, &c, &ins, BUF_LENS[buf]);
    }
}
