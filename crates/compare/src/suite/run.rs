//! Checking and timing every case with every decompressor.

use std::hint::black_box;
use std::time::Instant;

use super::cases::Case;
use super::impls::{Inflater, Mode};
use super::report::Row;

/// How long to time.
#[derive(Clone, Debug)]
pub struct Timing {
    /// Passes over every case. Each round times every decompressor once per
    /// case and mode, in an order that rotates between rounds.
    pub rounds: usize,
    /// Each timed sample repeats a case until it takes about this long, so
    /// small cases are not lost in timer resolution.
    pub sample_ms: f64,
}

struct Entry {
    case: usize,
    mode: Mode,
    imps: Vec<usize>,
    reps: u32,
    times: Vec<Vec<f64>>,
}

fn pass(imp: &mut dyn Inflater, mode: Mode, case: &Case, out: &mut [u8], reps: u32) -> f64 {
    let t = Instant::now();
    for _ in 0..reps {
        black_box(imp.run(mode, black_box(case), out));
    }
    t.elapsed().as_secs_f64() / f64::from(reps)
}

/// Checks that every decompressor decodes every case right, then times them.
/// The first decompressor (sirocco) sets each case's repeat count.
pub fn run(cases: &[Case], imps: &mut [Box<dyn Inflater>], modes: &[Mode], t: &Timing) -> Vec<Row> {
    let max = cases.iter().map(|c| c.expected.len()).max().unwrap_or(0);
    let mut out = vec![0u8; max];
    let mut entries = Vec::new();
    for (ci, case) in cases.iter().enumerate() {
        let out = &mut out[..case.expected.len()];
        for &mode in modes {
            let ids: Vec<usize> = (0..imps.len())
                .filter(|&i| imps[i].supports(mode))
                .collect();
            if ids.is_empty() {
                continue;
            }
            for &i in &ids {
                out.fill(0xa5);
                let n = imps[i].run(mode, case, out);
                let who = format!("{} {} on {}", imps[i].name(), mode.name(), case.name);
                assert_eq!(n, case.expected.len(), "{who}: wrong length");
                if mode != Mode::Measure {
                    assert!(*out == *case.expected, "{who}: wrong output");
                }
            }
            // The check above warmed up the first decompressor.
            let one = pass(&mut *imps[ids[0]], mode, case, out, 1);
            let reps = ((t.sample_ms / 1e3 / one).ceil() as u32).clamp(1, 1 << 20);
            entries.push(Entry {
                case: ci,
                mode,
                times: vec![Vec::with_capacity(t.rounds); ids.len()],
                imps: ids,
                reps,
            });
        }
    }

    for r in 0..t.rounds {
        let start = Instant::now();
        for e in &mut entries {
            let case = &cases[e.case];
            let out = &mut out[..case.expected.len()];
            let n = e.imps.len();
            for k in 0..n {
                let j = (k + r) % n;
                let secs = pass(&mut *imps[e.imps[j]], e.mode, case, out, e.reps);
                e.times[j].push(secs);
            }
        }
        #[allow(clippy::print_stderr)]
        {
            eprintln!(
                "round {}/{} done in {:.1}s",
                r + 1,
                t.rounds,
                start.elapsed().as_secs_f64()
            );
        }
    }

    let mut rows = Vec::new();
    for e in entries {
        let case = &cases[e.case];
        for (j, mut times) in e.imps.iter().zip(e.times) {
            times.sort_by(f64::total_cmp);
            rows.push(Row {
                case: case.name.clone(),
                kind: case.kind,
                size: case.size,
                encoding: case.encoding.clone(),
                source: case.source.clone(),
                format: case.format,
                mode: e.mode,
                imp: imps[*j].name().to_owned(),
                streams: case.streams.len(),
                bytes: case.expected.len(),
                reps: e.reps,
                best: times[0],
                median: times[times.len() / 2],
            });
        }
    }
    rows
}
