#!/usr/bin/env sh
set -eu

case "${1:-}" in
  bgl) header=6 ;;
  thunderbird) header=8 ;;
  *) echo "usage: $0 bgl|thunderbird < raw.log > labeled.tsv" >&2; exit 2 ;;
esac

LC_ALL=C awk -v header="$header" '{
  line = ""
  for (i = header + 1; i <= NF; i++) line = line (i > header + 1 ? " " : "") $i
  print ($1 == "-" ? 0 : 1) "\t" line
}'
