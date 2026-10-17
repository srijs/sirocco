//! Results files and the summary tables made from them.
//!
//! A results file is tab-separated: `#key<TAB>value` lines for the run's
//! metadata (layout, bmi2, revisions), then a header line and one [`Row`]
//! per case, mode and decompressor.

// Byte counts and case counts fit in f64.
#![allow(clippy::cast_precision_loss)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use super::cases::{Kind, SizeClass};
use super::impls::Mode;
use crate::Format;

/// One decompressor's time on one case and mode.
#[derive(Clone, Debug)]
pub struct Row {
    pub case: String,
    pub kind: Kind,
    pub size: Option<SizeClass>,
    pub encoding: String,
    pub source: String,
    pub format: Format,
    pub mode: Mode,
    pub imp: String,
    pub streams: usize,
    /// Decompressed bytes in one pass over the case.
    pub bytes: usize,
    pub reps: u32,
    /// Seconds per pass: the best round.
    pub best: f64,
    /// Seconds per pass: the median round.
    pub median: f64,
}

const HEADER: &str =
    "case\tkind\tsize\tencoding\tsource\tformat\tmode\timpl\tstreams\tbytes\treps\tbest_ns\tmedian_ns";

fn format_name(f: Format) -> &'static str {
    match f {
        Format::Deflate => "deflate",
        Format::Zlib => "zlib",
        Format::Gzip => "gzip",
    }
}

/// One run of the suite: one build (code layout, BMI2 or not).
#[derive(Clone, Debug, Default)]
pub struct Run {
    pub meta: Vec<(String, String)>,
    pub rows: Vec<Row>,
}

impl Run {
    #[must_use]
    pub fn meta(&self, key: &str) -> Option<&str> {
        self.meta
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[must_use]
    pub fn to_tsv(&self) -> String {
        let mut s = String::new();
        for (k, v) in &self.meta {
            writeln!(s, "#{k}\t{v}").unwrap();
        }
        writeln!(s, "{HEADER}").unwrap();
        for r in &self.rows {
            writeln!(
                s,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.0}\t{:.0}",
                r.case,
                r.kind.name(),
                r.size.map_or("mixed", SizeClass::label),
                r.encoding,
                r.source,
                format_name(r.format),
                r.mode.name(),
                r.imp,
                r.streams,
                r.bytes,
                r.reps,
                r.best * 1e9,
                r.median * 1e9,
            )
            .unwrap();
        }
        s
    }

    /// Reads a results file written by [`Run::to_tsv`].
    #[must_use]
    pub fn read(path: &Path) -> Run {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let mut run = Run::default();
        for line in text.lines() {
            if let Some(m) = line.strip_prefix('#') {
                let (k, v) = m.split_once('\t').unwrap_or((m, ""));
                run.meta.push((k.into(), v.into()));
                continue;
            }
            if line == HEADER || line.is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 13, "{}: bad line {line:?}", path.display());
            run.rows.push(Row {
                case: f[0].into(),
                kind: Kind::parse(f[1]).expect("kind"),
                size: SizeClass::parse(f[2]),
                encoding: f[3].into(),
                source: f[4].into(),
                format: match f[5] {
                    "zlib" => Format::Zlib,
                    "gzip" => Format::Gzip,
                    _ => Format::Deflate,
                },
                mode: Mode::parse(f[6]).expect("mode"),
                imp: f[7].into(),
                streams: f[8].parse().unwrap(),
                bytes: f[9].parse().unwrap(),
                reps: f[10].parse().unwrap(),
                best: f[11].parse::<f64>().unwrap() / 1e9,
                median: f[12].parse::<f64>().unwrap() / 1e9,
            });
        }
        run
    }
}

/// The decompressor every other is compared with.
const REFERENCE: &str = "sirocco";

/// One row of a summary table: a named set of cases.
struct Group {
    label: String,
    cases: Vec<String>,
}

fn groups(rows: &[&Row]) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    let mut add = |label: String, pred: &dyn Fn(&Row) -> bool| {
        let mut cases: Vec<String> = rows
            .iter()
            .filter(|r| pred(r))
            .map(|r| r.case.clone())
            .collect();
        cases.dedup();
        if !cases.is_empty() {
            out.push(Group { label, cases });
        }
    };
    let syn = |r: &Row| r.kind == Kind::Synthetic;
    add("synthetic, all".into(), &syn);
    for c in SizeClass::ALL {
        add(format!("synthetic {}", c.label()), &|r| {
            syn(r) && r.size == Some(c)
        });
    }
    let mut encodings: Vec<&str> = rows
        .iter()
        .filter(|r| syn(r))
        .map(|r| r.encoding.as_str())
        .collect();
    encodings.sort_by_key(|e| (e.len(), *e));
    encodings.dedup();
    for e in encodings {
        add(format!("synthetic {e}"), &|r| syn(r) && r.encoding == e);
    }
    for s in ["text", "binary", "random", "repeat", "corpus"] {
        add(format!("synthetic {s}"), &|r| syn(r) && r.source == s);
    }
    for (f, name) in [(Format::Zlib, "zlib"), (Format::Gzip, "gzip")] {
        add(format!("synthetic {name}"), &|r| syn(r) && r.format == f);
    }
    let file = |r: &Row| r.kind == Kind::File;
    add("corpus files, all".into(), &file);
    let mut levels: Vec<&str> = rows
        .iter()
        .filter(|r| file(r))
        .map(|r| r.encoding.as_str())
        .collect();
    levels.sort_by_key(|e| (e.len(), *e));
    levels.dedup();
    if levels.len() > 1 {
        for l in levels {
            add(format!("corpus files {l}"), &|r| file(r) && r.encoding == l);
        }
    }
    let mut packs: Vec<&str> = rows
        .iter()
        .filter(|r| r.kind == Kind::Pack)
        .map(|r| r.case.as_str())
        .collect();
    packs.dedup();
    for p in packs {
        add(format!("{p}, in order"), &|r| r.case == p);
    }
    for c in SizeClass::ALL {
        add(format!("pack entries {}", c.label()), &|r| {
            r.kind == Kind::PackEntries && r.size == Some(c)
        });
    }
    out
}

/// sirocco's speed over a set of cases in one run, and its speed relative
/// to each other decompressor (geometric mean over cases of their time over
/// sirocco's, so each case counts the same).
struct Cell {
    mbs: f64,
    vs: BTreeMap<String, f64>,
}

fn cell(run: &BTreeMap<(&str, &str), &Row>, cases: &[String], imps: &[String]) -> Option<Cell> {
    let (mut bytes, mut secs) = (0.0, 0.0);
    let mut logs: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for c in cases {
        let Some(ff) = run.get(&(c.as_str(), REFERENCE)) else {
            continue;
        };
        bytes += ff.bytes as f64;
        secs += ff.best;
        for imp in imps {
            if let Some(o) = run.get(&(c.as_str(), imp.as_str())) {
                let e = logs.entry(imp.clone()).or_default();
                e.0 += (o.best / ff.best).ln();
                e.1 += 1;
            }
        }
    }
    (secs > 0.0).then(|| Cell {
        mbs: bytes / secs / 1e6,
        vs: logs
            .into_iter()
            .map(|(k, (l, n))| (k, (l / n as f64).exp()))
            .collect(),
    })
}

fn pct(ratio: f64) -> String {
    format!("{:+.1}%", (ratio - 1.0) * 100.0)
}

/// Summary tables over runs that differ only in code layout, as Markdown.
///
/// Runs are grouped by their `bmi2` metadata; within a group each value is
/// first computed per run, then averaged (MB/s: mean; ratios: geometric
/// mean, with the lowest and highest run in brackets).
#[must_use]
pub fn summarize(runs: &[Run], per_case: bool) -> String {
    let mut s = String::new();
    let mut by_bmi2: BTreeMap<String, Vec<&Run>> = BTreeMap::new();
    for r in runs {
        by_bmi2
            .entry(r.meta("bmi2").unwrap_or("?").to_owned())
            .or_default()
            .push(r);
    }
    if let Some(r) = runs.first() {
        for key in ["new", "base", "cpu"] {
            if let Some(v) = r.meta(key) {
                writeln!(s, "{key}: {v}  ").unwrap();
            }
        }
    }
    writeln!(
        s,
        "\n\"vs X\" is sirocco's speed relative to X: +5% means sirocco is 5% faster.\n"
    )
    .unwrap();

    // `on` before `off`.
    for (bmi2, runs) in by_bmi2.iter().rev() {
        let layouts: Vec<&str> = runs
            .iter()
            .map(|r| r.meta("layout").unwrap_or("?"))
            .collect();
        writeln!(
            s,
            "## BMI2 {bmi2}: {} layout{} ({})\n",
            runs.len(),
            if runs.len() == 1 { "" } else { "s" },
            layouts.join(", ")
        )
        .unwrap();
        for mode in Mode::ALL {
            let rows: Vec<&Row> = runs[0].rows.iter().filter(|r| r.mode == mode).collect();
            if rows.is_empty() {
                continue;
            }
            let mut imps: Vec<String> = Vec::new();
            for r in &rows {
                if r.imp != REFERENCE && !imps.contains(&r.imp) {
                    imps.push(r.imp.clone());
                }
            }
            let maps: Vec<BTreeMap<(&str, &str), &Row>> = runs
                .iter()
                .map(|run| {
                    run.rows
                        .iter()
                        .filter(|r| r.mode == mode)
                        .map(|r| ((r.case.as_str(), r.imp.as_str()), r))
                        .collect()
                })
                .collect();

            writeln!(s, "### {}\n", mode.name()).unwrap();
            write!(s, "| cases | n | sirocco MB/s |").unwrap();
            for i in &imps {
                write!(s, " vs {i} |").unwrap();
            }
            write!(s, "\n|---|--:|--:|").unwrap();
            for _ in &imps {
                write!(s, "--:|").unwrap();
            }
            writeln!(s).unwrap();

            let mut table = |label: &str, cases: &[String], spread: bool| {
                let cells: Vec<Cell> = maps.iter().filter_map(|m| cell(m, cases, &imps)).collect();
                if cells.is_empty() {
                    return;
                }
                let mbs = cells.iter().map(|c| c.mbs).sum::<f64>() / cells.len() as f64;
                write!(s, "| {label} | {} | {mbs:.0} |", cases.len()).unwrap();
                for i in &imps {
                    let rs: Vec<f64> = cells.iter().filter_map(|c| c.vs.get(i).copied()).collect();
                    if rs.is_empty() {
                        write!(s, " |").unwrap();
                        continue;
                    }
                    let g = (rs.iter().map(|r| r.ln()).sum::<f64>() / rs.len() as f64).exp();
                    write!(s, " {}", pct(g)).unwrap();
                    if spread && rs.len() > 1 {
                        let lo = rs.iter().copied().fold(f64::MAX, f64::min);
                        let hi = rs.iter().copied().fold(f64::MIN, f64::max);
                        write!(s, " ({}..{})", pct(lo), pct(hi)).unwrap();
                    }
                    write!(s, " |").unwrap();
                }
                writeln!(s).unwrap();
            };
            if per_case {
                let mut cases: Vec<String> = rows.iter().map(|r| r.case.clone()).collect();
                cases.dedup();
                for c in cases {
                    table(&c, std::slice::from_ref(&c), false);
                }
            } else {
                for g in groups(&rows) {
                    table(&g.label, &g.cases, true);
                }
            }
            writeln!(s).unwrap();
        }
    }
    s
}
