use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::NAME;
use crate::judge::Verdict;
use crate::pipeline::Triaged;
use crate::route::Route;

const MAX_ROWS: usize = 100_000;
const QUEUE: usize = 1024;
const ATTEMPTS: u32 = 4;

#[derive(Default)]
pub struct Board {
    rows: HashMap<usize, Row>,
    tick: u64,
}

#[derive(Clone, Serialize)]
pub struct Row {
    template: Arc<str>,
    route: Route,
    attention: f32,
    #[serde(flatten)]
    verdict: Arc<Verdict>,
    count: u64,
    example: String,
    #[serde(skip)]
    unnotified: u64,
    #[serde(skip)]
    notified_at: Option<Instant>,
    #[serde(skip)]
    seen_at: u64,
}

#[derive(Debug, Serialize)]
pub struct Alert {
    text: String,
    template: Arc<str>,
    example: String,
    count: u64,
    #[serde(flatten)]
    verdict: Arc<Verdict>,
}

impl Board {
    pub fn record(&mut self, lines: &[String], triaged: &[Triaged]) {
        for (line, t) in lines.iter().zip(triaged) {
            self.tick += 1;
            let now = self.tick;
            let row = self.rows.entry(t.cluster).or_insert_with(|| Row {
                template: t.template.clone(),
                route: t.route,
                attention: 0.0,
                verdict: t.verdict.clone(),
                count: 0,
                example: line.clone(),
                unnotified: 0,
                notified_at: None,
                seen_at: now,
            });
            if !Arc::ptr_eq(&row.template, &t.template) {
                row.template = t.template.clone();
            }
            row.attention = t.verdict.attention();
            row.verdict = t.verdict.clone();
            row.route = t.route;
            row.count += 1;
            row.seen_at = now;
            if t.route == Route::Page {
                row.unnotified += 1;
                row.example.clone_from(line);
            }
        }
        if self.rows.len() > MAX_ROWS {
            self.evict(MAX_ROWS / 2);
        }
    }

    pub fn snapshot(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self.rows.values().cloned().collect();
        rows.sort_by(|a, b| {
            b.attention
                .total_cmp(&a.attention)
                .then(b.count.cmp(&a.count))
        });
        rows
    }

    fn evict(&mut self, keep: usize) {
        let mut seen: Vec<u64> = self.rows.values().map(|row| row.seen_at).collect();
        let Some(index) = seen.len().checked_sub(keep) else {
            return;
        };
        let cutoff = *seen.select_nth_unstable(index).1;
        self.rows
            .retain(|_, row| row.seen_at >= cutoff || row.unnotified > 0);
    }

    pub fn due(&mut self, now: Instant, cooldown: Duration, force: bool) -> Vec<Alert> {
        self.rows
            .values_mut()
            .filter(|row| {
                row.unnotified > 0
                    && (force
                        || row
                            .notified_at
                            .is_none_or(|at| now.duration_since(at) >= cooldown))
            })
            .map(|row| {
                let alert = Alert {
                    text: format!(
                        "{NAME} · {} · attention={:.2} severity={:.1} · {} new occurrence(s)\n{}",
                        row.verdict.area,
                        row.attention,
                        row.verdict.severity,
                        row.unnotified,
                        row.template
                    ),
                    template: row.template.clone(),
                    example: row.example.clone(),
                    count: row.unnotified,
                    verdict: row.verdict.clone(),
                };
                row.unnotified = 0;
                row.notified_at = Some(now);
                alert
            })
            .collect()
    }
}

pub struct Notifier {
    tx: mpsc::Sender<Alert>,
    worker: JoinHandle<()>,
}

impl Notifier {
    pub fn start(http: reqwest::Client, webhook: Option<String>) -> Self {
        let (tx, mut rx) = mpsc::channel::<Alert>(QUEUE);
        let worker = tokio::spawn(async move {
            while let Some(alert) = rx.recv().await {
                match &webhook {
                    Some(url) => deliver(&http, url, &alert).await,
                    None => eprintln!("PAGE {}", alert.text.replace('\n', " · ")),
                }
            }
        });
        Self { tx, worker }
    }

    pub async fn send(&self, alerts: Vec<Alert>) {
        for alert in alerts {
            if self.tx.send(alert).await.is_err() {
                eprintln!("{NAME}: notifier stopped, dropping an alert");
            }
        }
    }

    pub fn sender(&self) -> mpsc::Sender<Alert> {
        self.tx.clone()
    }

    pub async fn close(self) {
        drop(self.tx);
        let _ = self.worker.await;
    }
}

/// Flushes alerts whose cooldown expired, so a quiet stream still pages.
pub fn flush_every(
    board: Arc<Mutex<Board>>,
    alerts: mpsc::Sender<Alert>,
    every: Duration,
    cooldown: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        loop {
            tick.tick().await;
            let due = board.lock().unwrap().due(Instant::now(), cooldown, false);
            for alert in due {
                if alerts.send(alert).await.is_err() {
                    return;
                }
            }
        }
    })
}

async fn deliver(http: &reqwest::Client, url: &str, alert: &Alert) {
    for attempt in 1..=ATTEMPTS {
        let sent = http
            .post(url)
            .json(alert)
            .send()
            .await
            .and_then(|res| res.error_for_status());
        let Err(err) = sent else { return };
        let transient = err
            .status()
            .is_none_or(|s| s.is_server_error() || s == reqwest::StatusCode::TOO_MANY_REQUESTS);
        if !transient || attempt == ATTEMPTS {
            eprintln!("{NAME}: webhook failed: {}", err.without_url());
            return;
        }
        tokio::time::sleep(Duration::from_secs(1 << (attempt - 1))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triaged(cluster: usize, route: Route) -> Triaged {
        Triaged {
            cluster,
            template: "disk <*> full".into(),
            verdict: Arc::new(Verdict {
                pageable: 0.9,
                detail: 0.0,
                severity: 2.5,
                area: "storage".into(),
            }),
            route,
        }
    }

    #[test]
    fn pages_once_per_template_within_cooldown() {
        let mut board = Board::default();
        let lines = vec!["disk a full".to_string(), "disk b full".to_string()];
        let batch = vec![triaged(7, Route::Page), triaged(7, Route::Page)];
        let cooldown = Duration::from_secs(600);
        let start = Instant::now();

        board.record(&lines, &batch);
        let first = board.due(start, cooldown, false);
        board.record(&lines, &batch);
        let within = board.due(start + Duration::from_secs(1), cooldown, false);
        let after = board.due(start + cooldown, cooldown, false);

        assert_eq!(first.len(), 1);
        assert_eq!(first[0].count, 2);
        assert!(within.is_empty());
        assert_eq!(after[0].count, 2);
        assert_eq!(board.rows[&7].count, 4);
    }

    #[test]
    fn shutdown_flushes_pending_alerts() {
        let mut board = Board::default();
        let lines = vec!["disk a full".to_string()];
        let now = Instant::now();
        board.record(&lines, &[triaged(1, Route::Page)]);
        board.due(now, Duration::from_secs(600), false);
        board.record(&lines, &[triaged(1, Route::Page)]);

        assert!(board.due(now, Duration::from_secs(600), false).is_empty());
        assert_eq!(board.due(now, Duration::from_secs(600), true)[0].count, 1);
    }

    #[test]
    fn logs_never_page() {
        let mut board = Board::default();
        board.record(&["ok".to_string()], &[triaged(1, Route::Log)]);
        assert!(board.due(Instant::now(), Duration::ZERO, true).is_empty());
    }

    #[test]
    fn eviction_keeps_recent_and_pending_rows() {
        let mut board = Board::default();
        for cluster in 0..10 {
            board.record(&["x".to_string()], &[triaged(cluster, Route::Ticket)]);
        }
        board.rows.get_mut(&0).unwrap().unnotified = 1;
        board.evict(3);

        let mut kept: Vec<_> = board.rows.keys().copied().collect();
        kept.sort();
        assert_eq!(kept, vec![0, 7, 8, 9]);
    }

    #[test]
    fn snapshot_sorts_by_attention() {
        let mut board = Board::default();
        board.record(&["a".to_string()], &[triaged(1, Route::Log)]);
        let mut quiet = triaged(2, Route::Log);
        quiet.verdict = Arc::new(Verdict {
            pageable: 0.1,
            ..(*quiet.verdict).clone()
        });
        board.record(&["b".to_string()], &[quiet]);

        let rows = board.snapshot();
        assert!(rows[0].attention > rows[1].attention);
    }
}
