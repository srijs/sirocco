# Benchmark suite

One suite measures every decompression speed change, so numbers from
different branches and machines can be compared. It lives in
`crates/compare/src/suite`; `scripts/bench.sh` builds and runs it.

```sh
scripts/fetch-corpus.sh silesia canterbury        # once: real data in corpus/
scripts/bench.sh --base origin/main               # the full A/B: ~40 min on a 4-core cloud VM
scripts/bench.sh --base origin/main --quick       # a smoke run while iterating: ~5 min
```

The summary is printed and saved, with the raw results, under
`target/bench/results/<time>-<revision>/`.

## What it times

Every case is checked (each decompressor's output must match the input)
before it is timed.

| Dimension | Values |
| --- | --- |
| Output size class | `<256B`, `256B-4K`, `4K-64K`, `64K-1M`, `>=1M`. Small classes are thousands of different streams per case, so one sample is not one call. |
| Block type | `stored` (libdeflate level 0), `fixed` (fixed Huffman blocks only, from zlib-rs's `Fixed` strategy), `L1`, `L6`, `L9` (libdeflate's levels, mostly dynamic blocks) |
| Data | synthetic `text`, `binary` records, `random`, `repeat` (short-period runs: overlapping copies), and `corpus` slices of the real corpus |
| Format | raw DEFLATE; zlib for small text (as git), gzip for MB-scale text |
| Real corpus | each file whole, at `--levels` (default 6) |
| Git packs | every zlib stream of a pack, in pack order, plus the same streams split by size class |
| Mode | `oneshot` (`Decompressor::decompress_into`), `stream` (`StreamDecompressor`, 32 KiB input and 64 KiB output pieces), `measure` (`StreamMeasurer`, 32 KiB pieces, as gitoxide's pack indexing) |
| Decompressors | `sirocco` (working tree), `base` (with `--base`), `libdeflate` (one-shot only), `zlib-rs` (its `Inflate`, reset between streams; in `measure` it inflates into a scratch buffer) |
| BMI2 | `on` and `off` on x86-64 (`--bmi2`) |

Without `-- --pack ...`, the script benchmarks a pack of this repository.
Real repositories make better packs; their pack order is what git and
gitoxide read:

```sh
git clone --bare https://github.com/GitoxideLabs/gitoxide.git /tmp/gitoxide.git
scripts/bench.sh --base origin/main -- --pack /tmp/gitoxide.git/objects/pack
```

## Method

Timing on shared machines moves more than most changes do, so the method is
built in rather than left to each run:

- **A/B in one binary.** `--base REV` copies REV's library into
  `target/bench/base`, renames its package `sirocco_base`, and links it next
  to the working tree's, so both run in the same process on the same data.
- **Code layouts.** Where the compiler places functions and loops moves
  timings by 5-10% on its own, even between identical code. Each run is built
  five times with different alignment flags (`default`, `align32`,
  `align64-fn64`, `align64`, `nofallthru`) and the summary averages them.
- **Interleaved rounds.** Each round times every decompressor once on every
  case, rotating which goes first; a sample repeats a case for about 2 ms; a
  case's time is its best round. Runs are pinned to one CPU with `taskset`
  where it exists.
- **BMI2.** `--bmi2 off` builds sirocco with `--cfg sirocco_no_bmi2`,
  which skips its BMI2 decode loops. libdeflate always picks its BMI2 loop at
  runtime, so the `off` tables compare sirocco's default loop with
  libdeflate's BMI2 loop.

To see the noise floor on a machine, A/B the working tree against itself:
`scripts/bench.sh --base HEAD`. Differences there are noise. On a 4-core
cloud Xeon, groups of many cases averaged within ±1% (layout brackets about
±1%), while single-case rows such as a pack in order moved up to ±1% with
brackets of ±3-5%.

## Reading the summary

There is one table per BMI2 setting and mode. Each row is a group of cases;
`n` is how many. `sirocco MB/s` is uncompressed bytes over time, averaged
over layouts. `vs X` is sirocco's speed relative to X, as a geometric mean
over the group's cases (each case counts the same, whatever its size), so
`+5%` means sirocco is 5% faster. The brackets give the lowest and highest
layout: a change whose bracket spans zero is not shown to be real.

`bench summarize --cases target/bench/results/<dir>/*.tsv` prints every case
instead of the groups.

## Reporting numbers

When a thread or PR claims a speed change:

1. Run the full suite against the PR's base: `scripts/bench.sh --base
   origin/main`, with Silesia and Canterbury fetched and at least one real
   pack (`--pack`) when small outputs matter. `--quick` is for iterating, not
   for claims.
2. Quote the `vs base` column with its bracket for the rows the change is
   about, plus `synthetic, all`, `corpus files, all` and the pack rows, from
   both BMI2 tables. Say which mode (`oneshot` unless the change is about
   streaming or measuring).
3. Name the machine (the `cpu:` line), the corpora and packs, and the base and
   new revisions (the summary's first lines).
4. Attach `summary.md` (or paste it into the PR description under a
   `<details>` block), so later threads can compare against it.

A line in a thread then reads like this, for some change:
"oneshot, BMI2 on, 5 layouts, cloud Xeon 2.8 GHz, Silesia + gitoxide pack,
vs base: synthetic <256B +X% (lo..hi), corpus files +Y% (lo..hi), gitoxide
pack in order +Z% (lo..hi); BMI2 off: ...".

For reference, main at a9f323e on that machine (BMI2 on, oneshot): synthetic
all 683 MB/s, +2.5% vs libdeflate and +31% vs zlib-rs; synthetic <256B +6.4%
vs libdeflate; Silesia files 756 MB/s, -3.9% vs libdeflate; gitoxide pack in
order 461 MB/s, +1.9% vs libdeflate. With BMI2 off, Silesia is -8.5% vs
libdeflate (whose loop still uses BMI2).

## Options

`scripts/bench.sh --help` lists the script's options (`--layouts`, `--bmi2`,
`--cpu`, `--out`, `--quick`). Options after `--` go to the suite itself:

```text
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
```

For example, only small outputs in one-shot mode against base:
`scripts/bench.sh --base origin/main -- --modes oneshot --impls base
--filter '<256B' --filter 256B-4K`.

The suite also runs without the script, for one build and no base:
`cargo run -p compare --release --bin bench`.

The base must have `StreamMeasurer` (sirocco after PR #29). To add a case,
extend `suite/cases.rs`; to add a decompressor, implement
`suite::Inflater` and add it in `suite::main`.
