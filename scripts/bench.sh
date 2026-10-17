#!/usr/bin/env bash
# Runs the benchmark suite (crates/compare/src/suite) the way every speed
# claim in this repo should be measured. See crates/compare/BENCH.md.
#
#   scripts/bench.sh [OPTIONS] [-- SUITE_OPTIONS]
#
# Options:
#   --base REV     also time REV (e.g. origin/main) as `base`, in the same
#                  binary as the working tree, so each case is A/B'd
#   --layouts N    average over the first N of 5 code layouts (default 5);
#                  layout alone moves timings 5-10%
#   --bmi2 MODE    on, off or both (default: both on x86-64, else on). `off`
#                  builds sirocco without its BMI2 loops; libdeflate still
#                  uses BMI2 at runtime
#   --cpu N        pin runs to CPU N with taskset (default: the last CPU;
#                  `none` to not pin; ignored without taskset)
#   --out DIR      results directory (default target/bench/results/<time>)
#   --quick        a smoke run: 1 layout, BMI2 on, 3 rounds
#
# SUITE_OPTIONS go to every run (see `cargo run -p compare --release --bin
# bench -- --help`), e.g. `--filter pack --modes oneshot`. Without a `--pack`
# option, a pack of this repository is benchmarked; pass real ones with
# `-- --pack path/to/repo.git/objects/pack`.
#
# The summary is printed and saved as DIR/summary.md; DIR/*.tsv are the raw
# results, which `bench summarize` reads.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
base=""
layouts=5
arch=$(uname -m)
if [ "$arch" = x86_64 ]; then bmi2=both; else bmi2=on; fi
cpu=default
out=""
quick=0
suite=()
while [ $# -gt 0 ]; do
  case "$1" in
    --base) base="$2"; shift 2 ;;
    --layouts) layouts="$2"; shift 2 ;;
    --bmi2) bmi2="$2"; shift 2 ;;
    --cpu) cpu="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    --quick) quick=1; shift ;;
    -h|--help) sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    --) shift; suite+=("$@"); break ;;
    *) echo "unknown option $1 (see --help)" >&2; exit 1 ;;
  esac
done
if [ "$quick" = 1 ]; then
  layouts=1
  bmi2=on
  suite=(--rounds 3 "${suite[@]+"${suite[@]}"}")
fi
if [ "$arch" != x86_64 ] && [ "$bmi2" != on ]; then
  echo "--bmi2 $bmi2: BMI2 is x86-64 only, running with --bmi2 on" >&2
  bmi2=on
fi

# The code layouts: the same code, functions and loops placed differently.
layout_names=(default align32 align64-fn64 align64 nofallthru)
layout_flags=(
  ""
  "-C llvm-args=-align-loops=32 -C llvm-args=-align-all-functions=5"
  "-C llvm-args=-align-loops=64 -C llvm-args=-align-all-functions=6"
  "-C llvm-args=-align-loops=64"
  "-C llvm-args=-align-all-nofallthru-blocks=5"
)
[ "$layouts" -ge 1 ] && [ "$layouts" -le 5 ] || { echo "--layouts must be 1-5" >&2; exit 1; }

bench="$root/target/bench"
new_rev="$(git -C "$root" rev-parse --short HEAD)$(git -C "$root" diff --quiet HEAD -- . || echo +dirty)"
[ -n "$out" ] || out="$bench/results/$(date +%Y%m%d-%H%M%S)-$new_rev"
mkdir -p "$out"
out=$(cd "$out" && pwd)

# Base: REV's library, renamed so it can sit next to the working tree's.
runner="$bench/runner"
rm -rf "$runner"
mkdir -p "$runner/src"
if [ -n "$base" ]; then
  base_rev=$(git -C "$root" rev-parse --short "$base^{commit}")
  rm -rf "$bench/base"
  mkdir -p "$bench/base"
  git -C "$root" archive "$base_rev" Cargo.toml README.md src | tar -x -C "$bench/base"
  # `tar` keeps the commit's mtimes, which can look older than the last
  # build of another base; touch them so cargo rebuilds.
  find "$bench/base" -type f -exec touch {} +
  # Rename the package, drop its workspace members (not copied), and let
  # `--cfg sirocco_no_bmi2` switch BMI2 off in bases older than that cfg.
  sed -i.orig -e 's/^name = "sirocco"$/name = "sirocco_base"/' \
    -e '/^members = /d' "$bench/base/Cargo.toml"
  if [ -f "$bench/base/src/decompress/mod.rs" ]; then
    sed -i.orig 's/if std::is_x86_feature_detected!("bmi2")/if !cfg!(sirocco_no_bmi2) \&\& std::is_x86_feature_detected!("bmi2")/' \
      "$bench/base/src/decompress/mod.rs"
    rm -f "$bench/base/src/decompress/mod.rs.orig"
  fi
  rm -f "$bench/base/Cargo.toml.orig"
  if ! grep -q sirocco_no_bmi2 "$bench/base/Cargo.toml"; then
    awk '{ print } /^\[workspace\.lints\.rust\]$/ {
      print "unexpected_cfgs = { level = \"warn\", check-cfg = [\"cfg(sirocco_no_bmi2)\"] }" }' \
      "$bench/base/Cargo.toml" >"$bench/base/Cargo.toml.new"
    mv "$bench/base/Cargo.toml.new" "$bench/base/Cargo.toml"
  fi
  grep -q '^name = "sirocco_base"$' "$bench/base/Cargo.toml" || { echo "could not rename the base package" >&2; exit 1; }
  if [ "$bmi2" != on ] && ! grep -rq sirocco_no_bmi2 "$bench/base/src"; then
    echo "base $base_rev has no BMI2 dispatch to switch off; use --bmi2 on" >&2
    exit 1
  fi
  base_dep='sirocco_base = { path = "../base" }'
  # Bases from before the `inflate` module have sirocco 0.1's API.
  if [ -d "$bench/base/src/inflate" ]; then inflater=sirocco_inflater; else inflater=sirocco_v01_inflater; fi
  main="compare::$inflater!(Base, \"base\", sirocco_base);

fn main() {
    compare::suite::main(vec![Box::new(Base::new())]);
}"
else
  base_rev=""
  base_dep=""
  main='fn main() {
    compare::suite::main(Vec::new());
}'
fi
cat > "$runner/Cargo.toml" <<EOF
# Generated by scripts/bench.sh.
[package]
name = "sirocco-bench"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
compare = { path = "../../../crates/compare" }
$base_dep

[profile.release]
debug = true

[workspace]
EOF
echo "$main" > "$runner/src/main.rs"
# Start from the workspace's lockfile so dependency versions match.
cp "$root/Cargo.lock" "$runner/Cargo.lock"

# Default pack: this repository's own objects.
if ! printf '%s\n' "${suite[@]+"${suite[@]}"}" | grep -qx -- --pack; then
  mkdir -p "$bench/packs"
  git -C "$root" pack-objects --all --stdout </dev/null >"$bench/packs/sirocco.pack" 2>/dev/null
  suite+=(--pack "$bench/packs/sirocco.pack")
fi

pin=()
if command -v taskset >/dev/null && [ "$cpu" != none ]; then
  [ "$cpu" != default ] || cpu=$(($(nproc) - 1))
  pin=(taskset -c "$cpu")
fi
case "$(uname -s)" in
  Darwin) cpu_name=$(sysctl -n machdep.cpu.brand_string) ;;
  *) cpu_name=$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//' || true)
     [ -n "$cpu_name" ] || cpu_name=$arch ;;
esac

case "$bmi2" in
  both) bmi2_set=(on off) ;;
  on|off) bmi2_set=("$bmi2") ;;
  *) echo "--bmi2 must be on, off or both" >&2; exit 1 ;;
esac

# Build every variant first, then run them back to back.
builds=()
for b in "${bmi2_set[@]}"; do
  for ((i = 0; i < layouts; i++)); do
    id="${layout_names[$i]}-bmi2$b"
    flags="${layout_flags[$i]}"
    [ "$b" = on ] || flags="$flags --cfg sirocco_no_bmi2"
    echo "building $id" >&2
    CARGO_TARGET_DIR="$bench/build/$id" RUSTFLAGS="$flags" \
      cargo build --quiet --release --manifest-path "$runner/Cargo.toml"
    builds+=("$id")
  done
done

for id in "${builds[@]}"; do
  echo "running $id" >&2
  layout=${id%-bmi2*}
  "${pin[@]+"${pin[@]}"}" "$bench/build/$id/release/sirocco-bench" run \
    --out "$out/$id.tsv" --meta "layout=$layout" --meta "new=$new_rev" \
    ${base_rev:+--meta "base=$base_rev"} --meta "cpu=$cpu_name" \
    "${suite[@]}" >/dev/null
done

"$bench/build/${builds[0]}/release/sirocco-bench" summarize "$out"/*.tsv | tee "$out/summary.md"
echo "results in $out" >&2
