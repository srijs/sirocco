use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

fn bench(c: &mut Criterion) {
    let corpus = compare::corpus();

    let mut g = c.benchmark_group("adler32");
    for s in &corpus {
        g.throughput(Throughput::Bytes(s.data.len() as u64));
        // Every implementation this CPU supports, not just the dispatched one.
        for imp in adler32furious::implementations() {
            let id = BenchmarkId::new(format!("adler32furious-{}", imp.name()), &s.name);
            g.bench_with_input(id, &s.data, |b, d| {
                b.iter(|| {
                    let mut sum = adler32furious::Adler32::new();
                    imp.update(&mut sum, d);
                    sum.checksum()
                });
            });
        }
        g.bench_with_input(BenchmarkId::new("libdeflate", &s.name), &s.data, |b, d| {
            b.iter(|| {
                let mut a = libdeflater::Adler32::new();
                a.update(d);
                a.sum()
            });
        });
        g.bench_with_input(BenchmarkId::new("zlib-rs", &s.name), &s.data, |b, d| {
            b.iter(|| zlib_rs::adler32::adler32(1, d));
        });
        // miniz_oxide's Adler-32.
        g.bench_with_input(BenchmarkId::new("adler2", &s.name), &s.data, |b, d| {
            b.iter(|| adler2::adler32_slice(d));
        });
        #[cfg(feature = "zenflate")]
        g.bench_with_input(BenchmarkId::new("zenflate", &s.name), &s.data, |b, d| {
            b.iter(|| zenflate::adler32(1, d));
        });
    }
    g.finish();

    let mut g = c.benchmark_group("crc32");
    for s in &corpus {
        g.throughput(Throughput::Bytes(s.data.len() as u64));
        g.bench_with_input(BenchmarkId::new("crc32fast", &s.name), &s.data, |b, d| {
            b.iter(|| crc32fast::hash(d));
        });
        g.bench_with_input(BenchmarkId::new("libdeflate", &s.name), &s.data, |b, d| {
            b.iter(|| {
                let mut c = libdeflater::Crc::new();
                c.update(d);
                c.sum()
            });
        });
        g.bench_with_input(BenchmarkId::new("zlib-rs", &s.name), &s.data, |b, d| {
            b.iter(|| zlib_rs::crc32::crc32(0, d));
        });
        #[cfg(feature = "zenflate")]
        g.bench_with_input(BenchmarkId::new("zenflate", &s.name), &s.data, |b, d| {
            b.iter(|| zenflate::crc32(0, d));
        });
    }
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
