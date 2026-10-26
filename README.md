# sirocco

[![crates.io](https://img.shields.io/crates/v/sirocco.svg)](https://crates.io/crates/sirocco)
[![docs.rs](https://img.shields.io/docsrs/sirocco)](https://docs.rs/sirocco)

Fast DEFLATE, zlib and gzip compression and decompression in pure Rust.

- **Compression at zlib's levels, much faster.** Each level from 1 to 9
  compresses about as well as zlib's same level (a little better, mostly)
  at 2 to 6 times its speed on the Silesia corpus (see
  [Compression performance](#compression-performance)).
- **Fast decompression.** A table-driven decoder in the style of
  libdeflate's, about as fast as libdeflate on large inputs and faster on
  small ones, and 15-60% faster than zlib-rs (see [Decompression
  performance](#decompression-performance)).
- **One-shot or streaming, with one API.** `inflate::Decoder` decompresses
  a whole buffer in one call, or input in pieces of any size into a buffer
  you own, at nearly the same speed; `deflate::Encoder` likewise
  compresses from a buffer you own, with zlib's sync, full and finish
  flushes. `inflate::Reader` reads from any `BufRead`, and
  `deflate::Writer` writes to any `Write`.
- **Scanning.** `inflate::Scanner` finds where a stream ends and how long
  its output is without writing it, for example to walk a git pack.
- **Safe.** The encoder is safe Rust, and so is the decoder but for calling
  into code compiled for the CPU features detected at runtime.
- **`no_std`.** Encoding and decoding, streaming included, work without an
  allocator.
- **Tested.** Differentially tested (with proptest) and fuzzed: the
  decoder against zlib-rs, and every stream the encoder writes, flushes
  included, against zlib itself, zlib-rs, libdeflate and miniz_oxide.

Relies on hardware-accelerated CRC-32 from [crc32fast](https://crates.io/crates/crc32fast)
and Adler-32 from [adler32furious](https://crates.io/crates/adler32furious).

## Usage

```toml
[dependencies]
sirocco = "0.3"
```

```rust
use sirocco::{deflate::Level, Format};

let compressed = sirocco::compress(Format::Gzip, Level::DEFAULT, b"hello hello hello");
let data = sirocco::decompress(Format::Gzip, &compressed, 1 << 20).unwrap();
assert_eq!(data, b"hello hello hello");
```

```rust
use sirocco::Format;

// "hello hello hello" as a raw DEFLATE stream.
let compressed = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00];
// Decompress at most 1 MiB.
let data = sirocco::decompress(Format::Deflate, &compressed, 1 << 20).unwrap();
assert_eq!(data, b"hello hello hello");
```

To compress into a file or socket, and read back:

```rust,no_run
use std::{fs::File, io::{BufReader, Read, Write}};
use sirocco::{deflate::{Level, Writer}, Format};

fn main() -> std::io::Result<()> {
    let mut w = Writer::new(Format::Gzip, Level::DEFAULT, File::create("data.gz")?);
    w.write_all(b"hello hello hello")?;
    w.finish()?;

    let file = BufReader::new(File::open("data.gz")?);
    let mut reader = sirocco::inflate::Reader::new(sirocco::Format::Gzip, file);
    let mut data = Vec::new();
    reader.read_to_end(&mut data)?;
    Ok(())
}
```

To decompress many buffers, reuse an `inflate::Decoder`. It writes into a
buffer you own, takes input in one piece or many, and never allocates:

```rust
use sirocco::{inflate::Decoder, Format};

# let compressed = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00];
let mut d = Decoder::new(Format::Deflate);
let mut out = [0; 17];
let p = d.decompress(&compressed, &mut out, true).unwrap();
assert!(p.done);
assert_eq!(p.output, b"hello hello hello");
```

`deflate::Encoder` is the same for compression: it reads from a buffer you
own, writes into another, and allocates only its tables, which you can give
it instead.

See the [documentation](https://docs.rs/sirocco) for streaming through
buffers smaller than the data.

### Features

- `std` (default): `deflate::Writer`, `inflate::Reader` and other `std::io`
  integrations; enables `alloc`.
- `alloc`: `sirocco::compress` and `sirocco::decompress`, which return a
  `Vec`; and `deflate::Encoder::new`, which allocates the encoder's tables.

With `default-features = false`, `deflate::Encoder` (with tables you give
it, a `static` array say), `inflate::Decoder` and `inflate::Scanner` still
work, streaming included.

The minimum supported Rust version is 1.89.

### Limitations

- The encoder has zlib's levels 0 to 9 and flushes (`Z_NO_FLUSH`,
  `Z_SYNC_FLUSH`, `Z_FULL_FLUSH`, `Z_FINISH`), but not zlib's strategies
  (`Z_FILTERED`, `Z_HUFFMAN_ONLY`, `Z_RLE`, `Z_FIXED`), preset dictionaries,
  or gzip header fields (name, comment, time).
- zlib streams with a preset dictionary (`FDICT`) are rejected, with
  `inflate::Error::PresetDictionary`.
- `sirocco::decompress` reads every member of a multi-member gzip file (as
  `gzip -d` does); `inflate::Decoder`, `Scanner` and `Reader` read one member
  at a time.

### Strictness

While libdeflate tends to be more lenient, sirocco aims for full zlib
compatibility, both in terms of how it validates and rejects, as well as which
errors take preference. There are differential fuzz tests to cover this. Any
divergence should be considered a bug.

## Decompression performance

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

## Compression performance

sirocco against zlib 1.3.2, zlib-rs 0.6 and libdeflate 1.26 on an Apple
M4, compressing each file of the Silesia corpus in one call
(`cargo run -p compare --release --bin compress`, best of 5 rounds; see
[BENCH.md](crates/compare/BENCH.md)). Sizes are relative to zlib's at the
same level (negative is smaller), speeds are multiples of zlib's:

| Level | sirocco MB/s | sirocco size | speed | zlib-rs size | speed | libdeflate size | speed |
| --: | --: | --: | --: | --: | --: | --: | --: |
| 1 | 447 | -0.5% | 2.5x | +21.9% | 1.8x | -4.8% | 2.0x |
| 2 | 350 | -0.5% | 2.2x | +0.4% | 1.3x | -5.2% | 1.7x |
| 3 | 270 | -1.7% | 2.2x | -1.6% | 1.5x | -3.8% | 2.0x |
| 4 | 252 | -0.3% | 2.4x | -0.9% | 1.5x | -2.1% | 2.1x |
| 5 | 219 | -0.0% | 3.0x | +0.1% | 2.0x | -1.4% | 2.4x |
| 6 | 182 | +0.0% | 3.5x | +1.0% | 2.3x | -1.0% | 2.8x |
| 7 | 163 | -0.1% | 3.8x | +0.6% | 1.9x | -1.2% | 2.4x |
| 8 | 148 | -0.2% | 5.3x | +0.7% | 1.8x | -1.4% | 2.3x |
| 9 | 133 | -0.3% | 6.4x | -0.1% | 2.6x | -1.4% | 2.5x |

On other inputs, sirocco's size and speed against zlib's at levels 1, 6 and 9:

| Inputs | Level 1 | Level 6 | Level 9 |
| --- | --: | --: | --: |
| Canterbury, each file | -4.3%, 2.1x | -0.6%, 3.8x | -0.9%, 17.6x |
| Server logs (`scripts/fetch-corpus.sh logs`), 39 MB | -2.0%, 2.0x | -1.9%, 2.5x | -0.8%, 5.3x |
| Silesia in 4 KiB pieces, each compressed alone | +2.6%, 3.1x | -0.3%, 2.6x | -0.3%, 2.9x |
| git.git objects (a 10% sample), each compressed alone | -0.5%, 2.3x | +0.4%, 2.8x | +0.0%, 3.4x |

Level 1 skips ahead through data where it finds no matches, which on small
binary inputs (the 4 KiB Silesia pieces) costs a few percent against zlib;
on real small inputs such as git objects it still compresses better. At
levels 6 and 9, git objects come out up to 0.4% larger than zlib's.

libdeflate's levels compress more than zlib's, so its rows are not like for
like. At the same settings, sirocco's lazy parsing is a little faster than
libdeflate's (sirocco's level 6 searches as deep as libdeflate's level 5,
and runs 3% faster for 0.1% more output, from its smaller blocks), while
at the fast end, with libdeflate's level 1 settings, sirocco is 8% slower. Data built from a few short words in random order, where deep
searches for short matches pay, is a weak spot sirocco shares with
libdeflate: zlib's 3-byte hash chains do better there.

## Relationship to libdeflate

sirocco began as a Rust port of Eric Biggers'
[libdeflate](https://github.com/ebiggers/libdeflate), and its decoder still
follows libdeflate's design: a word-sized bit buffer refilled without
branches, Huffman tables with subtables built the same way, and a fastloop
that runs while the input and output are far from their ends. We owe
libdeflate a great deal, and it remains the speed to beat.

Its encoder is a port of libdeflate's compressor: hash chains (or a hash
table, at the fastest levels) searched greedily or lazily, length-limited
Huffman codes built as libdeflate builds them (after 7-Zip), blocks ended
where the statistics of the data change, and each block written as
whichever of a dynamic, static or stored block is smallest, by exact count.

Much of sirocco is its own work, though:

- **Streaming.** libdeflate decompresses whole buffers only. sirocco's
  `inflate::Decoder`, `Reader` and `Scanner` take input in pieces of any
  size and resume mid-block, mid-match included, decoding straight into the
  caller's buffer.
- **Scanning** a stream's end and length without writing its output.
- **Small inputs.** A fastloop for the tail of the output, decode tables for
  fixed-Huffman blocks built at compile time, a fast path for zlib headers,
  and table builds that skip unused symbols eight at a time make sirocco
  much faster than libdeflate on outputs of a few KiB and less.
- **Safe Rust.** The decode loops were restructured so that the compiler can
  drop bounds checks, instead of translating libdeflate's pointer arithmetic.
- **Repetitive data.** Long matches in the slow decode loop are copied a span
  at a time rather than byte by byte, which turned a deficit against
  libdeflate on highly repetitive data on Apple M4 into a lead.
- **Streaming compression.** libdeflate compresses whole buffers only.
  sirocco's `deflate::Encoder` takes input in pieces, writes output into
  buffers of any size (resuming mid-block), and has zlib's sync and full
  flushes, with tables you can provide yourself.
- **zlib's levels.** Each level is tuned to compress at least as well as
  zlib's same level in as little time as can be, with a few ideas from
  elsewhere at the fast end: one-entry hash buckets, zlib's limit on
  inserting the positions inside long matches, and lz4-style skipping
  through incompressible data. Small inputs given whole get smaller hash
  tables, so that clearing them doesn't cost more than compressing.
- **An idiomatic Rust API**, `no_std` support, and a differential test,
  fuzzing and benchmark suite against zlib, libdeflate, zlib-rs and
  miniz_oxide.

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
