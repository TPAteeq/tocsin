use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::judge::Verdict;

const MAX_ENTRIES: usize = 250_000;

pub struct Cache {
    path: Option<PathBuf>,
    fingerprint: String,
    model: Option<String>,
    verdicts: HashMap<Arc<str>, Arc<Verdict>>,
    dirty: bool,
}

#[derive(Serialize, Deserialize)]
struct Snapshot<V> {
    fingerprint: String,
    #[serde(default)]
    model: Option<String>,
    verdicts: V,
}

impl Cache {
    pub fn open(path: Option<PathBuf>, fingerprint: String) -> anyhow::Result<Self> {
        let mut cache = Self::memory(fingerprint);
        if let Some(existing) = path.as_ref().filter(|p| p.exists()) {
            let context = || {
                format!(
                    "reading cache {} (delete it or pass --no-cache)",
                    existing.display()
                )
            };
            let snapshot: Snapshot<serde_json::Value> =
                serde_json::from_slice(&fs::read(existing)?).with_context(context)?;
            if snapshot.fingerprint == cache.fingerprint {
                let stored: HashMap<String, Verdict> =
                    serde_json::from_value(snapshot.verdicts).with_context(context)?;
                cache.verdicts = stored
                    .into_iter()
                    .filter(|(_, v)| v.is_valid())
                    .map(|(k, v)| (k.into(), Arc::new(v)))
                    .collect();
                cache.model = snapshot.model;
            }
        }
        cache.path = path;
        Ok(cache)
    }

    pub fn memory(fingerprint: String) -> Self {
        Self {
            path: None,
            fingerprint,
            model: None,
            verdicts: HashMap::new(),
            dirty: false,
        }
    }

    pub fn get(&self, template: &str) -> Option<&Arc<Verdict>> {
        self.verdicts.get(template)
    }

    pub fn insert(&mut self, template: Arc<str>, verdict: Verdict, model: &str) -> Arc<Verdict> {
        if self.model.as_deref() != Some(model) {
            if self.model.is_some() {
                self.verdicts.clear();
            }
            self.model = Some(model.to_string());
        }
        if self.verdicts.len() >= MAX_ENTRIES {
            self.verdicts.clear();
        }
        let verdict = Arc::new(verdict);
        self.verdicts.insert(template, verdict.clone());
        self.dirty = true;
        verdict
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn len(&self) -> usize {
        self.verdicts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.verdicts.is_empty()
    }

    pub fn save(&mut self) -> anyhow::Result<()> {
        let Some(path) = self.path.as_ref().filter(|_| self.dirty) else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let snapshot = Snapshot {
            fingerprint: self.fingerprint.clone(),
            model: self.model.clone(),
            verdicts: self
                .verdicts
                .iter()
                .map(|(k, v)| (&**k, &**v))
                .collect::<BTreeMap<_, _>>(),
        };
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&tmp, serde_json::to_vec_pretty(&snapshot)?)?;
        fs::rename(&tmp, path)?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict() -> Verdict {
        Verdict {
            pageable: 0.9,
            detail: 0.1,
            severity: 2.5,
            area: "storage".into(),
        }
    }

    #[test]
    fn round_trips_and_invalidates_on_fingerprint_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("verdicts.json");
        let mut cache = Cache::open(Some(path.clone()), "a".into()).unwrap();
        cache.insert("disk <*> is full".into(), verdict(), "jev-1");
        cache.save().unwrap();

        let reopened = Cache::open(Some(path.clone()), "a".into()).unwrap();
        assert_eq!(
            reopened.get("disk <*> is full").map(|v| v.as_ref()),
            Some(&verdict())
        );
        assert_eq!(reopened.model(), Some("jev-1"));
        assert!(
            Cache::open(Some(path.clone()), "b".into())
                .unwrap()
                .is_empty()
        );

        let mut upgraded = Cache::open(Some(path.clone()), "a".into()).unwrap();
        upgraded.insert("disk <*> is gone".into(), verdict(), "jev-2");
        assert!(upgraded.get("disk <*> is full").is_none());
        assert_eq!(upgraded.model(), Some("jev-2"));

        fs::write(
            &path,
            r#"{"fingerprint":"old","verdicts":{"x":{"legacy":true}}}"#,
        )
        .unwrap();
        assert!(Cache::open(Some(path), "a".into()).unwrap().is_empty());
    }
}
