//! The benchmark suite every speed change is measured with. See
//! `crates/compare/BENCH.md` for how to run it and report its numbers.
//!
//! It times sirocco (and, through `scripts/bench.sh`, a base revision of
//! it in the same binary), libdeflate and zlib-rs on:
//!
//! - synthetic data in five output size classes (under 256 B up to MB
//!   scale), as stored, fixed-Huffman and libdeflate level 1/6/9 streams,
//!   plus slices of the real corpus when there is one;
//! - each real-corpus file whole;
//! - every zlib stream of real git packs, in pack order and per size class;
//!
//! each in one call ([`Mode::OneShot`]), streamed ([`Mode::Stream`]) and
//! measured without output ([`Mode::Measure`]).

pub mod cases;
mod impls;
mod report;
mod run;

use std::path::PathBuf;

pub use cases::{Case, Kind, SizeClass, Span};
pub use impls::{Inflater, Libdeflate, Mode, Sirocco, ZlibRs, IN_CHUNK, OUT_CHUNK};
pub use report::{summarize, Row, Run};
pub use run::{run, Timing};

const USAGE: &str = "\
usage: bench [run] [OPTIONS]
       bench summarize [--cases] FILE...

run options:
  --rounds N         timing rounds (default 10)
  --sample-ms F      repeat each case until a sample takes about F ms (default 2)
  --modes LIST       oneshot,stream,measure (default all)
  --impls LIST       only these decompressors (sirocco is always kept)
  --filter TEXT      only cases whose name contains TEXT (repeatable)
  --levels LIST      libdeflate levels for whole corpus files (default 6)
  --pack PATH        a git .pack file, or a directory of them (repeatable)
  --pack-entries N   at most N streams per pack (default 100000)
  --no-synthetic     skip the synthetic cases
  --no-files         skip whole corpus files
  --out FILE         write the results there (a TSV for `summarize`)
  --meta KEY=VALUE   record KEY in the results (repeatable)

summarize prints Markdown tables over results files; files with the same
`bmi2` value are averaged as code layouts of one build. --cases prints
every case instead of the groups.";

/// Whether this build skips sirocco's BMI2 decode loops (see
/// `scripts/bench.sh --bmi2`).
#[must_use]
pub fn bmi2_label() -> &'static str {
    if cfg!(sirocco_no_bmi2) {
        return "off";
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("bmi2") {
            "on"
        } else {
            "absent"
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        "n/a"
    }
}

/// Runs the suite's command line with sirocco, libdeflate, zlib-rs and
/// `extra` (for example a base revision of sirocco).
#[allow(clippy::print_stdout, clippy::print_stderr)]
pub fn main(extra: Vec<Box<dyn Inflater>>) {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("summarize") => {
            let per_case = args.iter().any(|a| a == "--cases");
            let runs: Vec<Run> = args[1..]
                .iter()
                .filter(|a| !a.starts_with("--"))
                .map(|a| Run::read(a.as_ref()))
                .collect();
            assert!(!runs.is_empty(), "{USAGE}");
            print!("{}", summarize(&runs, per_case));
            return;
        }
        Some("run") => {
            args.remove(0);
        }
        Some("-h" | "--help") => {
            println!("{USAGE}");
            return;
        }
        _ => {}
    }

    let mut timing = Timing {
        rounds: 10,
        sample_ms: 2.0,
    };
    let mut modes = Mode::ALL.to_vec();
    let mut only: Option<Vec<String>> = None;
    let mut filters: Vec<String> = Vec::new();
    let mut levels = vec![6];
    let mut packs: Vec<PathBuf> = Vec::new();
    let mut pack_entries = 100_000;
    let (mut synthetic, mut files) = (true, true);
    let mut out: Option<PathBuf> = None;
    let mut meta = vec![("bmi2".to_owned(), bmi2_label().to_owned())];

    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .unwrap_or_else(|| panic!("{a} needs a value\n{USAGE}"))
        };
        match a.as_str() {
            "--rounds" => timing.rounds = val().parse::<usize>().unwrap().max(1),
            "--sample-ms" => timing.sample_ms = val().parse().unwrap(),
            "--modes" => {
                modes = val()
                    .split(',')
                    .map(|m| Mode::parse(m).unwrap_or_else(|| panic!("unknown mode {m}")))
                    .collect();
            }
            "--impls" => only = Some(val().split(',').map(Into::into).collect()),
            "--filter" => filters.push(val()),
            "--levels" => levels = val().split(',').map(|l| l.parse().unwrap()).collect(),
            "--pack" => packs.push(val().into()),
            "--pack-entries" => pack_entries = val().parse().unwrap(),
            "--no-synthetic" => synthetic = false,
            "--no-files" => files = false,
            "--out" => out = Some(val().into()),
            "--meta" => {
                let v = val();
                let (k, v) = v.split_once('=').expect("--meta KEY=VALUE");
                meta.retain(|(m, _)| m != k);
                meta.push((k.into(), v.into()));
            }
            // `cargo bench`-style harness flags.
            "--bench" => {}
            _ => panic!("unknown option {a}\n{USAGE}"),
        }
    }

    let mut imps: Vec<Box<dyn Inflater>> = vec![Box::new(Sirocco::new())];
    imps.extend(extra);
    imps.push(Box::new(Libdeflate::default()));
    imps.push(Box::new(ZlibRs::default()));
    if let Some(only) = &only {
        imps.retain(|i| i.name() == "sirocco" || only.iter().any(|o| o == i.name()));
    }

    let started = std::time::Instant::now();
    let corpus = crate::real_corpus();
    let mut cases = Vec::new();
    if synthetic {
        let joined: Vec<u8> = corpus.iter().flat_map(|s| s.data.iter().copied()).collect();
        cases.extend(cases::synthetic(
            (!joined.is_empty()).then_some(&joined[..]),
            &SizeClass::ALL,
        ));
    }
    if files {
        cases.extend(cases::files(&corpus, &levels));
    }
    for p in &packs {
        for pack in cases::find_packs(p) {
            cases.extend(cases::pack(&pack, pack_entries));
        }
    }
    drop(corpus);
    if !filters.is_empty() {
        cases.retain(|c| filters.iter().any(|f| c.name.contains(f.as_str())));
    }
    eprintln!(
        "{} cases, {} decompressors, built in {:.1}s; bmi2 {}",
        cases.len(),
        imps.len(),
        started.elapsed().as_secs_f64(),
        bmi2_label()
    );
    assert!(!cases.is_empty(), "no cases to run");

    let rows = run(&cases, &mut imps, &modes, &timing);
    let run = Run { meta, rows };
    if let Some(path) = out {
        std::fs::write(&path, run.to_tsv())
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }
    print!("{}", summarize(std::slice::from_ref(&run), false));
}
