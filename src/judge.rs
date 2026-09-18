use std::future::Future;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub pageable: f32,
    pub detail: f32,
    pub severity: f32,
    pub area: String,
}

impl Verdict {
    pub fn attention(&self) -> f32 {
        self.pageable * (1.0 - self.detail)
    }

    pub fn is_valid(&self) -> bool {
        (0.0..=1.0).contains(&self.pageable)
            && (0.0..=1.0).contains(&self.detail)
            && (0.0..=3.0).contains(&self.severity)
    }
}

#[derive(Debug, Clone)]
pub struct Judged {
    pub verdict: Verdict,
    pub input_tokens: u64,
    pub model: String,
}

pub trait Judge: Send + Sync + 'static {
    fn fingerprint(&self) -> String;
    fn judge(&self, template: &str) -> impl Future<Output = anyhow::Result<Judged>> + Send;

    /// Used when a judgement fails, so a batch always routes.
    fn fallback(&self, template: &str) -> Verdict {
        Rules::verdict(template)
    }
}

pub struct Rules;

static CRITICAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(fatal|panic|critical|emerg\w*|segfault|segmentation fault|out of memory|oom|corrupt\w*|data loss)\b").unwrap()
});
static ERROR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(err|error|errors|fail|failed|failure|failures|exception|refused|denied|unreachable|abort\w*|timed? ?out|unavailable|crash\w*)\b").unwrap()
});
static WARNING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(warn|warning|retry\w*|degraded|slow|deprecated)\b").unwrap()
});
static AREAS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("security", r"(?i)\b(auth\w*|login|password|permission|unauthori[sz]ed|forbidden|certificate|ssh|sudo)\b"),
        ("storage", r"(?i)\b(disk|filesystem|volume|mount|nfs|lustre|i/o|database|replica\w*)\b"),
        ("network", r"(?i)\b(network|connection|socket|dns|tcp|udp|http|link|unreachable|timed? ?out)\b"),
        ("hardware", r"(?i)\b(cpu|memory|ecc|parity|fan|temperature|power|hardware|node card|interconnect)\b"),
        ("operating_system", r"(?i)\b(kernel|driver|process|daemon|systemd|cron\w*|pid)\b"),
    ]
    .into_iter()
    .map(|(area, pattern)| (area, Regex::new(pattern).unwrap()))
    .collect()
});

impl Rules {
    pub fn level(text: &str) -> (f32, f32) {
        if CRITICAL.is_match(text) {
            (0.9, 3.0)
        } else if ERROR.is_match(text) {
            (0.7, 2.0)
        } else if WARNING.is_match(text) {
            (0.3, 1.0)
        } else {
            (0.05, 0.0)
        }
    }

    pub fn verdict(text: &str) -> Verdict {
        let (pageable, severity) = Self::level(text);
        let area = AREAS
            .iter()
            .find(|(_, re)| re.is_match(text))
            .map_or("application", |(area, _)| area);
        Verdict {
            pageable,
            detail: 0.0,
            severity,
            area: area.to_string(),
        }
    }
}

impl Judge for Rules {
    fn fingerprint(&self) -> String {
        "rules-v1".into()
    }

    async fn judge(&self, template: &str) -> anyhow::Result<Judged> {
        Ok(Judged {
            verdict: Self::verdict(template),
            input_tokens: 0,
            model: "rules".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_rank_by_keyword_severity() {
        assert_eq!(Rules::verdict("kernel panic - not syncing").severity, 3.0);
        assert_eq!(Rules::verdict("connection to <*> failed").pageable, 0.7);
        assert_eq!(Rules::verdict("retrying request <NUM>").pageable, 0.3);
        assert_eq!(Rules::verdict("GET /health <NUM>").pageable, 0.05);
    }

    #[test]
    fn rules_pick_an_area() {
        assert_eq!(Rules::verdict("ssh login failed for root").area, "security");
        assert_eq!(Rules::verdict("disk <*> is full").area, "storage");
        assert_eq!(Rules::verdict("order <NUM> shipped").area, "application");
    }
}
