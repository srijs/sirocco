# sirocco

[![crates.io](https://img.shields.io/crates/v/sirocco.svg)](https://crates.io/crates/sirocco)
[![docs.rs](https://img.shields.io/docsrs/sirocco)](https://docs.rs/sirocco)

Fast DEFLATE, zlib and gzip decompression in pure Rust.

- **Fast.** A table-driven decoder in the style of libdeflate's, about as fast
  as libdeflate on large inputs and faster on small ones, and 15-60% faster
  than zlib-rs (see [Performance](#performance)).
- **One-shot or streaming.** Decompress a whole buffer in one call, or feed
  input in pieces of any size to `StreamDecompressor` (or read from any
  `BufRead` with `DecompressReader`) at nearly the same speed.
- **Measuring.** `Decompressor::measure` and `StreamMeasurer` find where a
  stream ends and how long its output is without writing it, for example to
  walk a git pack.
- **Safe.** The decoder is safe Rust; the only `unsafe` is calling into
  code compiled for the CPU features detected at runtime.
- **`no_std`.** The core API works without an allocator.
- **Tested.** Differentially tested (with proptest) and fuzzed against
  libdeflate.

Relies on hardware-accelerated CRC-32 from [crc32fast](https://crates.io/crates/crc32fast)
and Adler-32 from [adler32furious](https://crates.io/crates/adler32furious).

Does not support compression/deflating yet.

## Usage

```toml
[dependencies]
sirocco = "0.1"
```

```rust
use sirocco::Format;

// "hello hello hello" as a raw DEFLATE stream.
let compressed = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00];
// Decompress at most 1 MiB.
let data = sirocco::decompress(Format::Deflate, &compressed, 1 << 20).unwrap();
assert_eq!(data, b"hello hello hello");
```

To read from a file or socket:

```rust,no_run
use std::{fs::File, io::{BufReader, Read}};

fn main() -> std::io::Result<()> {
    let file = BufReader::new(File::open("data.gz")?);
    let mut reader = sirocco::DecompressReader::new(sirocco::Format::Gzip, file);
    let mut data = Vec::new();
    reader.read_to_end(&mut data)?;
    Ok(())
}
```

See the [documentation](https://docs.rs/sirocco) for reusing a
`Decompressor`, decompressing into your own buffer and streaming.

### Features

- `std` (default): `DecompressReader` and other `std::io` integrations;
  enables `alloc`.
- `alloc`: everything that returns a `Vec`, `StreamDecompressor` and
  `StreamMeasurer`.

With `default-features = false`, the checksums,
`Decompressor::decompress_into` and `Decompressor::measure` still work.

The minimum supported Rust version is 1.89.

### Limitations

- zlib streams with a preset dictionary (`FDICT`) are rejected.
- The `Vec` helpers read every member of a multi-member gzip file (as
  `gzip -d` does); `decompress_into` and the streaming types read one member
  at a time.

## Performance

sirocco against libdeflate 1.26 and zlib-rs 0.6, one-shot decompression,
averaged over five code layouts (`scripts/bench.sh`; positive means
sirocco is faster):

| Inputs | Intel Xeon (x86-64) vs libdeflate | vs zlib-rs | Apple M4 vs libdeflate | vs zlib-rs |
| --- | --: | --: | --: | --: |
| Outputs under 256 B | +20% | +74% | +42% | +82% |
| Outputs of 256 B to 4 KiB | +12% | +45% | +17% | +63% |
| Outputs of 1 MiB and more | -2% | +18% | -1% | +42% |
| Real corpora (Silesia, Canterbury, enwik8) | -3% | +15% | +6% | +49% |
| A git pack, entry by entry | +5% | +40% | +14% | +48% |

Streaming is within 4% of one-shot speed and 13-60% faster than zlib-rs's
streaming inflate, depending on the input. libdeflate has no streaming API.
The x86 numbers include sirocco's BMI2 decode loop, picked at runtime.

## Relationship to libdeflate

sirocco began as a Rust port of Eric Biggers'
[libdeflate](https://github.com/ebiggers/libdeflate), and its decoder still
follows libdeflate's design: a word-sized bit buffer refilled without
branches, Huffman tables with subtables built the same way, and a fastloop
that runs while the input and output are far from their ends. We owe
libdeflate a great deal, and it remains the speed to beat.

Much of sirocco is its own work, though:

- **Streaming.** libdeflate decompresses whole buffers only. sirocco's
  `StreamDecompressor`, `DecompressReader` and `StreamMeasurer` take input in
  pieces of any size, resume mid-block, and copy stored blocks straight to
  the caller's output.
- **Measuring** a stream's end and length without writing its output.
- **Small inputs.** A fastloop for the tail of the output, decode tables for
  fixed-Huffman blocks built at compile time, a fast path for zlib headers,
  and table builds that skip unused symbols eight at a time make sirocco
  much faster than libdeflate on outputs of a few KiB and less.
- **Safe Rust.** The decode loops were restructured so that the compiler can
  drop bounds checks, instead of translating libdeflate's pointer arithmetic.
- **Repetitive data.** Long matches in the slow decode loop are copied a span
  at a time rather than byte by byte, which turned a deficit against
  libdeflate on highly repetitive data on Apple M4 into a lead.
- **An idiomatic Rust API**, `no_std` support, and a differential test,
  fuzzing and benchmark suite against libdeflate and zlib-rs.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

sirocco contains code derived from
[libdeflate](https://github.com/ebiggers/libdeflate), which is MIT licensed.
Its copyright notices (Eric Biggers, and Google LLC) are kept in
[LICENSE-MIT](LICENSE-MIT) and apply to the derived code whichever license
you choose.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

## Acknowledgements

Thanks to Eric Biggers and the libdeflate contributors for
[libdeflate](https://github.com/ebiggers/libdeflate), which sirocco's
decoder is based on.
