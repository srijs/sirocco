#!/usr/bin/env bash
# Reports the size of the compressor (src/deflate), to keep an eye on it
# while changing it: compare before and after.
#
#   scripts/deflate-size.sh
#
# - Source: lines of code (not blank, comments or tests) per file and in all.
# - Machine code: bytes of `sirocco::deflate` functions in a release build of
#   examples/gzip (which compresses only).
# - Stack: the largest stack frames of `sirocco::deflate` functions, from
#   `-Z emit-stack-sizes` in a nightly release build for x86-64 Linux (ELF,
#   where the sizes are recorded; x86-64 also has the largest frames). Set
#   STACK_TARGET for another target. Skipped without nightly, its rust-src,
#   or llvm-tools.
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

# Stack frames, if the tools are there.
target=${STACK_TARGET:-x86_64-unknown-linux-gnu}
tools="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin"
if [ ! -x "$tools/llvm-readobj" ]; then
  echo "stack: skipped (rustup component add llvm-tools)"
  exit 0
fi
dir="$root/target/deflate-stack"
if ! RUSTFLAGS="-Z emit-stack-sizes" cargo +nightly build --quiet -Zbuild-std=core,alloc \
    --release --lib --no-default-features --features alloc --target "$target" \
    --manifest-path "$root/Cargo.toml" --target-dir "$dir" 2>/dev/null; then
  echo "stack: skipped (needs nightly with rust-src)"
  exit 0
fi
objs="$dir/objs"
rm -rf "$objs" && mkdir -p "$objs"
(cd "$objs" && "$tools/llvm-ar" x "$dir/$target/release/libsirocco.rlib")
echo "largest stack frames ($target):"
"$tools/llvm-readobj" --stack-sizes --demangle "$objs"/*.o 2>/dev/null | awk '
  function hex(s,  i, n) {
    n = 0
    for (i = 3; i <= length(s); i++) n = n * 16 + index("0123456789abcdef", tolower(substr(s, i, 1))) - 1
    return n
  }
  /Functions: \[/ { sub(/.*Functions: \[/, ""); sub(/\]$/, ""); fn = $0 }
  /Size: 0x/ { if (fn ~ /sirocco::deflate/) printf "%8d bytes  %s\n", hex($2), fn }
' | sort -rn | head -8
