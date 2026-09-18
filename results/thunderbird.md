## tbird-sample.tsv · jev-1.13.0

22833750 lines (415754 alerts) → 11812 templates → 13080 new judgements, 13080 cached verdicts · 15274299 input tokens · $0.6415 (judging every line: $1119.90)
templating 384366 lines/s · judge latency p50 370 ms, p95 442 ms · wall 383.1s · fallbacks 0

| scorer | threshold | precision | recall | F1 | templates flagged | PR-AUC | ROC-AUC |
|---|---:|---:|---:|---:|---:|---:|---:|
| tocsin | 0.750 | 0.000 | 0.000 | 0.000 | 123 | 0.122 | 0.867 |
| jev pageable only | 0.710* | 0.156 | 0.998 | 0.269 | 298 | 0.155 | 0.899 |
| keyword rules | 0.500 | 0.123 | 0.999 | 0.220 | 763 | 0.373 | 0.984 |
| rare templates | 0.000* | 0.026 | 1.000 | 0.050 | 11803 | 0.026 | 0.297 |

\* threshold picked with the labels (best case for that scorer)

best F1 reachable with one decision per template: 1.000

| route | lines | templates | alert precision |
|---|---:|---:|---:|
| Page | 737644 | 123 | 0.000 |
| Ticket | 4001587 | 380 | 0.104 |
| Log | 18094519 | 11390 | 0.000 |

calibration · line-weighted Brier 0.1020, ECE 0.1958 · per-template Brier 0.0260, ECE 0.0906

| predicted | templates | mean predicted | observed alert rate |
|---|---:|---:|---:|
| 0.0–0.1 | 10459 | 0.053 | 0.000 |
| 0.1–0.2 | 413 | 0.137 | 0.002 |
| 0.2–0.3 | 149 | 0.245 | 0.000 |
| 0.3–0.4 | 143 | 0.350 | 0.006 |
| 0.4–0.5 | 201 | 0.448 | 0.002 |
| 0.5–0.6 | 131 | 0.547 | 0.008 |
| 0.6–0.7 | 149 | 0.655 | 0.013 |
| 0.7–0.8 | 112 | 0.745 | 0.038 |
| 0.8–0.9 | 50 | 0.845 | 0.100 |
| 0.9–1.0 | 5 | 0.907 | 0.000 |
