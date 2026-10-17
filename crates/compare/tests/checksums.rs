use proptest::prelude::*;

fn reference_adler32(data: &[u8]) -> u32 {
    let mut a = libdeflater::Adler32::new();
    a.update(data);
    a.sum()
}

/// Every Adler-32 implementation `adler32furious` has for this CPU, as
/// `(name, checksum of data)`.
fn adler32_all(data: &[u8]) -> impl Iterator<Item = (&'static str, u32)> + '_ {
    adler32furious::implementations().map(move |imp| {
        let mut sum = adler32furious::Adler32::new();
        imp.update(&mut sum, data);
        (imp.name(), sum.checksum())
    })
}

fn reference_crc32(data: &[u8]) -> u32 {
    let mut c = libdeflater::Crc::new();
    c.update(data);
    c.sum()
}

#[test]
fn corpus_matches_libdeflate() {
    for s in compare::corpus() {
        let want = reference_adler32(&s.data);
        assert_eq!(sirocco::adler32(1, &s.data), want, "adler32 {}", s.name);
        for (name, got) in adler32_all(&s.data) {
            assert_eq!(got, want, "adler32 ({name}) {}", s.name);
        }
        assert_eq!(
            sirocco::crc32(0, &s.data),
            reference_crc32(&s.data),
            "crc32 {}",
            s.name
        );
    }
}

proptest! {
    #[test]
    fn adler32_matches_libdeflate(
        data in proptest::collection::vec(any::<u8>(), 0..20_000),
        offset in 0usize..64,
    ) {
        let data = &data[offset.min(data.len())..];
        let want = reference_adler32(data);
        prop_assert_eq!(sirocco::adler32(1, data), want);
        for (name, got) in adler32_all(data) {
            prop_assert_eq!(got, want, "{}", name);
        }
    }

    #[test]
    fn crc32_matches_libdeflate(data in proptest::collection::vec(any::<u8>(), 0..20_000)) {
        prop_assert_eq!(sirocco::crc32(0, &data), reference_crc32(&data));
    }
}
