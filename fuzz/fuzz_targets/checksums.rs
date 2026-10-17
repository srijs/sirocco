#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut a = libdeflater::Adler32::new();
    a.update(data);
    assert_eq!(sirocco::adler32(1, data), a.sum());

    let mut c = libdeflater::Crc::new();
    c.update(data);
    assert_eq!(sirocco::crc32(0, data), c.sum());
});
