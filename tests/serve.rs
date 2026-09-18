use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

struct Server {
    child: Child,
    base: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn start(webhook: &str) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tocsin"))
        .args([
            "serve",
            "--offline",
            "--no-cache",
            "--listen",
            "127.0.0.1:0",
            "--only",
            "page",
        ])
        .args(["--webhook", webhook])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tocsin");
    let mut line = String::new();
    BufReader::new(child.stderr.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let base = line.trim().rsplit(' ').next().unwrap().to_string();
    Server { child, base }
}

type Hook = (mpsc::Sender<Value>, Arc<AtomicUsize>);

async fn flaky_webhook() -> (String, mpsc::Receiver<Value>) {
    let (tx, rx) = mpsc::channel(8);
    let app = Router::new()
        .route(
            "/hook",
            post(
                |State((tx, attempts)): State<Hook>, body: axum::Json<Value>| async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        return StatusCode::SERVICE_UNAVAILABLE;
                    }
                    tx.send(body.0).await.unwrap();
                    StatusCode::OK
                },
            ),
        )
        .with_state((tx, Arc::new(AtomicUsize::new(0))));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, rx)
}

#[tokio::test]
async fn ingests_text_and_otlp_and_pages_once_per_template() {
    let (hook, mut alerts) = flaky_webhook().await;
    let server = start(&hook);
    let http = reqwest::Client::new();

    let text = "GET /health 200 3ms\nkernel panic - not syncing: fatal exception on cpu 3\nkernel panic - not syncing: fatal exception on cpu 7\n";
    let res: Value = http
        .post(format!("{}/ingest", server.base))
        .body(text)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["accepted"], 3);

    let otlp = json!({"resourceLogs": [{
        "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "billing"}}]},
        "scopeLogs": [{"logRecords": [{"severityText": "ERROR", "body": {"stringValue": "invoice 42 failed to render"}}]}]
    }]});
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(otlp.to_string().as_bytes()).unwrap();
    let res = http
        .post(format!("{}/v1/logs", server.base))
        .header("content-type", "application/json")
        .header("content-encoding", "gzip")
        .body(gz.finish().unwrap())
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success(), "{}", res.status());

    let mut rows: Vec<Value> = Vec::new();
    for _ in 0..50 {
        rows = http
            .get(format!("{}/templates", server.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if rows.len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(rows.len(), 3, "{rows:#?}");
    assert_eq!(rows[0]["route"], "page");
    assert_eq!(rows[0]["attention"], 0.9);
    assert_eq!(rows[0]["count"], 2);
    assert!(
        rows.iter()
            .any(|r| r["template"] == "GET /health <2xx> 3ms")
    );
    assert!(
        rows.iter()
            .any(|r| r["template"] == "billing ERROR invoice <NUM> failed to render")
    );

    let alert = tokio::time::timeout(Duration::from_secs(10), alerts.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(alert["count"], 2);
    assert!(alert["text"].as_str().unwrap().contains("kernel panic"));
    assert!(
        tokio::time::timeout(Duration::from_millis(500), alerts.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn flushes_pending_alerts_on_sigterm() {
    let (hook, mut alerts) = flaky_webhook().await;
    let server = start(&hook);
    let http = reqwest::Client::new();
    let page = "kernel panic - not syncing: fatal exception on cpu 1\n";

    http.post(format!("{}/ingest", server.base))
        .body(page)
        .send()
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(10), alerts.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first["count"], 1);

    http.post(format!("{}/ingest", server.base))
        .body(page)
        .send()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(300), alerts.recv())
            .await
            .is_err(),
        "cooldown should hold the second occurrence"
    );

    Command::new("kill")
        .args(["-TERM", &server.child.id().to_string()])
        .status()
        .unwrap();

    let flushed = tokio::time::timeout(Duration::from_secs(10), alerts.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(flushed["count"], 1);
}

#[tokio::test]
async fn rejects_malformed_otlp() {
    let server = start("http://127.0.0.1:9/unused");
    let res = reqwest::Client::new()
        .post(format!("{}/v1/logs", server.base))
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
}
