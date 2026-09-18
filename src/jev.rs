use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::judge::{Judge, Judged, Verdict};

pub const DEFAULT_ENDPOINT: &str = "https://api.typesafe.ai";
pub const DEFAULT_POLICY: &str = include_str!("../policies/default.json");
const MAX_ATTEMPTS: u32 = 6;

static QUESTIONS: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "pageable": {
            "type": "noul",
            "instructions": {
                "question": "Does `policy` say to page an on-call engineer for the event reported in `log.template`?",
                "focus": "Judge the event the message describes. Placeholders such as <*> or <NUM> stand for values that vary between occurrences."
            },
            "criteria": {
                "true": "The event matches something in `policy.page` and nothing in `policy.do_not_page`.",
                "false": "The event matches `policy.do_not_page`, or matches nothing in `policy.page`."
            }
        },
        "detail": {
            "type": "noul",
            "instructions": {
                "question": "Is `log.template` supporting detail that belongs to another log message, rather than a report of an event on its own?",
                "focus": "Multi-line output such as stack traces and dumps spreads one event across many lines; only the headline line reports the event."
            },
            "criteria": {
                "true": {
                    "what": "A continuation of another message: a stack frame, register or memory dump, field or value listing, or one line of a multi-line report.",
                    "examples": ["at com.example.Worker.run(Worker.java:<NUM>)", "rax: <HEX> rbx: <HEX> rcx: <HEX>", "... <NUM> more"]
                },
                "false": {
                    "what": "A complete message that reports an event, state change, or result by itself.",
                    "examples": ["NullPointerException in request handler", "worker <NUM> exited with status <NUM>", "GET /health <NUM> <NUM>ms"]
                }
            }
        },
        "severity": {
            "type": "score",
            "instructions": "How severe is the condition described in `log.template`?",
            "criteria": [
                "Informational: normal operation, nothing is wrong.",
                "Warning: unusual or degraded, but the system continues or recovers on its own.",
                "Error: a component or operation failed and needs investigation.",
                "Critical: outage, crash, data loss, hardware fault, or security breach needing immediate action."
            ]
        },
        "area": {
            "type": "choice",
            "instructions": "Which part of the system does `log.template` concern?",
            "criteria": {
                "hardware": "Physical devices: CPU, memory, disks, power, fans, interconnects.",
                "operating_system": "Kernel, drivers, processes, scheduling, system services.",
                "network": "Connectivity, DNS, sockets, load balancers, remote endpoints.",
                "storage": "Filesystems, volumes, databases, replication, backups.",
                "security": "Authentication, authorization, certificates, intrusion attempts.",
                "application": "Business logic, services, jobs, and their requests.",
                "other": null
            }
        }
    })
});

pub struct Jev {
    http: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
    policy: Value,
}

impl Jev {
    pub fn new(api_key: String, model: String, endpoint: &str, policy: &str) -> Self {
        Self {
            http: crate::http_client(),
            url: format!("{}/v1/systemone", endpoint.trim_end_matches('/')),
            api_key,
            model,
            policy: serde_json::from_str(policy)
                .unwrap_or_else(|_| Value::String(policy.trim().to_string())),
        }
    }

    async fn post(&self, body: &Value) -> anyhow::Result<Response> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let request = self
                .http
                .post(&self.url)
                .bearer_auth(&self.api_key)
                .json(body);
            let delay = match request.send().await {
                Ok(res) if res.status().is_success() => {
                    return res.json().await.context("decoding TypeSafe response");
                }
                Ok(res) if retryable(res.status()) && attempt < MAX_ATTEMPTS => {
                    retry_after(&res).unwrap_or_else(|| backoff(attempt))
                }
                Ok(res) => {
                    let status = res.status();
                    bail!(
                        "TypeSafe returned {status}: {}",
                        res.text().await.unwrap_or_default()
                    );
                }
                Err(err) if attempt < MAX_ATTEMPTS && !err.is_builder() => backoff(attempt),
                Err(err) => return Err(err).context("calling TypeSafe"),
            };
            tokio::time::sleep(delay).await;
        }
    }
}

impl Judge for Jev {
    fn fingerprint(&self) -> String {
        let inputs = format!("{}{}{}", self.url, *QUESTIONS, self.policy);
        format!("{}:{:016x}", self.model, fnv1a(inputs.as_bytes()))
    }

    async fn judge(&self, template: &str) -> anyhow::Result<Judged> {
        let body = json!({
            "model": self.model,
            "state": { "policy": self.policy, "log": { "template": template } },
            "questions": &*QUESTIONS,
        });
        let res = self.post(&body).await?;
        let verdict = Verdict {
            pageable: res.answers.pageable.noul,
            detail: res.answers.detail.noul,
            severity: res.answers.severity.score,
            area: res.answers.area.choice,
        };
        anyhow::ensure!(
            verdict.is_valid(),
            "TypeSafe returned an out-of-range verdict: {verdict:?}"
        );
        Ok(Judged {
            verdict,
            input_tokens: res.usage.input_tokens,
            model: res.model,
        })
    }
}

#[derive(Deserialize)]
struct Response {
    model: String,
    answers: Answers,
    usage: Usage,
}

#[derive(Deserialize)]
struct Answers {
    pageable: Noul,
    detail: Noul,
    severity: Score,
    area: Choice,
}

#[derive(Deserialize)]
struct Noul {
    noul: f32,
}

#[derive(Deserialize)]
struct Score {
    score: f32,
}

#[derive(Deserialize)]
struct Choice {
    choice: String,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: u64,
}

fn retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

fn retry_after(res: &reqwest::Response) -> Option<Duration> {
    let secs: f64 = res
        .headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .parse()
        .ok()?;
    secs.is_finite()
        .then(|| Duration::from_secs_f64(secs.clamp(0.0, 60.0)))
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(250 * 2u64.pow(attempt.min(6))).min(Duration::from_secs(10))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, b| {
        (hash ^ *b as u64).wrapping_mul(0x100000001b3)
    })
}
