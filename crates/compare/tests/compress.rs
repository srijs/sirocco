//! Differential tests of sirocco's compressor: every stream it writes, in
//! one call or streamed with flushes, must decompress with zlib (and
//! zlib-rs, libdeflate, miniz_oxide and sirocco) to the input, and each
//! level must compress about as well as zlib's.

#![allow(clippy::cast_precision_loss)]

use compare::deflate::{
    check_round_trip, check_stream, sirocco_compress, sirocco_compress_stream, zlib_compress, Step,
};
use compare::Format;
use proptest::prelude::*;
use sirocco::deflate::{compress_bound, Flush, MIN_BUFFER_LEN};

#[test]
fn round_trips_at_every_level() {
    for s in compare::test_corpus() {
        for level in 0..=9 {
            for format in Format::ALL {
                let c = sirocco_compress(format, level, &s.data);
                let what = format!("{} level {level} {format:?}", s.name);
                assert!(
                    c.len() <= compress_bound(format.ff(), s.data.len()),
                    "{what}: over the bound"
                );
                check_round_trip(format, &c, &s.data, &what);
            }
        }
    }
}

/// sirocco's size at each level against zlib's, per sample: within `slack`
/// (and 16 bytes), and in all within `total_slack`.
fn check_sizes(samples: &[compare::Sample], slack: f64, total_slack: f64) {
    for level in 1..=9u8 {
        let (mut ours, mut theirs) = (0, 0);
        for s in samples {
            let a = sirocco_compress(Format::Deflate, level, &s.data).len();
            let b = zlib_compress(Format::Deflate, level.into(), &s.data).len();
            assert!(
                a as f64 <= b as f64 * (1.0 + slack) + 16.0,
                "{} level {level}: {a} bytes, zlib {b}",
                s.name
            );
            ours += a;
            theirs += b;
        }
        assert!(
            ours as f64 <= theirs as f64 * (1.0 + total_slack),
            "level {level}: {ours} bytes in all, zlib {theirs}"
        );
    }
}

/// Each level compresses about as well as zlib's on the synthetic samples.
/// (`cargo run -p compare --release --bin compress -- --max-size-vs-zlib 1`
/// checks it on the real corpus.)
///
/// The synthetic text is random picks from 20 words: it rewards deep
/// searches for short matches, where zlib's 3-byte hash chains do better
/// than libdeflate's design, which sirocco follows. A single repeated byte
/// compresses so well that the header of each 64 KiB block shows.
#[test]
fn sizes_match_zlib() {
    let (text, rest): (Vec<_>, Vec<_>) = compare::test_corpus()
        .into_iter()
        .partition(|s| s.name.starts_with("text") || s.name.starts_with("repeat"));
    check_sizes(&rest, 0.01, 0.01);
    check_sizes(&text, 0.25, 0.25);
}

/// Data with a mix of literals, short-offset matches and long runs.
fn mixed_data() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop_oneof![
            prop::collection::vec(any::<u8>(), 0..64),
            (any::<u8>(), 1usize..600).prop_map(|(b, n)| vec![b; n]),
            (prop::collection::vec(any::<u8>(), 1..9), 1usize..80).prop_map(|(p, n)| p.repeat(n)),
            prop::collection::vec(prop::sample::select(b"abcde ".to_vec()), 0..300),
        ],
        0..60,
    )
    .prop_map(|chunks| chunks.concat())
}

/// Larger data: `mixed_data` repeated with changes, so that matches reach
/// far back and blocks fill up.
fn big_data() -> impl Strategy<Value = Vec<u8>> {
    (mixed_data(), 1usize..300, any::<u64>()).prop_map(|(base, reps, seed)| {
        let mut rng = compare::Rng(seed | 1);
        let mut out = Vec::new();
        for _ in 0..reps {
            out.extend_from_slice(&base);
            for _ in 0..rng.next() % 8 {
                out.push(rng.next() as u8);
            }
        }
        out
    })
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let flush = prop_oneof![
        8 => Just(Flush::None),
        1 => Just(Flush::Sync),
        1 => Just(Flush::Full),
    ];
    let out_len = prop_oneof![1usize..16, 16usize..4096, Just(1 << 20)];
    prop::collection::vec(
        (0usize..100_000, flush, out_len).prop_map(|(add, flush, out_len)| Step {
            add,
            flush,
            out_len,
        }),
        1..10,
    )
}

fn cases() -> u32 {
    std::env::var("SIROCCO_PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(64)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn small_inputs_round_trip(data in mixed_data(), level in 0u8..=9, f in 0usize..3) {
        let format = Format::ALL[f];
        let c = sirocco_compress(format, level, &data);
        prop_assert!(c.len() <= compress_bound(format.ff(), data.len()));
        check_round_trip(format, &c, &data, &format!("level {level} {format:?}"));
    }

    #[test]
    fn streams_round_trip(
        data in big_data(),
        level in 0u8..=9,
        f in 0usize..3,
        steps in steps(),
        min_buf in any::<bool>(),
    ) {
        let format = Format::ALL[f];
        let buf_len = if min_buf { MIN_BUFFER_LEN } else { 1 << 20 };
        let s = sirocco_compress_stream(format, level, &data, &steps, buf_len);
        check_stream(format, &s, &data, &format!("level {level} {format:?} {steps:?}"));
    }
}

/// Streaming without flushes compresses as well as one call, however the
/// input and output are cut up. (Not byte for byte: one call knows where the
/// input ends, which moves the last block boundaries.)
#[test]
fn streaming_compresses_as_well_as_one_call() {
    let data = &compare::synthetic_corpus()[5].data;
    for level in [1, 3, 6, 9] {
        let one = sirocco_compress(Format::Gzip, level, data).len();
        for (add, out_len) in [(1, 1 << 20), (1000, 7), (65_536, 4096)] {
            let steps = [Step {
                add,
                flush: Flush::None,
                out_len,
            }];
            let s = sirocco_compress_stream(Format::Gzip, level, data, &steps, MIN_BUFFER_LEN);
            let len = s.compressed.len();
            assert!(
                len <= one + one / 200,
                "level {level}, {add}-byte pieces into {out_len}: {len}, one call {one}"
            );
            check_round_trip(Format::Gzip, &s.compressed, data, &"stream");
        }
    }
}
