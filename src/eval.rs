use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, bail};
use serde::Serialize;

use crate::judge::{Judge, Rules};
use crate::metrics::{Calibration, Confusion, Ranking, calibration, confusion, ranking};
use crate::pipeline::{Pipeline, Stats};
use crate::route::Route;

const BATCH: usize = 50_000;

#[derive(Debug, Serialize)]
pub struct Report {
    pub dataset: String,
    pub model: Option<String>,
    pub lines: u64,
    pub positives: u64,
    pub templates: u64,
    pub judged_templates: u64,
    pub cached_verdicts: u64,
    pub fallbacks: u64,
    pub input_tokens: u64,
    pub usd: f64,
    pub per_line_usd: f64,
    pub latency_p50_ms: u32,
    pub latency_p95_ms: u32,
    pub templating_lines_per_sec: f64,
    pub wall_secs: f64,
    pub template_ceiling_f1: f64,
    pub scorers: Vec<Scorer>,
    pub routes: Vec<RouteCount>,
    pub line_calibration: Calibration,
    pub template_calibration: Calibration,
    pub template_breakdown: Vec<TemplateRow>,
}

#[derive(Debug, Serialize)]
pub struct TemplateRow {
    pub template: Arc<str>,
    pub lines: u64,
    pub alerts: u64,
    pub attention: f64,
}

#[derive(Debug, Serialize)]
pub struct Scorer {
    pub name: &'static str,
    pub threshold: f32,
    pub threshold_is_oracle: bool,
    pub at_threshold: Confusion,
    pub flagged_templates: u64,
    pub ranking: Ranking,
}

#[derive(Debug, Serialize)]
pub struct RouteCount {
    pub route: Route,
    pub lines: u64,
    pub templates: u64,
    pub precision: f64,
}

struct Labeled {
    template: Arc<str>,
    label: bool,
    attention: f32,
    pageable: f32,
    keywords: f32,
    cluster: usize,
    route: Route,
}

#[derive(Default)]
struct Cluster {
    lines: u64,
    alerts: u64,
    attention: f64,
}

pub async fn run<J: Judge>(
    pipeline: &mut Pipeline<J>,
    path: &Path,
    limit: Option<usize>,
    usd_per_mtok: f64,
) -> anyhow::Result<Report> {
    let started = Instant::now();
    let mut reader =
        BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
    let mut rows = Vec::new();
    let (mut texts, mut labels) = (Vec::with_capacity(BATCH), Vec::with_capacity(BATCH));
    let mut buf = Vec::new();
    let limit = limit.unwrap_or(usize::MAX);

    let mut number = 0;
    while rows.len() + texts.len() < limit {
        number += 1;
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let Some((label, text)) = line.trim_end_matches(['\r', '\n']).split_once('\t') else {
            bail!("expected `<0|1>\\t<log line>`, got: {}", line.trim_end());
        };
        labels.push(match label {
            "1" => true,
            "0" => false,
            other => bail!("line {number}: label must be 0 or 1, got {other:?}"),
        });
        texts.push(text.to_string());
        if texts.len() == BATCH {
            score_batch(pipeline, &mut texts, &mut labels, &mut rows).await;
        }
    }
    score_batch(pipeline, &mut texts, &mut labels, &mut rows).await;
    pipeline.save()?;

    Ok(summarize(
        pipeline.stats(),
        pipeline.cached(),
        pipeline.thresholds().page,
        path,
        rows,
        usd_per_mtok,
        started,
    ))
}

async fn score_batch<J: Judge>(
    pipeline: &mut Pipeline<J>,
    texts: &mut Vec<String>,
    labels: &mut Vec<bool>,
    rows: &mut Vec<Labeled>,
) {
    for (t, (text, label)) in pipeline
        .process(texts)
        .await
        .into_iter()
        .zip(texts.iter().zip(labels.iter()))
    {
        rows.push(Labeled {
            template: t.template,
            label: *label,
            attention: t.verdict.attention(),
            pageable: t.verdict.pageable,
            keywords: Rules::level(text).0,
            cluster: t.cluster,
            route: t.route,
        });
    }
    texts.clear();
    labels.clear();
}

fn summarize(
    stats: &Stats,
    cached: usize,
    page_threshold: f32,
    path: &Path,
    rows: Vec<Labeled>,
    usd_per_mtok: f64,
    started: Instant,
) -> Report {
    let labels: Vec<bool> = rows.iter().map(|r| r.label).collect();

    let mut clusters: HashMap<usize, Cluster> = HashMap::new();
    let mut names: HashMap<usize, &Arc<str>> = HashMap::new();
    for r in &rows {
        let c = clusters.entry(r.cluster).or_default();
        c.lines += 1;
        c.alerts += r.label as u64;
        c.attention += r.attention as f64;
        names.insert(r.cluster, &r.template);
    }
    let mut templates: Vec<TemplateRow> = clusters
        .iter()
        .map(|(id, c)| TemplateRow {
            template: names[id].clone(),
            lines: c.lines,
            alerts: c.alerts,
            attention: c.attention / c.lines as f64,
        })
        .collect();
    templates.sort_by_key(|t| std::cmp::Reverse(t.lines));

    let attention: Vec<f32> = rows.iter().map(|r| r.attention).collect();
    let pageable: Vec<f32> = rows.iter().map(|r| r.pageable).collect();
    let keywords: Vec<f32> = rows.iter().map(|r| r.keywords).collect();
    let rarity: Vec<f32> = rows
        .iter()
        .map(|r| 1.0 / clusters[&r.cluster].lines as f32)
        .collect();
    let alert_rate: Vec<f32> = rows
        .iter()
        .map(|r| {
            let c = &clusters[&r.cluster];
            c.alerts as f32 / c.lines as f32
        })
        .collect();

    let scorer = |name, scores: &[f32], threshold: Option<f32>| {
        let ranked = ranking(scores, &labels);
        let threshold_is_oracle = threshold.is_none();
        let threshold = threshold.unwrap_or(ranked.best_threshold);
        let flagged_templates = rows
            .iter()
            .zip(scores)
            .filter(|(_, s)| **s >= threshold)
            .map(|(r, _)| r.cluster)
            .collect::<std::collections::HashSet<_>>()
            .len() as u64;
        Scorer {
            name,
            threshold,
            threshold_is_oracle,
            at_threshold: confusion(scores, &labels, threshold),
            flagged_templates,
            ranking: ranked,
        }
    };

    let routes = [Route::Page, Route::Ticket, Route::Log]
        .into_iter()
        .map(|route| {
            let matched: Vec<&Labeled> = rows.iter().filter(|r| r.route == route).collect();
            let hits = matched.iter().filter(|r| r.label).count();
            RouteCount {
                route,
                lines: matched.len() as u64,
                templates: matched
                    .iter()
                    .map(|r| r.cluster)
                    .collect::<std::collections::HashSet<_>>()
                    .len() as u64,
                precision: if matched.is_empty() {
                    0.0
                } else {
                    hits as f64 / matched.len() as f64
                },
            }
        })
        .collect();

    let (template_p, template_y): (Vec<f32>, Vec<f32>) = clusters
        .values()
        .map(|c| {
            (
                (c.attention / c.lines as f64) as f32,
                c.alerts as f32 / c.lines as f32,
            )
        })
        .unzip();
    let line_targets: Vec<f32> = labels.iter().map(|&y| y as u8 as f32).collect();

    let mut latencies = stats.latencies_ms.clone();
    latencies.sort_unstable();
    let percentile = |q: f64| {
        latencies
            .get(((latencies.len() as f64 - 1.0) * q).round() as usize)
            .copied()
            .unwrap_or(0)
    };
    let tokens_per_call = stats.input_tokens as f64 / stats.judged.max(1) as f64;

    Report {
        dataset: path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
        model: stats.model.clone(),
        lines: rows.len() as u64,
        positives: labels.iter().filter(|&&y| y).count() as u64,
        templates: clusters.len() as u64,
        judged_templates: stats.judged,
        cached_verdicts: cached as u64,
        fallbacks: stats.fallbacks,
        input_tokens: stats.input_tokens,
        usd: stats.input_tokens as f64 / 1e6 * usd_per_mtok,
        per_line_usd: rows.len() as f64 * tokens_per_call / 1e6 * usd_per_mtok,
        latency_p50_ms: percentile(0.5),
        latency_p95_ms: percentile(0.95),
        templating_lines_per_sec: stats.lines as f64 / stats.template_secs.max(1e-9),
        wall_secs: started.elapsed().as_secs_f64(),
        template_ceiling_f1: ranking(&alert_rate, &labels).best_f1,
        scorers: vec![
            scorer("tocsin", &attention, Some(page_threshold)),
            scorer("jev pageable only", &pageable, None),
            scorer("keyword rules", &keywords, Some(0.5)),
            scorer("rare templates", &rarity, None),
        ],
        routes,
        line_calibration: calibration(&attention, &line_targets, 10),
        template_calibration: calibration(&template_p, &template_y, 10),
        template_breakdown: templates,
    }
}

impl Report {
    pub fn markdown(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "## {} · {}\n",
            self.dataset,
            self.model.as_deref().unwrap_or("unknown model")
        );
        let _ = writeln!(
            s,
            "{} lines ({} alerts) → {} templates → {} new judgements, {} cached verdicts · {} input tokens · ${:.4} (judging every line: ${:.2})",
            self.lines,
            self.positives,
            self.templates,
            self.judged_templates,
            self.cached_verdicts,
            self.input_tokens,
            self.usd,
            self.per_line_usd
        );
        let _ = writeln!(
            s,
            "templating {:.0} lines/s · judge latency p50 {} ms, p95 {} ms · wall {:.1}s · fallbacks {}\n",
            self.templating_lines_per_sec,
            self.latency_p50_ms,
            self.latency_p95_ms,
            self.wall_secs,
            self.fallbacks
        );
        let _ = writeln!(
            s,
            "| scorer | threshold | precision | recall | F1 | templates flagged | PR-AUC | ROC-AUC |"
        );
        let _ = writeln!(s, "|---|---:|---:|---:|---:|---:|---:|---:|");
        for sc in &self.scorers {
            let c = &sc.at_threshold;
            let _ = writeln!(
                s,
                "| {} | {:.3}{} | {:.3} | {:.3} | {:.3} | {} | {:.3} | {:.3} |",
                sc.name,
                sc.threshold,
                if sc.threshold_is_oracle { "*" } else { "" },
                c.precision(),
                c.recall(),
                c.f1(),
                sc.flagged_templates,
                sc.ranking.average_precision,
                sc.ranking.roc_auc
            );
        }
        let _ = writeln!(
            s,
            "\n\\* threshold picked with the labels (best case for that scorer)\n\nbest F1 reachable with one decision per template: {:.3}\n",
            self.template_ceiling_f1
        );
        let _ = writeln!(
            s,
            "| route | lines | templates | alert precision |\n|---|---:|---:|---:|"
        );
        for r in &self.routes {
            let _ = writeln!(
                s,
                "| {:?} | {} | {} | {:.3} |",
                r.route, r.lines, r.templates, r.precision
            );
        }
        let _ = writeln!(
            s,
            "\ncalibration · line-weighted Brier {:.4}, ECE {:.4} · per-template Brier {:.4}, ECE {:.4}\n",
            self.line_calibration.brier,
            self.line_calibration.ece,
            self.template_calibration.brier,
            self.template_calibration.ece
        );
        let _ = writeln!(
            s,
            "| predicted | templates | mean predicted | observed alert rate |\n|---|---:|---:|---:|"
        );
        for b in self
            .template_calibration
            .bins
            .iter()
            .filter(|b| b.count > 0)
        {
            let _ = writeln!(
                s,
                "| {:.1}–{:.1} | {} | {:.3} | {:.3} |",
                b.lo, b.hi, b.count, b.mean_predicted, b.observed_rate
            );
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labeled(cluster: usize, attention: f32, label: bool) -> Labeled {
        Labeled {
            template: "t".into(),
            label,
            attention,
            pageable: attention,
            keywords: 0.05,
            cluster,
            route: if attention >= 0.75 {
                Route::Page
            } else {
                Route::Log
            },
        }
    }

    #[test]
    fn summarize_scores_the_shipped_threshold() {
        let rows = vec![
            labeled(1, 0.9, true),
            labeled(1, 0.9, true),
            labeled(2, 0.1, false),
            labeled(2, 0.1, false),
            labeled(3, 0.9, false),
        ];
        let report = summarize(
            &Stats::default(),
            0,
            0.75,
            Path::new("unit.tsv"),
            rows,
            0.042,
            Instant::now(),
        );

        assert_eq!(
            (report.lines, report.positives, report.templates),
            (5, 2, 3)
        );
        let tocsin = &report.scorers[0].at_threshold;
        assert_eq!((tocsin.tp, tocsin.fp, tocsin.fn_, tocsin.tn), (2, 1, 0, 2));
        assert_eq!(report.scorers[0].flagged_templates, 2);
        assert_eq!(report.template_ceiling_f1, 1.0);
        let paged = report
            .routes
            .iter()
            .find(|r| r.route == Route::Page)
            .unwrap();
        assert_eq!((paged.lines, paged.templates), (3, 2));
    }

    #[test]
    fn summarize_prices_the_run_from_token_usage() {
        let stats = Stats {
            judged: 2,
            input_tokens: 1_000_000,
            ..Stats::default()
        };
        let rows = vec![labeled(1, 0.9, true), labeled(2, 0.1, false)];
        let report = summarize(
            &stats,
            7,
            0.75,
            Path::new("u.tsv"),
            rows,
            0.042,
            Instant::now(),
        );

        assert!((report.usd - 0.042).abs() < 1e-9);
        assert!((report.per_line_usd - 0.042).abs() < 1e-9);
        assert_eq!(report.cached_verdicts, 7);
    }
}
