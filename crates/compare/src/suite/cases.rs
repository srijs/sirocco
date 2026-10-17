//! The inputs the suite times: synthetic data in every size class and block
//! type, the real corpus (whole files and small slices of it), and the zlib
//! streams of real git packs in pack order.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::{Format, Outcome, ReferenceDecompressor};

/// Decompressed size classes, by the length of each stream's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SizeClass {
    /// Under 256 B: most git objects.
    Tiny,
    /// 256 B to 4 KiB.
    Small,
    /// 4 KiB to 64 KiB.
    Medium,
    /// 64 KiB to 1 MiB.
    Large,
    /// 1 MiB and up.
    Huge,
}

impl SizeClass {
    pub const ALL: [SizeClass; 5] = [
        SizeClass::Tiny,
        SizeClass::Small,
        SizeClass::Medium,
        SizeClass::Large,
        SizeClass::Huge,
    ];

    #[must_use]
    pub fn of(len: usize) -> SizeClass {
        match len {
            0..256 => SizeClass::Tiny,
            256..4096 => SizeClass::Small,
            4096..65536 => SizeClass::Medium,
            65536..1_048_576 => SizeClass::Large,
            _ => SizeClass::Huge,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SizeClass::Tiny => "<256B",
            SizeClass::Small => "256B-4K",
            SizeClass::Medium => "4K-64K",
            SizeClass::Large => "64K-1M",
            SizeClass::Huge => ">=1M",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<SizeClass> {
        SizeClass::ALL.into_iter().find(|c| c.label() == s)
    }

    /// Stream lengths the synthetic cases draw from, and how many streams
    /// make one case (enough that one pass is not a handful of calls).
    fn synthetic(self) -> (Range<usize>, usize) {
        match self {
            SizeClass::Tiny => (16..256, 2048),
            SizeClass::Small => (256..4096, 512),
            SizeClass::Medium => (4096..65536, 48),
            SizeClass::Large => (65536..1 << 20, 4),
            SizeClass::Huge => (4 << 20..(4 << 20) + 1, 1),
        }
    }
}

/// Where a case comes from; the report groups by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// Generated data, or slices of the real corpus, in one size class.
    Synthetic,
    /// One whole file of the real corpus.
    File,
    /// Every zlib stream of a git pack, in pack order.
    Pack,
    /// The streams of a git pack in one size class, in pack order.
    PackEntries,
}

impl Kind {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Kind::Synthetic => "synthetic",
            Kind::File => "file",
            Kind::Pack => "pack",
            Kind::PackEntries => "pack-entries",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Kind> {
        [Kind::Synthetic, Kind::File, Kind::Pack, Kind::PackEntries]
            .into_iter()
            .find(|k| k.name() == s)
    }
}

/// One compressed stream of a case: where its input and output are.
#[derive(Clone, Debug)]
pub struct Span {
    pub input: Range<usize>,
    pub output: Range<usize>,
}

/// A set of streams decoded back to back as one timed unit.
#[derive(Debug)]
pub struct Case {
    /// Unique, e.g. `text 4K-64K L6` or `silesia/dickens L6`.
    pub name: String,
    pub kind: Kind,
    /// `None` for a whole pack, whose streams are every size.
    pub size: Option<SizeClass>,
    /// `stored`, `fixed` (fixed Huffman blocks only), `L1`..`L12`
    /// (libdeflate's level) or `git` (what git wrote).
    pub encoding: String,
    /// `text`, `binary`, `random`, `repeat`, `corpus`, `file` or the pack's
    /// name.
    pub source: String,
    pub format: Format,
    /// Every compressed stream, back to back.
    pub input: Vec<u8>,
    pub streams: Vec<Span>,
    /// Every decompressed stream, back to back.
    pub expected: Rc<[u8]>,
}

/// Which compressor writes a synthetic stream.
#[derive(Clone, Copy, Debug)]
enum Enc {
    /// libdeflate level 0: stored blocks.
    Stored,
    /// zlib-rs with `Strategy::Fixed`: fixed Huffman blocks only.
    Fixed,
    /// libdeflate at this level, which picks its own block types (dynamic,
    /// except for short or incompressible input).
    Level(u32),
}

impl Enc {
    fn name(self) -> String {
        match self {
            Enc::Stored => "stored".into(),
            Enc::Fixed => "fixed".into(),
            Enc::Level(l) => format!("L{l}"),
        }
    }

    fn deflate(self, data: &[u8]) -> Vec<u8> {
        match self {
            Enc::Stored => crate::reference_compress(Format::Deflate, 0, data),
            Enc::Level(l) => crate::reference_compress(Format::Deflate, l as i32, data),
            Enc::Fixed => {
                let config = zlib_rs::DeflateConfig {
                    window_bits: -15,
                    strategy: zlib_rs::Strategy::Fixed,
                    ..zlib_rs::DeflateConfig::new(6)
                };
                let mut out = vec![0; zlib_rs::compress_bound(data.len())];
                let (c, rc) = zlib_rs::compress_slice(&mut out, data, config);
                assert_eq!(rc, zlib_rs::ReturnCode::Ok, "zlib-rs compress");
                let n = c.len();
                out.truncate(n);
                out
            }
        }
    }

    fn compress(self, format: Format, data: &[u8]) -> Vec<u8> {
        let raw = self.deflate(data);
        match format {
            Format::Deflate => raw,
            Format::Zlib => {
                let mut out = vec![0x78, 0x9c];
                out.extend(raw);
                out.extend(adler2::adler32_slice(data).to_be_bytes());
                out
            }
            Format::Gzip => {
                let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
                out.extend(raw);
                out.extend(libdeflater::crc32(data).to_le_bytes());
                out.extend((data.len() as u32).to_le_bytes());
                out
            }
        }
    }
}

/// xorshift64, so every run builds the same cases.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn range(&mut self, r: Range<usize>) -> usize {
        r.start + self.below(r.end - r.start)
    }
}

const SOURCE_LEN: usize = 8 << 20;

/// English-like text: a Zipf-ish vocabulary of real and made-up words, with
/// capitals, punctuation, numbers and line breaks.
fn text(rng: &mut Rng) -> Vec<u8> {
    const SYL: &[&str] = &[
        "ka", "ro", "mi", "sen", "tal", "ve", "dor", "lin", "qua", "ph", "est", "ion", "ra", "nu",
        "bel", "ix", "or", "an", "th", "ur",
    ];
    let mut vocab: Vec<String> = crate::WORDS.iter().map(|w| (*w).to_owned()).collect();
    while vocab.len() < 4000 {
        let n = 1 + rng.below(4);
        vocab.push((0..n).map(|_| SYL[rng.below(SYL.len())]).collect());
    }
    let mut out = Vec::with_capacity(SOURCE_LEN + 64);
    let mut capital = true;
    while out.len() < SOURCE_LEN {
        // Zipf-ish: small indexes much more often.
        let r = rng.below(vocab.len());
        let w = &vocab[rng.below(r + 1)];
        if capital {
            out.push(w.as_bytes()[0].to_ascii_uppercase());
            out.extend_from_slice(&w.as_bytes()[1..]);
        } else {
            out.extend_from_slice(w.as_bytes());
        }
        capital = false;
        match rng.below(40) {
            0 => out.extend_from_slice(format!(" {}", rng.below(100_000)).as_bytes()),
            1..=3 => out.push(b','),
            4..=5 => {
                out.push(b'.');
                capital = true;
            }
            _ => {}
        }
        out.push(if rng.below(14) == 0 { b'\n' } else { b' ' });
    }
    out.truncate(SOURCE_LEN);
    out
}

/// Table-like binary records: counters, timestamps, small enums, noisy
/// fields and repeated tags, so matches have many offsets and lengths.
fn binary(rng: &mut Rng) -> Vec<u8> {
    const TAGS: &[&[u8; 8]] = &[b"ACTIVE\0\0", b"PENDING\0", b"DONE\0\0\0\0", b"FAILED\0\0"];
    let mut out = Vec::with_capacity(SOURCE_LEN + 64);
    let (mut id, mut ts) = (1000u32, 1_700_000_000u32);
    while out.len() < SOURCE_LEN {
        id += 1;
        ts += rng.below(30) as u32;
        out.extend(id.to_le_bytes());
        out.extend(ts.to_le_bytes());
        out.extend((rng.below(8) as u16).to_le_bytes());
        out.extend(((rng.next() & 0xffff) as u16 | 0x4000).to_le_bytes());
        out.extend((rng.next() as u32 & 0x00ff_00ff).to_le_bytes());
        out.extend_from_slice(TAGS[rng.below(TAGS.len())]);
        out.extend((f64::from(rng.below(10_000) as u32) / 100.0).to_le_bytes());
    }
    out.truncate(SOURCE_LEN);
    out
}

/// Runs of short repeating patterns: offsets 1 to 200, which take the
/// overlapping-copy paths.
fn repeat(rng: &mut Rng) -> Vec<u8> {
    const PERIODS: &[usize] = &[1, 2, 3, 4, 7, 8, 13, 40, 200];
    let mut out = Vec::with_capacity(SOURCE_LEN + 8192);
    while out.len() < SOURCE_LEN {
        let p = PERIODS[rng.below(PERIODS.len())];
        let pat: Vec<u8> = (0..p).map(|_| rng.next() as u8).collect();
        let n = rng.range(16..4096);
        out.extend((0..n).map(|i| pat[i % p]));
    }
    out.truncate(SOURCE_LEN);
    out
}

/// Builds one case of `count` streams cut from `source` at random places.
fn sliced(
    rng: &mut Rng,
    source_name: &str,
    source: &[u8],
    class: SizeClass,
    enc: Enc,
    format: Format,
) -> Case {
    let (lens, count) = class.synthetic();
    let mut input = Vec::new();
    let mut expected = Vec::new();
    let mut streams = Vec::with_capacity(count);
    for _ in 0..count {
        let len = rng.range(lens.clone()).min(source.len());
        let at = rng.below(source.len() - len + 1);
        let data = &source[at..at + len];
        let c = enc.compress(format, data);
        streams.push(Span {
            input: input.len()..input.len() + c.len(),
            output: expected.len()..expected.len() + len,
        });
        input.extend(c);
        expected.extend_from_slice(data);
    }
    let fmt = match format {
        Format::Deflate => String::new(),
        Format::Zlib => " zlib".into(),
        Format::Gzip => " gzip".into(),
    };
    Case {
        name: format!("{source_name} {} {}{fmt}", class.label(), enc.name()),
        kind: Kind::Synthetic,
        size: Some(class),
        encoding: enc.name(),
        source: source_name.into(),
        format,
        input,
        streams,
        expected: expected.into(),
    }
}

/// The synthetic cases in `classes`, plus slices of `corpus` (the real
/// corpus run together) when there is one. Raw DEFLATE unless the name says
/// otherwise.
#[must_use]
pub fn synthetic(corpus: Option<&[u8]>, classes: &[SizeClass]) -> Vec<Case> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let text = text(&mut rng);
    let binary = binary(&mut rng);
    let random: Vec<u8> = (0..SOURCE_LEN).map(|_| rng.next() as u8).collect();
    let repeat = repeat(&mut rng);

    let mut out = Vec::new();
    for &class in classes {
        let mut add = |name: &str, src: &[u8], enc: Enc, format: Format| {
            out.push(sliced(&mut rng, name, src, class, enc, format));
        };
        for enc in [
            Enc::Stored,
            Enc::Fixed,
            Enc::Level(1),
            Enc::Level(6),
            Enc::Level(9),
        ] {
            add("text", &text, enc, Format::Deflate);
        }
        for enc in [Enc::Fixed, Enc::Level(1), Enc::Level(6)] {
            add("binary", &binary, enc, Format::Deflate);
        }
        add("random", &random, Enc::Level(6), Format::Deflate);
        add("repeat", &repeat, Enc::Level(6), Format::Deflate);
        // The zlib and gzip wrappers, where git (zlib, small) and gzip
        // files (large) meet them.
        match class {
            SizeClass::Tiny | SizeClass::Small => {
                add("text", &text, Enc::Level(6), Format::Zlib);
            }
            SizeClass::Huge => add("text", &text, Enc::Level(6), Format::Gzip),
            _ => {}
        }
        if let Some(c) = corpus.filter(|_| class != SizeClass::Huge) {
            add("corpus", c, Enc::Level(6), Format::Deflate);
        }
    }
    out
}

/// Each real-corpus file whole, at each of `levels` (raw DEFLATE).
#[must_use]
pub fn files(corpus: &[crate::Sample], levels: &[u32]) -> Vec<Case> {
    let mut out = Vec::new();
    for s in corpus {
        let data: Rc<[u8]> = s.data.as_slice().into();
        for &level in levels {
            let c = Enc::Level(level).compress(Format::Deflate, &data);
            out.push(Case {
                name: format!("{} L{level}", s.name),
                kind: Kind::File,
                size: Some(SizeClass::of(data.len())),
                encoding: format!("L{level}"),
                source: "file".into(),
                format: Format::Deflate,
                streams: vec![Span {
                    input: 0..c.len(),
                    output: 0..data.len(),
                }],
                input: c,
                expected: data.clone(),
            });
        }
    }
    out
}

/// `.pack` files at `path`: the file itself, or every one under a directory.
#[must_use]
pub fn find_packs(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_owned()];
    }
    let mut out: Vec<PathBuf> = std::fs::read_dir(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "pack"))
        .collect();
    out.sort();
    out
}

/// The repository a pack belongs to (`gitoxide` for
/// `gitoxide.git/objects/pack/pack-1234.pack`), else the file's name.
fn pack_name(path: &Path) -> String {
    for dir in path.ancestors().skip(1) {
        let Some(name) = dir.file_name().map(|n| n.to_string_lossy()) else {
            break;
        };
        if name == ".git" {
            if let Some(repo) = dir.parent().and_then(Path::file_name) {
                return repo.to_string_lossy().into_owned();
            }
        } else if let Some(repo) = name.strip_suffix(".git") {
            return repo.to_owned();
        }
    }
    path.file_stem().unwrap().to_string_lossy().into_owned()
}

/// A git pack's zlib streams in pack order (at most `max` of them), as one
/// case for the whole pack and one per size class.
///
/// Each stream's end is found with libdeflate, so a broken sirocco cannot
/// skew the cases.
#[must_use]
pub fn pack(path: &Path, max: usize) -> Vec<Case> {
    let pack = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(&pack[..4], b"PACK", "{} is not a git pack", path.display());
    let count = u32::from_be_bytes(pack[8..12].try_into().unwrap()) as usize;
    let name = pack_name(path);

    let mut reference = ReferenceDecompressor::new();
    // (input range in `pack`, decompressed bytes)
    let mut entries: Vec<(Range<usize>, Vec<u8>)> = Vec::new();
    let mut pos = 12;
    for _ in 0..count.min(max) {
        let mut c = pack[pos];
        pos += 1;
        let kind = (c >> 4) & 7;
        let mut size = usize::from(c & 15);
        let mut shift = 4;
        while c & 0x80 != 0 {
            c = pack[pos];
            pos += 1;
            size |= usize::from(c & 0x7f) << shift;
            shift += 7;
        }
        match kind {
            // OFS_DELTA: a varint offset to the base.
            6 => loop {
                let c = pack[pos];
                pos += 1;
                if c & 0x80 == 0 {
                    break;
                }
            },
            // REF_DELTA: the base's SHA-1.
            7 => pos += 20,
            _ => {}
        }
        match reference.decompress(Format::Zlib, &pack[pos..], size) {
            Outcome::Ok(consumed, data) => {
                assert_eq!(data.len(), size, "pack entry size");
                entries.push((pos..pos + consumed, data));
                pos += consumed;
            }
            o => panic!("{}: entry at {pos}: {o:?}", path.display()),
        }
    }

    let build = |sel: &[&(Range<usize>, Vec<u8>)], size: Option<SizeClass>| {
        let mut input = Vec::new();
        let mut expected = Vec::new();
        let mut streams = Vec::with_capacity(sel.len());
        for (r, data) in sel {
            streams.push(Span {
                input: input.len()..input.len() + r.len(),
                output: expected.len()..expected.len() + data.len(),
            });
            input.extend_from_slice(&pack[r.clone()]);
            expected.extend_from_slice(data);
        }
        Case {
            name: match size {
                None => format!("pack {name}"),
                Some(c) => format!("pack {name} {}", c.label()),
            },
            kind: if size.is_none() {
                Kind::Pack
            } else {
                Kind::PackEntries
            },
            size,
            encoding: "git".into(),
            source: name.clone(),
            format: Format::Zlib,
            input,
            streams,
            expected: expected.into(),
        }
    };
    let all: Vec<_> = entries.iter().collect();
    let mut out = vec![build(&all, None)];
    for class in SizeClass::ALL {
        let sel: Vec<_> = entries
            .iter()
            .filter(|e| SizeClass::of(e.1.len()) == class)
            .collect();
        if !sel.is_empty() {
            out.push(build(&sel, Some(class)));
        }
    }
    out
}
