//! Raw DEFLATE decompression speed of every codec in `compare::codecs()`.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

fn bench(c: &mut Criterion) {
    let corpus = compare::corpus();
    let codecs = compare::codecs();

    // Every codec decompresses the same stream: libdeflate's at level 6.
    let mut g = c.benchmark_group("decompress");
    for s in &corpus {
        let compressed = compare::reference_compress(compare::Format::Deflate, 6, &s.data);
        g.throughput(Throughput::Bytes(s.data.len() as u64));
        for codec in &codecs {
            let id = BenchmarkId::new(codec.name, &s.name);
            let mut out = vec![0; s.data.len()];
            g.bench_with_input(id, &compressed, |b, d| {
                b.iter(|| (codec.decompress_into)(d, &mut out));
            });
        }
    }
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
