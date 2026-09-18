use std::future::Future;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tower::limit::ConcurrencyLimitLayer;
use tower_http::decompression::RequestDecompressionLayer;

use crate::alert::{Board, Notifier, Row, flush_every};
use crate::judge::Judge;
use crate::pipeline::{Pipeline, Triaged, next_batch, write_jsonl};
use crate::route::Route;
use crate::{NAME, http_client, ingest};

const MAX_BODY: usize = 16 * 1024 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 64;
const MAX_QUEUED_BYTES: usize = 256 * 1024 * 1024;
const MAX_QUEUED_REQUESTS: usize = 65_536;
const LINE_OVERHEAD: usize = 64;
const BATCH_REQUESTS: usize = 256;
const BATCH_LINGER: Duration = Duration::from_millis(200);
const FLUSH_EVERY: Duration = Duration::from_secs(5);

pub struct Options {
    pub listen: SocketAddr,
    pub webhook: Option<String>,
    pub cooldown: Duration,
    pub emit: Vec<Route>,
}

struct Chunk {
    lines: Vec<String>,
    permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
struct AppState {
    tx: mpsc::Sender<Chunk>,
    budget: Arc<Semaphore>,
    board: Arc<Mutex<Board>>,
}

type Rejection = (StatusCode, String);

pub async fn run<J: Judge>(
    mut pipeline: Pipeline<J>,
    options: Options,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::channel(MAX_QUEUED_REQUESTS);
    let board = Arc::new(Mutex::new(Board::default()));
    let app = router(AppState {
        tx,
        budget: Arc::new(Semaphore::new(MAX_QUEUED_BYTES)),
        board: board.clone(),
    });

    let listener = TcpListener::bind(options.listen).await?;
    eprintln!("{NAME} listening on http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await
    });

    let notifier = Notifier::start(http_client(), options.webhook);
    let flusher = flush_every(
        board.clone(),
        notifier.sender(),
        FLUSH_EVERY,
        options.cooldown,
    );

    // The channel closes only once axum drops the router that owns the sender,
    // so this loop drains everything accepted before shutdown.
    let mut emitting = !options.emit.is_empty();
    while let Some(chunks) = next_batch(&mut rx, BATCH_REQUESTS, BATCH_LINGER).await {
        let (mut lines, mut permits) = (Vec::new(), Vec::with_capacity(chunks.len()));
        for chunk in chunks {
            lines.extend(chunk.lines);
            permits.push(chunk.permit);
        }
        let triaged = pipeline.process(&lines).await;
        let due = {
            let mut board = board.lock().unwrap();
            board.record(&lines, &triaged);
            board.due(Instant::now(), options.cooldown, false)
        };
        notifier.send(due).await;
        if emitting {
            if let Err(err) = emit(&lines, &triaged, &options.emit) {
                eprintln!("{NAME}: stdout closed, no longer printing lines: {err}");
                emitting = false;
            }
        }
        if let Err(err) = pipeline.checkpoint() {
            eprintln!("{NAME}: saving verdict cache failed: {err:#}");
        }
        drop(permits);
    }

    flusher.abort();
    let server = server.await;
    let remaining = board
        .lock()
        .unwrap()
        .due(Instant::now(), options.cooldown, true);
    notifier.send(remaining).await;
    notifier.close().await;
    server??;
    pipeline.save()
}

pub async fn os_signals() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = interrupt => {}
        _ = terminate => {}
    }
}

fn emit(lines: &[String], triaged: &[Triaged], routes: &[Route]) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    write_jsonl(&mut out, lines, triaged, routes)?;
    out.flush()
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/logs", post(otlp))
        .route("/ingest", post(text))
        .route("/templates", get(templates))
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(RequestDecompressionLayer::new())
        .layer(ConcurrencyLimitLayer::new(MAX_CONCURRENT_REQUESTS))
        .with_state(state)
}

async fn otlp(State(state): State<AppState>, body: Bytes) -> Result<Json<Value>, Rejection> {
    let lines = ingest::otlp(&body)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid OTLP/JSON: {e}")))?;
    enqueue(&state, lines)?;
    Ok(Json(json!({})))
}

async fn text(State(state): State<AppState>, body: String) -> Result<Json<Value>, Rejection> {
    let lines = ingest::lines(&body);
    let accepted = lines.len();
    enqueue(&state, lines)?;
    Ok(Json(json!({ "accepted": accepted })))
}

async fn templates(State(state): State<AppState>) -> Json<Vec<Row>> {
    Json(state.board.lock().unwrap().snapshot())
}

fn enqueue(state: &AppState, lines: Vec<String>) -> Result<(), Rejection> {
    if lines.is_empty() {
        return Ok(());
    }
    let cost: usize = lines.iter().map(|line| line.len() + LINE_OVERHEAD).sum();
    if cost > MAX_QUEUED_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "request has too many lines".into(),
        ));
    }
    let busy = || {
        (
            StatusCode::TOO_MANY_REQUESTS,
            "ingest queue is full, retry later".into(),
        )
    };
    let permit = state
        .budget
        .clone()
        .try_acquire_many_owned(cost as u32)
        .map_err(|_| busy())?;
    state
        .tx
        .try_send(Chunk { lines, permit })
        .map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => busy(),
            mpsc::error::TrySendError::Closed(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, "shutting down".into())
            }
        })
}
