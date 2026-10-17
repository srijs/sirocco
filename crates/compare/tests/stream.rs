//! Differential tests for the streaming decompressor: fed in arbitrary
//! pieces, it must agree with libdeflate's one-shot decompressor on output,
//! consumed input and whether the stream is valid.

use compare::{
    reference_compress, sirocco_stream as stream, Format, Outcome, ReferenceDecompressor,
};
use proptest::prelude::*;

fn check_same(format: Format, input: &[u8], ins: &[usize], outs: &[usize]) {
    let ours = stream(format, input, ins, outs);
    let theirs = ReferenceDecompressor::new().decompress(format, input, 4 << 20);
    assert_eq!(
        ours,
        theirs,
        "{format:?}, {} input bytes, ins {ins:?}, outs {outs:?}",
        input.len()
    );
}

#[test]
fn corpus_streams_at_every_level() {
    for s in compare::corpus() {
        for level in 0..=12 {
            for format in Format::ALL {
                let mut c = reference_compress(format, level, &s.data);
                c.extend_from_slice(&[0xA5; 37]);
                for (ins, outs) in [
                    (&[usize::MAX][..], &[1 << 17][..]),
                    (&[32768][..], &[32768][..]),
                    (&[13, 4096][..], &[4099, 1, 65536][..]),
                ] {
                    let r = stream(format, &c, ins, outs);
                    let what = format!(
                        "{} level {level} {format:?} ins {ins:?} outs {outs:?}",
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
                assert_eq!(stream(format, &c[..len], &[97], &[1000]), Outcome::BadData);
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
    fn chunked_matches_libdeflate(
        data in mixed_data(),
        level in 0i32..=12,
        f in 0usize..3,
        ins in sizes(5000),
        outs in sizes(70_000),
    ) {
        check_same(Format::ALL[f], &reference_compress(Format::ALL[f], level, &data), &ins, &outs);
    }

    #[test]
    fn corrupted_chunked_matches_libdeflate(
        data in mixed_data(),
        level in 0i32..=12,
        f in 0usize..3,
        flips in prop::collection::vec((any::<prop::sample::Index>(), 0u8..8), 1..4),
        ins in sizes(5000),
        outs in sizes(70_000),
    ) {
        let format = Format::ALL[f];
        let mut c = reference_compress(format, level, &data);
        for (i, bit) in flips {
            let i = i.index(c.len());
            c[i] ^= 1 << bit;
        }
        check_same(format, &c, &ins, &outs);
    }
}
