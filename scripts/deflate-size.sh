#!/usr/bin/env bash
# Reports the size of the compressor (src/deflate), to keep an eye on it
# while changing it: compare before and after.
#
#   scripts/deflate-size.sh
#
# - Source: lines of code (not blank, comments or tests) per file and in all.
# - Machine code: bytes of `sirocco::deflate` functions in a release build of
#   examples/gzip (which compresses only).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)

total=0
for f in "$root"/src/deflate/*.rs; do
  case "$f" in */tests.rs) continue ;; esac
  # Up to the tests module, which ends each file that has one.
  n=$(awk '/^mod tests/ { exit } /^[[:space:]]*($|\/\/)/ { next } { n++ } END { print n + 0 }' "$f")
  total=$((total + n))
  printf '%-24s %5d lines\n' "${f#"$root"/}" "$n"
done
printf '%-24s %5d lines\n' total "$total"

cargo build --quiet --release --example gzip --manifest-path "$root/Cargo.toml"
# Linked binaries list symbols by address: each one's size is the gap to the
# next.
bytes=$(nm -n -t d "$root/target/release/examples/gzip" | awk '
  { addr[NR] = $1; type[NR] = $2; name[NR] = $3 }
  END {
    for (i = 1; i < NR; i++)
      if (type[i] ~ /^[tT]$/ && name[i] ~ /7sirocco7deflate/) n += addr[i + 1] - addr[i]
    print n + 0
  }')
printf '%-24s %5d bytes\n' "machine code" "$bytes"
