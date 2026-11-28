#!/bin/sh
# Regenerates the synthetic checkup-report fixtures. macOS only: rendering
# uses CoreGraphics and OCR uses Apple Vision. Fake data only.
#
#   tools/report-fixtures/regenerate.sh            # acceptance + eval sets
#   tools/report-fixtures/regenerate.sh acceptance # one set
#
# fixtures/reports/acceptance keeps the rendered files (the tests import
# them); fixtures/reports/eval keeps only the specs and the recorded OCR
# (the rendered files are reproducible and large; see .gitignore).
set -eu
cd "$(dirname "$0")/../.."
sets=${*:-acceptance eval}

cargo build -q -p mirrorself-cli
bin=target/debug/mirrorself
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
xcrun swiftc -O tools/report-fixtures/render.swift -o "$tmp/render"

for set in $sets; do
  dir=fixtures/reports/$set
  if [ "$set" = eval ]; then
    rm -f "$dir"/eval-*.json
    "$bin" dev synth-reports --count 12 --seed 42 --out "$dir" > /dev/null
  fi
  for spec in "$dir"/*.json; do
    case "$spec" in *.ocr.json) continue ;; esac
    id=$(basename "$spec" .json)
    lang=$(sed -n 's/^  "language": "\(..\)",$/\1/p' "$spec")
    rm -f "$dir/$id".pdf "$dir/$id".scan.pdf "$dir/$id".photo-p*.jpg "$dir/$id".*.ocr.json
    "$tmp/render" "$spec"
    # shellcheck disable=SC2086 # the globs are meant to expand
    "$bin" dev ocr --record --lang "$lang" $(ls "$dir/$id".pdf "$dir/$id".scan.pdf "$dir/$id".photo-p*.jpg 2>/dev/null) > /dev/null
    echo "recorded OCR for $id"
  done
done
