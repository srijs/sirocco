//! The decompressors the suite times, behind one trait.

use super::cases::Case;
use crate::Format;

/// How a case is decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Mode {
    /// One call per stream with the whole input and an exact-size output.
    OneShot,
    /// A resumable decoder fed [`IN_CHUNK`]-byte pieces of input, writing
    /// [`OUT_CHUNK`]-byte pieces of output, as a `Read` adapter would.
    Stream,
    /// Finding each stream's end and length without its output
    /// (`inflate::Scanner`), fed [`IN_CHUNK`]-byte pieces as gitoxide's pack
    /// indexing does. zlib-rs inflates into a scratch buffer instead.
    Measure,
}

/// Input piece size for [`Mode::Stream`] and [`Mode::Measure`], matching a
/// 32 KiB `BufReader`.
pub const IN_CHUNK: usize = 32 << 10;
/// Output piece size for [`Mode::Stream`].
pub const OUT_CHUNK: usize = 64 << 10;

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::OneShot, Mode::Stream, Mode::Measure];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Mode::OneShot => "oneshot",
            Mode::Stream => "stream",
            Mode::Measure => "measure",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.name() == s)
    }
}

/// A decompressor the suite can time.
pub trait Inflater {
    /// Column name in reports.
    fn name(&self) -> &'static str;

    /// Whether this decompressor has `mode`.
    fn supports(&self, mode: Mode) -> bool;

    /// Decodes every stream of `case` with `mode`, returning the total
    /// decompressed length. With [`Mode::OneShot`] and [`Mode::Stream`],
    /// stream `i` is written to `out[case.streams[i].output]`; `out` is
    /// `case.expected.len()` bytes. [`Mode::Measure`] leaves `out` alone.
    /// Panics on any decode error.
    fn run(&mut self, mode: Mode, case: &Case, out: &mut [u8]) -> usize;
}

/// Implements [`Inflater`] for a sirocco crate under the name `$krate`,
/// as a struct `$ty` reported as `$name`.
///
/// The suite's runner (see `scripts/bench.sh`) uses this on a copy of a base
/// revision whose package is renamed `sirocco_base`, so the base and the
/// working tree are timed in one binary. The base needs the `inflate`
/// module's API; [`sirocco_v01_inflater`] covers bases from before it.
#[macro_export]
macro_rules! sirocco_inflater {
    ($ty:ident, $name:literal, $krate:ident) => {
        /// sirocco as an [`Inflater`]($crate::suite::Inflater).
        #[derive(Debug)]
        pub struct $ty {
            decoders: [Box<$krate::inflate::Decoder>; 3],
            scanners: [Box<$krate::inflate::Scanner>; 3],
            /// [`Mode::Stream`]($crate::suite::Mode::Stream)'s buffer, sized
            /// as `inflate::Reader`'s.
            buf: Vec<u8>,
        }

        impl $ty {
            const FORMATS: [$krate::Format; 3] = [
                $krate::Format::Deflate,
                $krate::Format::Zlib,
                $krate::Format::Gzip,
            ];

            #[must_use]
            pub fn new() -> Self {
                $ty {
                    decoders: Self::FORMATS.map(|f| Box::new($krate::inflate::Decoder::new(f))),
                    scanners: Self::FORMATS.map(|f| Box::new($krate::inflate::Scanner::new(f))),
                    buf: vec![0; 256 << 10],
                }
            }

            fn index(format: $crate::Format) -> usize {
                match format {
                    $crate::Format::Deflate => 0,
                    $crate::Format::Zlib => 1,
                    $crate::Format::Gzip => 2,
                }
            }
        }

        impl Default for $ty {
            fn default() -> Self {
                Self::new()
            }
        }

        impl $crate::suite::Inflater for $ty {
            fn name(&self) -> &'static str {
                $name
            }

            fn supports(&self, _mode: $crate::suite::Mode) -> bool {
                true
            }

            fn run(
                &mut self,
                mode: $crate::suite::Mode,
                case: &$crate::suite::Case,
                out: &mut [u8],
            ) -> usize {
                use $crate::suite::{Mode, IN_CHUNK, OUT_CHUNK};
                let f = Self::index(case.format);
                let mut total = 0;
                match mode {
                    Mode::OneShot => {
                        let d = &mut self.decoders[f];
                        for s in &case.streams {
                            let p = d
                                .decompress(
                                    &case.input[s.input.clone()],
                                    &mut out[s.output.clone()],
                                    true,
                                )
                                .expect("sirocco one-shot");
                            assert!(p.done, "sirocco one-shot");
                            total += p.output.len();
                        }
                    }
                    Mode::Stream => {
                        // Copy out in `OUT_CHUNK` pieces, as `Read` would.
                        let d = &mut self.decoders[f];
                        for s in &case.streams {
                            let input = &case.input[s.input.clone()];
                            let out = &mut out[s.output.clone()];
                            let (mut ip, mut op) = (0, 0);
                            loop {
                                let end = (ip + IN_CHUNK).min(input.len());
                                let p = d
                                    .decompress(&input[ip..end], &mut self.buf, end == input.len())
                                    .expect("sirocco stream");
                                ip += p.consumed;
                                for piece in p.output.chunks(OUT_CHUNK) {
                                    out[op..op + piece.len()].copy_from_slice(piece);
                                    op += piece.len();
                                }
                                if p.done {
                                    break;
                                }
                                if p.full {
                                    d.make_room(&mut self.buf);
                                }
                            }
                            total += op;
                        }
                    }
                    Mode::Measure => {
                        let m = &mut self.scanners[f];
                        for s in &case.streams {
                            let input = &case.input[s.input.clone()];
                            let mut ip = 0;
                            loop {
                                let end = (ip + IN_CHUNK).min(input.len());
                                let done = m
                                    .scan(&input[ip..end], end == input.len())
                                    .expect("sirocco scan");
                                if done.is_some() {
                                    break;
                                }
                                ip = end;
                            }
                            total += m.total_out() as usize;
                        }
                    }
                }
                total
            }
        }
    };
}

/// [`sirocco_inflater`] for bases with sirocco 0.1's API (after PR #29).
/// `scripts/bench.sh` picks it for bases without `src/inflate`.
#[macro_export]
macro_rules! sirocco_v01_inflater {
    ($ty:ident, $name:literal, $krate:ident) => {
        /// sirocco as an [`Inflater`]($crate::suite::Inflater).
        #[derive(Debug)]
        pub struct $ty {
            one: $krate::Decompressor,
            stream: [$krate::StreamDecompressor; 3],
            measure: [$krate::StreamMeasurer; 3],
        }

        impl $ty {
            const FORMATS: [$krate::Format; 3] = [
                $krate::Format::Deflate,
                $krate::Format::Zlib,
                $krate::Format::Gzip,
            ];

            #[must_use]
            pub fn new() -> Self {
                $ty {
                    one: $krate::Decompressor::new($krate::Format::Deflate),
                    stream: Self::FORMATS.map($krate::StreamDecompressor::new),
                    measure: Self::FORMATS.map($krate::StreamMeasurer::new),
                }
            }

            fn index(format: $crate::Format) -> usize {
                match format {
                    $crate::Format::Deflate => 0,
                    $crate::Format::Zlib => 1,
                    $crate::Format::Gzip => 2,
                }
            }
        }

        impl Default for $ty {
            fn default() -> Self {
                Self::new()
            }
        }

        impl $crate::suite::Inflater for $ty {
            fn name(&self) -> &'static str {
                $name
            }

            fn supports(&self, _mode: $crate::suite::Mode) -> bool {
                true
            }

            fn run(
                &mut self,
                mode: $crate::suite::Mode,
                case: &$crate::suite::Case,
                out: &mut [u8],
            ) -> usize {
                use $crate::suite::{Mode, IN_CHUNK, OUT_CHUNK};
                let f = Self::index(case.format);
                let mut total = 0;
                match mode {
                    Mode::OneShot => {
                        self.one.set_format(Self::FORMATS[f]);
                        for s in &case.streams {
                            let r = self
                                .one
                                .decompress_into(
                                    &case.input[s.input.clone()],
                                    &mut out[s.output.clone()],
                                )
                                .expect("sirocco one-shot");
                            total += r.written;
                        }
                    }
                    Mode::Stream => {
                        let d = &mut self.stream[f];
                        for s in &case.streams {
                            d.reset();
                            let input = &case.input[s.input.clone()];
                            let out = &mut out[s.output.clone()];
                            let (mut ip, mut op) = (0, 0);
                            loop {
                                let end = (ip + IN_CHUNK).min(input.len());
                                let oend = (op + OUT_CHUNK).min(out.len());
                                let p = d
                                    .decompress(
                                        &input[ip..end],
                                        &mut out[op..oend],
                                        end == input.len(),
                                    )
                                    .expect("sirocco stream");
                                ip += p.consumed;
                                op += p.written;
                                if p.status == $krate::StreamStatus::Done {
                                    break;
                                }
                            }
                            total += op;
                        }
                    }
                    Mode::Measure => {
                        let m = &mut self.measure[f];
                        for s in &case.streams {
                            m.reset();
                            let input = &case.input[s.input.clone()];
                            let mut ip = 0;
                            loop {
                                let end = (ip + IN_CHUNK).min(input.len());
                                let p = m
                                    .measure(&input[ip..end], end == input.len())
                                    .expect("sirocco measure");
                                ip += p.consumed;
                                if p.done {
                                    break;
                                }
                            }
                            total += m.total_out() as usize;
                        }
                    }
                }
                total
            }
        }
    };
}

crate::sirocco_inflater!(Sirocco, "sirocco", sirocco);

/// libdeflate, which only decodes in one call. It always picks its BMI2 loop
/// at runtime when the CPU has BMI2, whatever the Rust build flags.
pub struct Libdeflate(libdeflater::Decompressor);

impl std::fmt::Debug for Libdeflate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Libdeflate")
    }
}

impl Default for Libdeflate {
    fn default() -> Self {
        Libdeflate(libdeflater::Decompressor::new())
    }
}

impl Inflater for Libdeflate {
    fn name(&self) -> &'static str {
        "libdeflate"
    }

    fn supports(&self, mode: Mode) -> bool {
        mode == Mode::OneShot
    }

    fn run(&mut self, mode: Mode, case: &Case, out: &mut [u8]) -> usize {
        assert_eq!(mode, Mode::OneShot);
        let d = &mut self.0;
        let mut total = 0;
        for s in &case.streams {
            let input = &case.input[s.input.clone()];
            let out = &mut out[s.output.clone()];
            total += match case.format {
                Format::Deflate => d.deflate_decompress(input, out),
                Format::Zlib => d.zlib_decompress(input, out),
                Format::Gzip => d.gzip_decompress(input, out),
            }
            .expect("libdeflate");
        }
        total
    }
}

/// zlib-rs through its `Inflate` API, reset between streams as gitoxide
/// does.
pub struct ZlibRs {
    z: [zlib_rs::Inflate; 3],
    scratch: Vec<u8>,
}

impl std::fmt::Debug for ZlibRs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ZlibRs")
    }
}

impl Default for ZlibRs {
    fn default() -> Self {
        ZlibRs {
            z: [
                zlib_rs::Inflate::new(false, 15),
                zlib_rs::Inflate::new(true, 15),
                zlib_rs::Inflate::new(true, 16 + 15),
            ],
            scratch: vec![0; IN_CHUNK],
        }
    }
}

impl ZlibRs {
    fn reset(&mut self, format: Format) -> &mut zlib_rs::Inflate {
        let (i, header) = match format {
            Format::Deflate => (0, false),
            Format::Zlib => (1, true),
            Format::Gzip => (2, true),
        };
        let z = &mut self.z[i];
        if format == Format::Gzip {
            // `reset` only knows zlib and raw; gzip needs a fresh state,
            // which allocates. The suite only has MB-scale gzip cases.
            *z = zlib_rs::Inflate::new(true, 16 + 15);
        } else {
            z.reset(header);
        }
        z
    }
}

impl Inflater for ZlibRs {
    fn name(&self) -> &'static str {
        "zlib-rs"
    }

    fn supports(&self, _mode: Mode) -> bool {
        true
    }

    fn run(&mut self, mode: Mode, case: &Case, out: &mut [u8]) -> usize {
        use zlib_rs::{InflateFlush, Status};
        let mut total = 0;
        for s in &case.streams {
            let input = &case.input[s.input.clone()];
            match mode {
                Mode::OneShot => {
                    let z = self.reset(case.format);
                    let st = z
                        .decompress(input, &mut out[s.output.clone()], InflateFlush::Finish)
                        .expect("zlib-rs");
                    assert!(matches!(st, Status::StreamEnd), "zlib-rs: {st:?}");
                    total += z.total_out() as usize;
                }
                Mode::Stream | Mode::Measure => {
                    let mut scratch = std::mem::take(&mut self.scratch);
                    let z = self.reset(case.format);
                    let out = &mut out[s.output.clone()];
                    let (mut ip, mut op) = (0, 0);
                    loop {
                        let end = (ip + IN_CHUNK).min(input.len());
                        let (bi, bo) = (z.total_in(), z.total_out());
                        let o: &mut [u8] = if mode == Mode::Stream {
                            let oend = (op + OUT_CHUNK).min(out.len());
                            &mut out[op..oend]
                        } else {
                            &mut scratch
                        };
                        let st = z
                            .decompress(&input[ip..end], o, InflateFlush::NoFlush)
                            .expect("zlib-rs");
                        ip += (z.total_in() - bi) as usize;
                        op += (z.total_out() - bo) as usize;
                        if matches!(st, Status::StreamEnd) {
                            break;
                        }
                    }
                    self.scratch = scratch;
                    total += op;
                }
            }
        }
        total
    }
}
