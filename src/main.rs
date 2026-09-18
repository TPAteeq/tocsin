use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, ErrorKind, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use tokio::sync::mpsc;

use tocsin::cache::Cache;
use tocsin::jev::{DEFAULT_ENDPOINT, DEFAULT_POLICY, Jev};
use tocsin::judge::{Judge, Rules};
use tocsin::pipeline::{Pipeline, next_batch, write_jsonl};
use tocsin::route::{Route, Thresholds};
use tocsin::{NAME, eval, ingest, serve, template};

const READ_AHEAD: usize = 65_536;
const BATCH_LINES: usize = 8_192;
const BATCH_LINGER: Duration = Duration::from_millis(250);

#[derive(Parser)]
#[command(
    name = NAME,
    version,
    about = "Log triage at ingest: page only on logs that matter."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    #[command(flatten)]
    common: Common,
}

#[derive(Args)]
struct Common {
    /// TypeSafe API key. Environment only, so it stays out of the process list.
    #[arg(
        long,
        env = "TYPESAFE_API_KEY",
        hide = true,
        hide_env_values = true,
        global = true
    )]
    api_key: Option<String>,
    /// Jev model to use.
    #[arg(
        long,
        env = "TOCSIN_MODEL",
        default_value = "jev-latest",
        global = true
    )]
    model: String,
    #[arg(long, env = "TYPESAFE_ENDPOINT", default_value = DEFAULT_ENDPOINT, global = true)]
    endpoint: String,
    /// Judge with local keyword rules instead of Jev.
    #[arg(long, global = true)]
    offline: bool,
    /// Parallel judge requests.
    #[arg(long, default_value_t = 32, global = true)]
    concurrency: usize,
    /// Verdict cache file, keyed by template.
    #[arg(long, default_value = ".tocsin/verdicts.json", global = true)]
    cache: PathBuf,
    #[arg(long, global = true)]
    no_cache: bool,
    /// Minimum attention score to page.
    #[arg(long, default_value_t = 0.75, global = true)]
    page_threshold: f32,
    /// Minimum attention score to open a ticket.
    #[arg(long, default_value_t = 0.5, global = true)]
    ticket_threshold: f32,
    /// Paging policy in plain English or JSON. Defaults to policies/default.json.
    #[arg(long, env = "TOCSIN_POLICY", global = true)]
    policy: Option<PathBuf>,
    /// Drain prefix-tree depth.
    #[arg(long, default_value_t = 4, global = true)]
    depth: usize,
    /// Drain similarity needed to merge a line into a template.
    #[arg(long, default_value_t = 0.7, global = true)]
    similarity: f64,
}

#[derive(Subcommand)]
enum Command {
    /// Triage log lines from a file or stdin and print JSON lines.
    Triage {
        file: Option<PathBuf>,
        /// Routes to print.
        #[arg(long, value_delimiter = ',', default_value = "page,ticket")]
        only: Vec<Route>,
    },
    /// Accept OTLP/HTTP JSON on /v1/logs and text or NDJSON on /ingest.
    Serve {
        #[arg(long, default_value = "127.0.0.1:4318")]
        listen: SocketAddr,
        /// Webhook for page alerts (Slack-compatible payload).
        #[arg(long, env = "TOCSIN_WEBHOOK", hide_env_values = true)]
        webhook: Option<String>,
        /// Minimum seconds between alerts for the same template.
        #[arg(long, default_value_t = 600)]
        cooldown_secs: u64,
        /// Routes to print to stdout.
        #[arg(long, value_delimiter = ',', default_value = "page,ticket")]
        only: Vec<Route>,
    },
    /// Score against labeled logs: one `<0|1>\t<line>` per line.
    Eval {
        file: PathBuf,
        #[arg(long)]
        limit: Option<usize>,
        /// Write the full report as JSON.
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 0.042)]
        usd_per_mtok: f64,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match dispatch(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    if cli.common.offline {
        return run(Rules, cli).await;
    }
    let key = cli
        .common
        .api_key
        .clone()
        .context("set TYPESAFE_API_KEY, or pass --offline to use keyword rules")?;
    let policy = match &cli.common.policy {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("reading policy {}", path.display()))?,
        None => DEFAULT_POLICY.to_string(),
    };
    let jev = Jev::new(key, cli.common.model.clone(), &cli.common.endpoint, &policy);
    run(jev, cli).await
}

async fn run<J: Judge>(judge: J, cli: Cli) -> anyhow::Result<()> {
    let args = cli.common;
    let cache = if args.no_cache {
        Cache::memory(judge.fingerprint())
    } else {
        Cache::open(Some(args.cache), judge.fingerprint())?
    };
    let thresholds = Thresholds {
        page: args.page_threshold,
        ticket: args.ticket_threshold,
    };
    let templates = template::Config {
        depth: args.depth,
        similarity: args.similarity,
        ..Default::default()
    };
    let mut pipeline = Pipeline::new(judge, cache, thresholds, templates, args.concurrency);

    match cli.command {
        Command::Triage { file, only } => triage(&mut pipeline, file, &only).await,
        Command::Serve {
            listen,
            webhook,
            cooldown_secs,
            only,
        } => {
            let options = serve::Options {
                listen,
                webhook,
                cooldown: Duration::from_secs(cooldown_secs),
                emit: only,
            };
            serve::run(pipeline, options, serve::os_signals()).await
        }
        Command::Eval {
            file,
            limit,
            out,
            usd_per_mtok,
        } => {
            let report = eval::run(&mut pipeline, &file, limit, usd_per_mtok).await?;
            if let Some(out) = out {
                std::fs::write(&out, serde_json::to_vec_pretty(&report)?)
                    .with_context(|| format!("writing {}", out.display()))?;
            }
            print!("{}", report.markdown());
            Ok(())
        }
    }
}

async fn triage<J: Judge>(
    pipeline: &mut Pipeline<J>,
    file: Option<PathBuf>,
    only: &[Route],
) -> anyhow::Result<()> {
    let reader: Box<dyn BufRead + Send> = match file {
        Some(path) => Box::new(BufReader::new(
            File::open(&path).with_context(|| format!("opening {}", path.display()))?,
        )),
        None => Box::new(BufReader::new(std::io::stdin())),
    };
    let (tx, mut rx) = mpsc::channel(READ_AHEAD);
    let input = std::thread::spawn(move || read_lines(reader, tx));

    let mut out = BufWriter::new(std::io::stdout());
    while let Some(batch) = next_batch(&mut rx, BATCH_LINES, BATCH_LINGER).await {
        let triaged = pipeline.process(&batch).await;
        let written = write_jsonl(&mut out, &batch, &triaged, only).and_then(|()| out.flush());
        match written {
            Err(err) if err.kind() == ErrorKind::BrokenPipe => return pipeline.save(),
            written => written?,
        }
        pipeline.checkpoint()?;
    }
    pipeline.save()?;
    input
        .join()
        .expect("input reader panicked")
        .context("reading input")?;

    let stats = pipeline.stats();
    eprintln!(
        "{NAME}: {} lines → {} templates → {} judged ({} input tokens, {} fallbacks)",
        stats.lines,
        pipeline.clusters(),
        stats.judged,
        stats.input_tokens,
        stats.fallbacks
    );
    Ok(())
}

fn read_lines(
    mut reader: Box<dyn BufRead + Send>,
    tx: mpsc::Sender<String>,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            return Ok(());
        }
        if let Some(line) = ingest::normalize(&String::from_utf8_lossy(&buf)) {
            if tx.blocking_send(line).is_err() {
                return Ok(());
            }
        }
    }
}
