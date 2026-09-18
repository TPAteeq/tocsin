use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::NAME;
use crate::cache::Cache;
use crate::judge::{Judge, Verdict};
use crate::route::{Route, Thresholds};
use crate::template::{self, Templater};

#[derive(Debug, Clone)]
pub struct Triaged {
    pub cluster: usize,
    pub template: Arc<str>,
    pub verdict: Arc<Verdict>,
    pub route: Route,
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub lines: u64,
    pub judged: u64,
    pub fallbacks: u64,
    pub input_tokens: u64,
    pub model: Option<String>,
    pub template_secs: f64,
    pub judge_secs: f64,
    pub(crate) latencies_ms: Vec<u32>,
}

pub struct Pipeline<J> {
    templater: Templater,
    judge: Arc<J>,
    cache: Cache,
    thresholds: Thresholds,
    concurrency: usize,
    stats: Stats,
    saved_at: Instant,
    latency_stride: u64,
}

const CHECKPOINT: Duration = Duration::from_secs(30);
const MAX_LATENCY_SAMPLES: usize = 100_000;

impl<J: Judge> Pipeline<J> {
    pub fn new(
        judge: J,
        cache: Cache,
        thresholds: Thresholds,
        templates: template::Config,
        concurrency: usize,
    ) -> Self {
        let stats = Stats {
            model: cache.model().map(str::to_string),
            ..Stats::default()
        };
        Self {
            templater: Templater::new(templates),
            judge: Arc::new(judge),
            cache,
            thresholds,
            concurrency: concurrency.max(1),
            stats,
            saved_at: Instant::now(),
            latency_stride: 1,
        }
    }

    pub fn thresholds(&self) -> Thresholds {
        self.thresholds
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn cached(&self) -> usize {
        self.cache.len()
    }

    pub fn clusters(&self) -> usize {
        self.templater.clusters()
    }

    pub fn save(&mut self) -> anyhow::Result<()> {
        self.saved_at = Instant::now();
        self.cache.save()
    }

    pub fn checkpoint(&mut self) -> anyhow::Result<()> {
        if self.saved_at.elapsed() < CHECKPOINT {
            return Ok(());
        }
        self.save()
    }

    pub async fn process<S: AsRef<str>>(&mut self, lines: &[S]) -> Vec<Triaged> {
        let started = Instant::now();
        let assigned: Vec<_> = lines
            .iter()
            .map(|line| self.templater.assign(line.as_ref()))
            .collect();
        self.stats.template_secs += started.elapsed().as_secs_f64();
        self.stats.lines += lines.len() as u64;

        let mut verdicts: HashMap<Arc<str>, Option<Arc<Verdict>>> = HashMap::new();
        for a in &assigned {
            verdicts
                .entry(a.template.clone())
                .or_insert_with(|| self.cache.get(&a.template).cloned());
        }
        let pending = verdicts
            .iter()
            .filter(|(_, verdict)| verdict.is_none())
            .map(|(template, _)| template.clone())
            .collect();
        for (template, verdict) in self.judge_all(pending).await {
            verdicts.insert(template, Some(verdict));
        }

        assigned
            .into_iter()
            .map(|a| {
                let verdict = verdicts[&a.template]
                    .clone()
                    .expect("every template is judged");
                Triaged {
                    route: self.thresholds.route(&verdict),
                    cluster: a.cluster,
                    template: a.template,
                    verdict,
                }
            })
            .collect()
    }

    fn sample_latency(&mut self, elapsed: Duration) {
        if self.stats.judged % self.latency_stride != 0 {
            return;
        }
        let samples = &mut self.stats.latencies_ms;
        samples.push(elapsed.as_millis() as u32);
        if samples.len() >= MAX_LATENCY_SAMPLES {
            let mut keep = false;
            samples.retain(|_| {
                keep = !keep;
                keep
            });
            self.latency_stride *= 2;
        }
    }

    async fn judge_all(&mut self, pending: Vec<Arc<str>>) -> Vec<(Arc<str>, Arc<Verdict>)> {
        let started = Instant::now();
        let mut judged = Vec::with_capacity(pending.len());
        let mut queue = pending.into_iter();
        let mut tasks = JoinSet::new();
        loop {
            while tasks.len() < self.concurrency {
                let Some(template) = queue.next() else { break };
                let judge = self.judge.clone();
                tasks.spawn(async move {
                    let started = Instant::now();
                    let result = judge.judge(&template).await;
                    (template, result, started.elapsed())
                });
            }
            let Some(joined) = tasks.join_next().await else {
                break;
            };
            let (template, result, elapsed) = joined.expect("judge task panicked");
            let verdict = match result {
                Ok(done) => {
                    self.stats.judged += 1;
                    self.stats.input_tokens += done.input_tokens;
                    self.sample_latency(elapsed);
                    let stored = self
                        .cache
                        .insert(template.clone(), done.verdict, &done.model);
                    self.stats.model = Some(done.model);
                    stored
                }
                Err(err) => {
                    eprintln!("{NAME}: judge failed, falling back to keyword rules: {err:#}");
                    self.stats.fallbacks += 1;
                    Arc::new(self.judge.fallback(&template))
                }
            };
            judged.push((template, verdict));
        }
        self.stats.judge_secs += started.elapsed().as_secs_f64();
        judged
    }
}

pub async fn next_batch<T>(
    rx: &mut mpsc::Receiver<T>,
    max: usize,
    linger: Duration,
) -> Option<Vec<T>> {
    let mut batch = Vec::new();
    if rx.recv_many(&mut batch, max).await == 0 {
        return None;
    }
    let deadline = tokio::time::Instant::now() + linger;
    while batch.len() < max {
        let room = max - batch.len();
        match tokio::time::timeout_at(deadline, rx.recv_many(&mut batch, room)).await {
            Ok(n) if n > 0 => {}
            _ => break,
        }
    }
    Some(batch)
}

#[derive(Serialize)]
struct Output<'a> {
    route: Route,
    attention: f32,
    #[serde(flatten)]
    verdict: &'a Verdict,
    template: &'a str,
    line: &'a str,
}

pub fn write_jsonl<W: Write, S: AsRef<str>>(
    out: &mut W,
    lines: &[S],
    triaged: &[Triaged],
    routes: &[Route],
) -> std::io::Result<()> {
    for (line, t) in lines.iter().zip(triaged) {
        if !routes.contains(&t.route) {
            continue;
        }
        let output = Output {
            route: t.route,
            attention: t.verdict.attention(),
            verdict: &t.verdict,
            template: &t.template,
            line: line.as_ref(),
        };
        serde_json::to_writer(&mut *out, &output).map_err(std::io::Error::from)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::judge::Judged;

    #[derive(Default)]
    struct Scripted {
        calls: AtomicUsize,
        seen: Mutex<Vec<String>>,
    }

    impl Judge for Arc<Scripted> {
        fn fingerprint(&self) -> String {
            "scripted".into()
        }

        async fn judge(&self, template: &str) -> anyhow::Result<Judged> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(template.to_string());
            if template.contains("explode") {
                anyhow::bail!("service unavailable");
            }
            let pageable = if template.contains("failed") {
                0.95
            } else {
                0.02
            };
            Ok(Judged {
                verdict: Verdict {
                    pageable,
                    detail: 0.0,
                    severity: pageable * 3.0,
                    area: "application".into(),
                },
                input_tokens: 100,
                model: "scripted-1".into(),
            })
        }
    }

    fn pipeline(judge: Arc<Scripted>) -> Pipeline<Arc<Scripted>> {
        Pipeline::new(
            judge,
            Cache::memory("scripted".into()),
            Thresholds::default(),
            template::Config::default(),
            4,
        )
    }

    #[tokio::test]
    async fn judges_each_template_once_across_batches() {
        let judge = Arc::new(Scripted::default());
        let mut p = pipeline(judge.clone());
        let lines: Vec<String> = (0..1000)
            .map(|i| {
                if i % 10 == 0 {
                    format!("payment {i} failed for card {i}")
                } else {
                    format!("GET /items/{i} 200")
                }
            })
            .collect();

        let first = p.process(&lines[..500]).await;
        let second = p.process(&lines[500..]).await;

        assert_eq!(first.len() + second.len(), 1000);
        assert_eq!(first[0].route, Route::Page);
        assert_eq!(first[1].route, Route::Log);
        assert_eq!(
            judge.calls.load(Ordering::SeqCst),
            p.stats().judged as usize
        );
        assert!(p.stats().judged <= 4, "judged {} times", p.stats().judged);
        assert_eq!(p.stats().input_tokens, p.stats().judged * 100);
        assert_eq!(p.stats().model.as_deref(), Some("scripted-1"));
    }

    #[tokio::test]
    async fn failed_judgements_fall_back_and_are_retried() {
        let judge = Arc::new(Scripted::default());
        let mut p = pipeline(judge.clone());

        let first = p.process(&["disk explode failed"]).await;
        let second = p.process(&["disk explode failed"]).await;

        assert_eq!(first[0].verdict.pageable, 0.7);
        assert_eq!(second[0].route, Route::Ticket);
        assert_eq!(p.stats().fallbacks, 2);
        assert_eq!(judge.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn batches_flush_on_size_or_linger() {
        let (tx, mut rx) = mpsc::channel(16);
        for i in 0..5 {
            tx.send(i).await.unwrap();
        }
        assert_eq!(
            next_batch(&mut rx, 3, Duration::from_millis(10)).await,
            Some(vec![0, 1, 2])
        );
        assert_eq!(
            next_batch(&mut rx, 3, Duration::from_millis(10)).await,
            Some(vec![3, 4])
        );
        drop(tx);
        assert_eq!(
            next_batch(&mut rx, 3, Duration::from_millis(10)).await,
            None
        );
    }

    #[test]
    fn writes_only_selected_routes() {
        let verdict = Arc::new(Verdict {
            pageable: 0.9,
            detail: 0.0,
            severity: 2.4,
            area: "storage".into(),
        });
        let triaged = |route| Triaged {
            cluster: 1,
            template: "disk <*> full".into(),
            verdict: verdict.clone(),
            route,
        };
        let mut out = Vec::new();
        write_jsonl(
            &mut out,
            &["disk a full", "ok"],
            &[triaged(Route::Page), triaged(Route::Log)],
            &[Route::Page],
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains(r#""route":"page""#) && text.contains(r#""line":"disk a full""#));
    }
}
