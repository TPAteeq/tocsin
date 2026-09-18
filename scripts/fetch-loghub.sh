#!/usr/bin/env sh
set -eu

scripts="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "${1:-data}"
cd "${1:-data}"

if [ ! -f BGL.log ]; then
  curl -fL --retry 3 -o BGL.zip 'https://zenodo.org/records/8196385/files/BGL.zip?download=1' \
    || curl -fL --retry 3 -o BGL.zip 'https://huggingface.co/datasets/YvanCarre/Loghub_dataset/resolve/main/BGL.zip'
  unzip -o -q BGL.zip BGL.log && rm BGL.zip
fi
[ "$(wc -l < BGL.log | tr -d ' ')" = 4747963 ] || { echo "BGL.log has an unexpected line count" >&2; exit 1; }

sh "$scripts/loghub-to-tsv.sh" bgl < BGL.log > bgl.tsv
wc -l bgl.tsv
