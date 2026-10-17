use compare::{reference_compress, Format};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

fn bench(c: &mut Criterion) {
    let corpus = compare::corpus();

    for (format, name) in [
        (Format::Deflate, "deflate_decompress"),
        (Format::Zlib, "zlib_decompress"),
        (Format::Gzip, "gzip_decompress"),
    ] {
        let mut g = c.benchmark_group(name);
        for s in &corpus {
            // Benchmark the levels people use most: default and fastest.
            let levels: &[i32] = if format == Format::Deflate {
                &[1, 6]
            } else {
                &[6]
            };
            for &level in levels {
                let compressed = reference_compress(format, level, &s.data);
                let id = format!("{}/L{level}", s.name);
                let mut out = vec![0u8; s.data.len()];
                // Throughput in uncompressed bytes, like libdeflate's benchmark.
                g.throughput(Throughput::Bytes(s.data.len() as u64));

                let mut d = sirocco::Decompressor::new(format.ff());
                g.bench_with_input(BenchmarkId::new("sirocco", &id), &compressed, |b, c| {
                    b.iter(|| d.decompress_into(c, &mut out));
                });

                let mut d = libdeflater::Decompressor::new();
                g.bench_with_input(BenchmarkId::new("libdeflate", &id), &compressed, |b, c| {
                    b.iter(|| match format {
                        Format::Deflate => d.deflate_decompress(c, &mut out),
                        Format::Zlib => d.zlib_decompress(c, &mut out),
                        Format::Gzip => d.gzip_decompress(c, &mut out),
                    });
                });
            }
        }
        g.finish();
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
