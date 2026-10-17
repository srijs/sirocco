//! The benchmark suite's own plumbing: every decompressor passes its checks
//! on the small cases, and results survive a round trip through a file.

use compare::suite::{self, cases, Inflater, Mode, Run, SizeClass, Timing};

#[test]
fn small_cases_check_and_summarize() {
    let mut cases = cases::synthetic(None, &[SizeClass::Tiny]);
    // Keep debug-build test time down: a few streams per case is enough.
    for c in &mut cases {
        c.streams.truncate(8);
        let end = c.streams.last().unwrap().output.end;
        c.expected = c.expected[..end].into();
    }
    let mut imps: Vec<Box<dyn Inflater>> = vec![
        Box::new(suite::Sirocco::new()),
        Box::new(suite::Libdeflate::default()),
        Box::new(suite::ZlibRs::default()),
    ];
    let timing = Timing {
        rounds: 1,
        sample_ms: 0.0,
    };
    let rows = suite::run(&cases, &mut imps, &Mode::ALL, &timing);
    // libdeflate only has one-shot.
    assert_eq!(rows.len(), cases.len() * (3 + 2 + 2));

    let run = Run {
        meta: vec![
            ("bmi2".into(), "on".into()),
            ("layout".into(), "default".into()),
        ],
        rows,
    };
    let dir = std::env::temp_dir().join(format!("sirocco-suite-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("run.tsv");
    std::fs::write(&path, run.to_tsv()).unwrap();
    let back = Run::read(&path);
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(back.rows.len(), run.rows.len());
    assert_eq!(back.meta("layout"), Some("default"));

    let summary = suite::summarize(&[back.clone(), back], false);
    assert!(summary.contains("| synthetic <256B |"), "{summary}");
    assert!(summary.contains("vs libdeflate"), "{summary}");
}
