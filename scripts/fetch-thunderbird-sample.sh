#!/usr/bin/env sh
set -eu

dir="${1:-data}"
mkdir -p "$dir/tbird"
cd "$dir"

for shard in 00 10 20 30 40 50 60 70; do
  file="tbird/train-000${shard}-of-00074.parquet"
  [ -f "$file" ] || curl -fL --retry 3 -o "$file" \
    "https://huggingface.co/datasets/logfit-project/Thunderbird/resolve/main/data/train-000${shard}-of-00074.parquet"
done

uv run -q --with duckdb python - <<'PY'
import duckdb
duckdb.sql("""
COPY (
  SELECT anomaly,
         regexp_replace(
           CASE WHEN pid = -1 THEN component || ': ' || content
                ELSE component || '[' || pid || ']: ' || content END,
           '[\t\r\n]+', ' ', 'g')
  FROM read_parquet('tbird/*.parquet', filename = true, file_row_number = true)
  ORDER BY filename, file_row_number
) TO 'tbird-sample.tsv' (DELIMITER '\t', HEADER false, QUOTE '', ESCAPE '')
""")
PY
wc -l tbird-sample.tsv
