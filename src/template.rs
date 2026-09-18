use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use drain3_rust::{Drain, UpdateType};
use regex::{Captures, Regex};

static MASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?P<STATUS>(?P<PREFIX>\b(?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+\S+(?:\s+HTTP/[0-9](?:\.[0-9])?\x22?)?\s+|\b(?i:status(?:[ _]?code)?|http)[\s:=]+)(?P<CODE>[1-5])[0-9]{2}\b)",
        r"|(?P<UUID>\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b)",
        r"|(?P<EMAIL>[\w.+-]+@[\w-]+\.[\w.-]+)",
        r"|(?P<IP>\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b)",
        r"|(?P<HEX>\b0x[0-9a-fA-F]+\b)",
        r"|(?P<NUM>\b\d+\b)",
    ))
    .expect("mask regex")
});

const MASKS: [&str; 5] = ["UUID", "EMAIL", "IP", "HEX", "NUM"];

const MAX_CHILDREN: usize = 100;
const WILDCARD: &str = "<*>";
const PARAMETRIZE_NUMERIC: bool = true;

static STATUS_CLASS: LazyLock<Regex> = LazyLock::new(|| Regex::new("<([1-5])xx>").unwrap());
const STATUS_NAMES: [&str; 5] = ["info", "success", "redirect", "clienterror", "servererror"];

fn status_key(masked: &str) -> Option<String> {
    let names: Vec<&str> = STATUS_CLASS
        .captures_iter(masked)
        .map(|c| STATUS_NAMES[(c[1].as_bytes()[0] - b'1') as usize])
        .collect();
    (!names.is_empty()).then(|| format!("http-{}", names.join("+")))
}

pub(crate) fn mask(line: &str) -> String {
    MASK.replace_all(line, |caps: &Captures| {
        if caps.name("STATUS").is_some() {
            return format!("{}<{}xx>", mask(&caps["PREFIX"]), &caps["CODE"]);
        }
        let name = MASKS
            .iter()
            .find(|m| caps.name(m).is_some())
            .unwrap_or(&"NUM");
        format!("<{name}>")
    })
    .into_owned()
}

#[derive(Debug, Clone)]
pub(crate) struct Assignment {
    pub cluster: usize,
    pub template: Arc<str>,
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub depth: usize,
    pub similarity: f64,
    pub max_clusters: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            depth: 4,
            similarity: 0.7,
            max_clusters: 100_000,
        }
    }
}

pub(crate) struct Templater {
    drain: Drain,
    templates: HashMap<usize, Arc<str>>,
    max_clusters: usize,
}

impl Templater {
    pub fn new(config: Config) -> Self {
        Self {
            drain: Drain::new(
                config.depth,
                config.similarity,
                MAX_CHILDREN,
                Some(config.max_clusters),
                vec![],
                WILDCARD.into(),
                PARAMETRIZE_NUMERIC,
            ),
            templates: HashMap::new(),
            max_clusters: config.max_clusters,
        }
    }

    pub(crate) fn assign(&mut self, line: &str) -> Assignment {
        let masked = mask(line);
        let key = status_key(&masked);
        let (cluster, update) = match &key {
            Some(key) => self.drain.add_log_message(&format!("{key} {masked}")),
            None => self.drain.add_log_message(&masked),
        };
        let id = cluster.cluster_id;
        if matches!(update, UpdateType::None) {
            if let Some(template) = self.templates.get(&id) {
                return Assignment {
                    cluster: id,
                    template: template.clone(),
                };
            }
        }
        if self.templates.len() >= self.max_clusters * 2 {
            self.templates.clear();
        }
        let template = cluster.get_template();
        let template: Arc<str> = match &key {
            Some(key) => template
                .strip_prefix(key.as_str())
                .unwrap_or(&template)
                .trim_start()
                .into(),
            None => template.into(),
        };
        self.templates.insert(id, template.clone());
        Assignment {
            cluster: id,
            template,
        }
    }

    pub fn clusters(&self) -> usize {
        self.drain.cluster_count()
    }
}

impl Default for Templater {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_variable_values() {
        assert_eq!(
            mask(
                "user a@b.io from 10.0.0.12 id 0x1f req 5f0c2a9e-7d4b-4c1a-9f2e-3b6a8c1d2e4f took 42 ms"
            ),
            "user <EMAIL> from <IP> id <HEX> req <UUID> took <NUM> ms"
        );
    }

    #[test]
    fn keeps_http_status_classes() {
        assert_eq!(mask("GET /checkout 500"), "GET /checkout <5xx>");
        assert_eq!(
            mask("GET /items/42 200 12 ms"),
            "GET /items/<NUM> <2xx> <NUM> ms"
        );
        assert_eq!(
            mask(r#""POST /v1/pay HTTP/1.1" 404 512"#),
            r#""POST /v1/pay HTTP/<NUM>.<NUM>" <4xx> <NUM>"#
        );
        assert_eq!(mask("upstream status=503"), "upstream status=<5xx>");
        assert_eq!(mask("wrote 500 records"), "wrote <NUM> records");
        assert_eq!(mask("GET /checkout 5١٢"), "GET /checkout <NUM>");
    }

    #[test]
    fn never_merges_different_status_classes() {
        let mut t = Templater::default();
        let ok = t.assign("GET /checkout 200 12 ms");
        let failed = t.assign("GET /checkout 500 12 ms");
        let failed_again = t.assign("GET /checkout 503 40 ms");
        assert_ne!(ok.cluster, failed.cluster);
        assert_eq!(failed.cluster, failed_again.cluster);
        assert_eq!(&*ok.template, "GET /checkout <2xx> <NUM> ms");
        assert_eq!(&*failed_again.template, "GET /checkout <5xx> <NUM> ms");
    }

    #[test]
    fn collapses_repeated_patterns() {
        let mut t = Templater::default();
        let a = t.assign("connection to alpha failed after 3 retries");
        let b = t.assign("connection to beta failed after 7 retries");
        let c = t.assign("GET /health 200");
        assert_eq!(a.cluster, b.cluster);
        assert_ne!(a.cluster, c.cluster);
        assert_eq!(&*b.template, "connection to <*> failed after <NUM> retries");
        assert_eq!(t.clusters(), 2);
    }

    #[test]
    fn unchanged_lines_share_the_template_allocation() {
        let mut t = Templater::default();
        t.assign("worker is ready on shard alpha");
        let b = t.assign("worker is ready on shard beta");
        let c = t.assign("worker is ready on shard gamma");
        assert!(Arc::ptr_eq(&b.template, &c.template));
    }
}
