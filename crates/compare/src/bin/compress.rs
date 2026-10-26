//! Compressed size and speed of sirocco against zlib, zlib-rs, libdeflate
//! and miniz_oxide, level by level. See `crates/compare/BENCH.md`.
//!
//! ```sh
//! cargo run -p compare --release --bin compress -- --help
//! ```

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_precision_loss
)]

use std::time::Instant;

use compare::Format;
use sirocco::deflate::{Encoder, Flush, Level};

const USAGE: &str = "\
usage: compress [OPTIONS] [DIR...]

Compresses every file under each DIR (default: each set in the corpus
directory, and the synthetic corpus) as one group, at each level, with each
compressor, and prints sizes and speeds against zlib's.

options:
  --levels LIST   levels to run (default 1,2,3,4,5,6,7,8,9)
  --impls LIST    compressors (default sirocco,zlib,zlib-rs,libdeflate,miniz_oxide;
                  also zlib-ng, built with --features zlib-ng)
  --rounds N      timing rounds, interleaved; the best counts (default 5)
  --chunk N       compress the files cut into N-byte pieces instead
  --check         decompress every output with zlib
  --max-size-vs-zlib PCT
                  fail if sirocco's output is more than PCT% larger than
                  zlib's at any level, for any group (needs zlib in --impls)";

/// A compressor that can be timed: state is reused between calls.
trait Compressor {
    fn name(&self) -> &'static str;
    /// Compresses `data` as raw DEFLATE at `level` into `out` (which is
    /// large enough), returning the size.
    fn compress(&mut self, level: u8, data: &[u8], out: &mut [u8]) -> usize;
}

struct Sirocco(Vec<Encoder>);

impl Compressor for Sirocco {
    fn name(&self) -> &'static str {
        "sirocco"
    }
    fn compress(&mut self, level: u8, data: &[u8], out: &mut [u8]) -> usize {
        let p = self.0[level as usize].compress(data, out, Flush::Finish);
        assert!(p.done);
        p.output.len()
    }
}

struct Zlib(Vec<flate2::Compress>);

impl Compressor for Zlib {
    fn name(&self) -> &'static str {
        "zlib"
    }
    fn compress(&mut self, level: u8, data: &[u8], out: &mut [u8]) -> usize {
        let c = &mut self.0[level as usize];
        c.reset();
        let st = c
            .compress(data, out, flate2::FlushCompress::Finish)
            .unwrap();
        assert_eq!(st, flate2::Status::StreamEnd);
        c.total_out() as usize
    }
}

struct ZlibRs;

impl Compressor for ZlibRs {
    fn name(&self) -> &'static str {
        "zlib-rs"
    }
    fn compress(&mut self, level: u8, data: &[u8], out: &mut [u8]) -> usize {
        let config = zlib_rs::DeflateConfig {
            window_bits: -15,
            ..zlib_rs::DeflateConfig::new(level.into())
        };
        let (c, rc) = zlib_rs::compress_slice(out, data, config);
        assert_eq!(rc, zlib_rs::ReturnCode::Ok);
        c.len()
    }
}

struct Libdeflate(Vec<libdeflater::Compressor>);

impl Compressor for Libdeflate {
    fn name(&self) -> &'static str {
        "libdeflate"
    }
    fn compress(&mut self, level: u8, data: &[u8], out: &mut [u8]) -> usize {
        self.0[level as usize].deflate_compress(data, out).unwrap()
    }
}

struct MinizOxide(Vec<Box<miniz_oxide::deflate::core::CompressorOxide>>);

impl Compressor for MinizOxide {
    fn name(&self) -> &'static str {
        "miniz_oxide"
    }
    fn compress(&mut self, level: u8, data: &[u8], out: &mut [u8]) -> usize {
        use miniz_oxide::deflate::core::{compress, TDEFLFlush, TDEFLStatus};
        let c = &mut self.0[level as usize];
        c.reset();
        let (st, _, n) = compress(c, data, out, TDEFLFlush::Finish);
        assert_eq!(st, TDEFLStatus::Done);
        n
    }
}

/// zlib-ng through its native (`zng_`) API, one stream per level.
#[cfg(feature = "zlib-ng")]
struct ZlibNg(Vec<Box<libz_ng_sys::z_stream>>);

#[cfg(feature = "zlib-ng")]
impl ZlibNg {
    fn new() -> Self {
        use libz_ng_sys as z;
        extern "C" fn zalloc(_: z::voidpf, n: z::uInt, size: z::uInt) -> z::voidpf {
            extern "C" {
                fn calloc(n: usize, size: usize) -> z::voidpf;
            }
            // SAFETY: calloc has no preconditions.
            unsafe { calloc(n as usize, size as usize) }
        }
        extern "C" fn zfree(_: z::voidpf, p: z::voidpf) {
            extern "C" {
                fn free(p: z::voidpf);
            }
            // SAFETY: zlib-ng frees only what zalloc returned.
            unsafe { free(p) }
        }
        Self(
            (0..=9)
                .map(|level| {
                    let mut s = Box::new(z::z_stream {
                        next_in: std::ptr::null_mut(),
                        avail_in: 0,
                        total_in: 0,
                        next_out: std::ptr::null_mut(),
                        avail_out: 0,
                        total_out: 0,
                        msg: std::ptr::null_mut(),
                        state: std::ptr::null_mut(),
                        zalloc,
                        zfree,
                        opaque: std::ptr::null_mut(),
                        data_type: 0,
                        adler: 0,
                        reserved: 0,
                    });
                    // SAFETY: `s` is initialized and boxed, so it stays put.
                    let rc = unsafe {
                        z::zng_deflateInit2(
                            &mut *s,
                            level,
                            z::Z_DEFLATED,
                            -15,
                            8,
                            z::Z_DEFAULT_STRATEGY,
                        )
                    };
                    assert_eq!(rc, z::Z_OK);
                    s
                })
                .collect(),
        )
    }
}

#[cfg(feature = "zlib-ng")]
impl Compressor for ZlibNg {
    fn name(&self) -> &'static str {
        "zlib-ng"
    }
    fn compress(&mut self, level: u8, data: &[u8], out: &mut [u8]) -> usize {
        use libz_ng_sys as z;
        let s = &mut *self.0[level as usize];
        s.next_in = data.as_ptr().cast_mut();
        s.avail_in = data.len().try_into().unwrap();
        s.next_out = out.as_mut_ptr();
        s.avail_out = out.len().try_into().unwrap();
        // SAFETY: the stream was initialized in `new`, and the buffers
        // outlive the calls.
        unsafe {
            assert_eq!(z::deflate(s, z::Z_FINISH), z::Z_STREAM_END);
            let n = s.total_out as usize;
            assert_eq!(z::deflateReset(s), z::Z_OK);
            n
        }
    }
}

fn make(name: &str) -> Box<dyn Compressor> {
    match name {
        "sirocco" => Box::new(Sirocco(
            (0..=9)
                .map(|l| Encoder::new(sirocco::Format::Deflate, Level::new(l)))
                .collect(),
        )),
        "zlib" => Box::new(Zlib(
            (0..=9)
                .map(|l| flate2::Compress::new(flate2::Compression::new(l), false))
                .collect(),
        )),
        "zlib-rs" => Box::new(ZlibRs),
        #[cfg(feature = "zlib-ng")]
        "zlib-ng" => Box::new(ZlibNg::new()),
        "libdeflate" => Box::new(Libdeflate(
            (0..=12)
                .map(|l| libdeflater::Compressor::new(libdeflater::CompressionLvl::new(l).unwrap()))
                .collect(),
        )),
        "miniz_oxide" => Box::new(MinizOxide(
            (0..=9)
                .map(|l| {
                    let flags =
                        miniz_oxide::deflate::core::create_comp_flags_from_zip_params(l, -15, 0);
                    Box::new(miniz_oxide::deflate::core::CompressorOxide::new(flags))
                })
                .collect(),
        )),
        _ => panic!("unknown compressor {name}\n\n{USAGE}"),
    }
}

/// A named set of inputs.
struct Group {
    name: String,
    data: Vec<Vec<u8>>,
}

fn read_dir(dir: &std::path::Path, out: &mut Vec<Vec<u8>>) {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    paths.sort();
    for p in paths {
        if p.file_name().unwrap().to_string_lossy().starts_with('.') {
            continue;
        }
        if p.is_dir() {
            read_dir(&p, out);
        } else {
            out.push(std::fs::read(&p).unwrap());
        }
    }
}

fn groups(dirs: &[String]) -> Vec<Group> {
    let mut groups = Vec::new();
    let mut add = |name: String, dir: &std::path::Path| {
        let mut data = Vec::new();
        read_dir(dir, &mut data);
        groups.push(Group { name, data });
    };
    if dirs.is_empty() {
        let root = std::env::var_os("SIROCCO_CORPUS_DIR").map_or_else(
            || std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus"),
            Into::into,
        );
        if let Ok(rd) = std::fs::read_dir(&root) {
            let mut sets: Vec<_> = rd
                .map(|e| e.unwrap().path())
                .filter(|p| p.is_dir())
                .collect();
            sets.sort();
            for s in sets {
                add(s.file_name().unwrap().to_string_lossy().into(), &s);
            }
        }
        groups.push(Group {
            name: "synthetic".into(),
            data: compare::synthetic_corpus()
                .into_iter()
                .map(|s| s.data)
                .collect(),
        });
    } else {
        for d in dirs {
            add(d.clone(), std::path::Path::new(d));
        }
    }
    groups
}

fn main() {
    let mut levels: Vec<u8> = (1..=9).collect();
    let mut impls = ["sirocco", "zlib", "zlib-rs", "libdeflate", "miniz_oxide"].join(",");
    let (mut rounds, mut chunk, mut check, mut dirs) = (5, None, false, Vec::new());
    let mut max_vs_zlib: Option<f64> = None;
    let mut over = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || {
            args.next()
                .unwrap_or_else(|| panic!("{a} needs a value\n\n{USAGE}"))
        };
        match a.as_str() {
            "--levels" => levels = val().split(',').map(|l| l.parse().unwrap()).collect(),
            "--impls" => impls = val(),
            "--rounds" => rounds = val().parse().unwrap(),
            "--chunk" => chunk = Some(val().parse::<usize>().unwrap()),
            "--check" => check = true,
            "--max-size-vs-zlib" => max_vs_zlib = Some(val().parse().unwrap()),
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            d if !d.starts_with('-') => dirs.push(d.to_string()),
            _ => panic!("unknown option {a}\n\n{USAGE}"),
        }
    }
    let mut imps: Vec<Box<dyn Compressor>> = impls.split(',').map(make).collect();
    for mut g in groups(&dirs) {
        if let Some(n) = chunk {
            g.data = g.data.concat().chunks(n).map(<[u8]>::to_vec).collect();
            g.name = format!("{} in {n}-byte pieces", g.name);
        }
        let total: usize = g.data.iter().map(Vec::len).sum();
        if total == 0 {
            continue;
        }
        let max = g.data.iter().map(Vec::len).max().unwrap();
        let mut out = vec![0u8; max + max / 8 + 4096];
        println!(
            "\n### {} ({} files, {:.1} MB)\n",
            g.name,
            g.data.len(),
            total as f64 / 1e6
        );
        println!("| level | compressor | size | MB/s | size vs zlib | speed vs zlib |");
        println!("| --: | --- | --: | --: | --: | --: |");
        for &level in &levels {
            // An untimed pass: sizes, checks, and warm caches and pages.
            let sizes: Vec<usize> = imps
                .iter_mut()
                .map(|imp| {
                    g.data
                        .iter()
                        .map(|d| {
                            let n = imp.compress(level, d, &mut out);
                            if check {
                                let back = compare::deflate::zlib_decompress(
                                    Format::Deflate,
                                    &out[..n],
                                    d.len(),
                                );
                                assert!(
                                    back.is_ok_and(|b| b == *d),
                                    "{} level {level}",
                                    imp.name()
                                );
                            }
                            n
                        })
                        .sum()
                })
                .collect();
            let mut best = vec![f64::MAX; imps.len()];
            for r in 0..rounds {
                for k in 0..imps.len() {
                    let i = (k + r) % imps.len();
                    let t = Instant::now();
                    for d in &g.data {
                        std::hint::black_box(imps[i].compress(
                            level,
                            std::hint::black_box(d),
                            &mut out,
                        ));
                    }
                    best[i] = best[i].min(t.elapsed().as_secs_f64());
                }
            }
            let z = imps.iter().position(|i| i.name() == "zlib");
            let si = imps.iter().position(|i| i.name() == "sirocco");
            if let (Some(max), Some(z), Some(si)) = (max_vs_zlib, z, si) {
                let pct = 100.0 * (sizes[si] as f64 / sizes[z] as f64 - 1.0);
                if pct > max {
                    over.push(format!("{} level {level}: {pct:+.2}% vs zlib", g.name));
                }
            }
            for (i, imp) in imps.iter().enumerate() {
                let speed = total as f64 / best[i] / 1e6;
                let (vs_size, vs_speed) = match z {
                    Some(z) if z != i => (
                        format!("{:+.2}%", 100.0 * (sizes[i] as f64 / sizes[z] as f64 - 1.0)),
                        format!("{:.2}x", best[z] / best[i]),
                    ),
                    _ => (String::new(), String::new()),
                };
                println!(
                    "| {level} | {} | {:.3}% | {speed:.1} | {vs_size} | {vs_speed} |",
                    imp.name(),
                    100.0 * sizes[i] as f64 / total as f64
                );
            }
        }
    }
    if !over.is_empty() {
        eprintln!("\nover --max-size-vs-zlib:\n  {}", over.join("\n  "));
        std::process::exit(1);
    }
}
