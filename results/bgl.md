## bgl.tsv · jev-1.13.0

4747963 lines (348460 alerts) → 1126 templates → 1496 new judgements, 1496 cached verdicts · 1742551 input tokens · $0.0732 (judging every line: $232.28)
templating 389846 lines/s · judge latency p50 378 ms, p95 460 ms · wall 55.4s · fallbacks 0

| scorer | threshold | precision | recall | F1 | templates flagged | PR-AUC | ROC-AUC |
|---|---:|---:|---:|---:|---:|---:|---:|
| tocsin | 0.750 | 0.913 | 0.813 | 0.860 | 78 | 0.941 | 0.995 |
| jev pageable only | 0.870* | 0.720 | 0.799 | 0.757 | 752 | 0.716 | 0.977 |
| keyword rules | 0.500 | 0.254 | 1.000 | 0.406 | 1037 | 0.348 | 0.926 |
| rare templates | 0.000* | 0.190 | 1.000 | 0.319 | 1121 | 0.166 | 0.771 |

\* threshold picked with the labels (best case for that scorer)

best F1 reachable with one decision per template: 0.996

| route | lines | templates | alert precision |
|---|---:|---:|---:|
| Page | 310377 | 78 | 0.913 |
| Ticket | 424619 | 611 | 0.147 |
| Log | 4012967 | 535 | 0.001 |

calibration · line-weighted Brier 0.0747, ECE 0.1868 · per-template Brier 0.2535, ECE 0.4571

| predicted | templates | mean predicted | observed alert rate |
|---|---:|---:|---:|
| 0.0–0.1 | 38 | 0.062 | 0.000 |
| 0.1–0.2 | 44 | 0.136 | 0.000 |
| 0.2–0.3 | 31 | 0.245 | 0.032 |
| 0.3–0.4 | 34 | 0.355 | 0.025 |
| 0.4–0.5 | 330 | 0.465 | 0.001 |
| 0.5–0.6 | 449 | 0.540 | 0.003 |
| 0.6–0.7 | 94 | 0.640 | 0.053 |
| 0.7–0.8 | 56 | 0.744 | 0.286 |
| 0.8–0.9 | 47 | 0.840 | 0.553 |
| 0.9–1.0 | 3 | 0.912 | 1.000 |
