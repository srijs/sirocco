#!/usr/bin/env bash
# Downloads the public corpora sirocco is tested and benchmarked against.
#
#   scripts/fetch-corpus.sh [--dir DIR] [SET...]
#
# SETs: silesia canterbury large enwik8 precompressed logs (default: all).
# Files land in DIR/<set>/ (default DIR: ./corpus, which is gitignored), which
# is where `compare::corpus()` looks unless SIROCCO_CORPUS_DIR says otherwise.
# Re-running skips sets that are already complete.
#
#   silesia        Silesia corpus, 12 files, 202 MiB: text, executables,
#                  databases, medical images, XML.
#   canterbury     Canterbury corpus, 11 small files (text, code, binaries).
#   large          Canterbury large corpus: E.coli, bible.txt, world192.txt.
#   enwik8         First 100 MB of an English Wikipedia XML dump.
#   precompressed  Already-compressed data (gzip, xz), built locally from
#                  silesia. Incompressible input mostly takes the stored/
#                  literal paths, which ratio and speed should not regress on.
#   logs           A JSON log and an access log, 39 MB, generated locally
#                  (scripts/gen-logs.py, with python3), which the
#                  compressor's levels are calibrated on with silesia.
set -euo pipefail

dir=corpus
sets=()
while [ $# -gt 0 ]; do
  case "$1" in
    --dir) dir="$2"; shift 2 ;;
    -h|--help) sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) sets+=("$1"); shift ;;
  esac
done
[ ${#sets[@]} -gt 0 ] || sets=(silesia canterbury large enwik8 precompressed logs)

mkdir -p "$dir"
dir=$(cd "$dir" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fetch() { # url dest
  echo "  downloading $1" >&2
  curl -fSL --retry 3 --progress-bar -o "$2" "$1"
}

# Checks every expected file is present and non-empty, then marks the set done.
finish() { # set file...
  local set=$1; shift
  for f in "$@"; do
    [ -s "$dir/$set/$f" ] || { echo "error: $set/$f missing after download" >&2; exit 1; }
  done
  touch "$dir/$set/.complete"
  echo "$set: $# files in $dir/$set" >&2
}

done_already() { [ -e "$dir/$1/.complete" ] && echo "$1: already present" >&2; }

silesia_files=(dickens mozilla mr nci ooffice osdb reymont samba sao webster x-ray xml)
canterbury_files=(alice29.txt asyoulik.txt cp.html fields.c grammar.lsp kennedy.xls lcet10.txt plrabn12.txt ptt5 sum xargs.1)
large_files=(E.coli bible.txt world192.txt)

for set in "${sets[@]}"; do
  done_already "$set" && continue
  echo "$set:" >&2
  rm -rf "${dir:?}/$set"
  mkdir -p "$dir/$set"
  case "$set" in
    silesia)
      if fetch https://sun.aei.polsl.pl/~sdeor/corpus/silesia.zip "$tmp/silesia.zip"; then
        unzip -q -o "$tmp/silesia.zip" -d "$dir/silesia"
      else
        # The official host is unreachable from some sandboxes; this mirror
        # has the same files, one zip each.
        echo "  falling back to the GitHub mirror" >&2
        for f in "${silesia_files[@]}"; do
          fetch "https://raw.githubusercontent.com/MiloszKrajewski/SilesiaCorpus/master/$f.zip" "$tmp/$f.zip"
          unzip -q -o "$tmp/$f.zip" -d "$dir/silesia"
        done
      fi
      finish silesia "${silesia_files[@]}" ;;
    canterbury)
      fetch https://corpus.canterbury.ac.nz/resources/cantrbry.tar.gz "$tmp/cantrbry.tar.gz"
      tar -xzf "$tmp/cantrbry.tar.gz" -C "$dir/canterbury"
      finish canterbury "${canterbury_files[@]}" ;;
    large)
      fetch https://corpus.canterbury.ac.nz/resources/large.tar.gz "$tmp/large.tar.gz"
      tar -xzf "$tmp/large.tar.gz" -C "$dir/large"
      finish large "${large_files[@]}" ;;
    enwik8)
      fetch https://mattmahoney.net/dc/enwik8.zip "$tmp/enwik8.zip"
      unzip -q -o "$tmp/enwik8.zip" -d "$dir/enwik8"
      finish enwik8 enwik8 ;;
    precompressed)
      src="$dir/silesia"
      [ -e "$src/.complete" ] || { echo "error: precompressed is built from silesia; fetch that first" >&2; exit 1; }
      out="$dir/precompressed"
      gzip -9 -c "$src/dickens" > "$out/dickens.gz"
      gzip -6 -c "$src/samba" > "$out/samba.gz"
      xz -6 -c "$src/xml" > "$out/xml.xz"
      finish precompressed dickens.gz samba.gz xml.xz ;;
    logs)
      python3 "$(dirname "$0")/gen-logs.py" "$dir/logs"
      finish logs app.jsonl access.log ;;
    *) echo "error: unknown set '$set'" >&2; exit 2 ;;
  esac
done
