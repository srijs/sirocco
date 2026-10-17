//! Every codec must decompress libdeflate's output, which the benchmarks
//! feed them all.

#[test]
fn codecs_decompress_libdeflate_output() {
    // The 4 MiB samples take minutes in debug builds.
    let small = compare::synthetic_corpus()
        .into_iter()
        .filter(|s| s.data.len() <= 256 << 10);
    for s in small {
        for level in [0, 1, 6, 9, 12] {
            let compressed = compare::reference_compress(compare::Format::Deflate, level, &s.data);
            for d in compare::codecs() {
                assert!(
                    d.decompress(&compressed, s.data.len()) == s.data,
                    "L{level} -> {} on {}",
                    d.name,
                    s.name
                );
            }
        }
    }
}
