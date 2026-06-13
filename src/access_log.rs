use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::debug;

const DEFAULT_HISTORY_PATH: &str = "access_history.json";

pub fn service_key(mac: &str, local_port: u16) -> String {
    format!("{}-{}", mac, local_port)
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn access_log_path() -> PathBuf {
    let path = if let Ok(p) = env::var("WAKEZILLA__STORAGE__ACCESS_HISTORY_PATH") {
        PathBuf::from(p)
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(DEFAULT_HISTORY_PATH)
    };
    if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessLog {
    #[serde(flatten)]
    inner: HashMap<String, VecDeque<i64>>,
    #[serde(skip)]
    max_records: usize,
}

impl AccessLog {
    pub fn new(max_records: usize) -> Self {
        Self {
            inner: HashMap::new(),
            max_records,
        }
    }

    pub fn record(&mut self, key: &str, ts: i64) {
        if self.max_records == 0 {
            return;
        }
        let buf = self.inner.entry(key.to_string()).or_default();
        buf.push_back(ts);
        while buf.len() > self.max_records {
            buf.pop_front();
        }
    }

    pub fn get(&self, key: &str) -> Vec<i64> {
        self.inner
            .get(key)
            .map(|b| b.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn load(max_records: usize) -> Self {
        let path = access_log_path();
        let mut log = match fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
                tracing::warn!("Failed to parse access history at {}: {e}", path.display());
                Self::new(max_records)
            }),
            Err(_) => Self::new(max_records),
        };
        log.max_records = max_records;
        log
    }

    pub fn save(&self) -> Result<()> {
        let path = access_log_path();
        let data = serde_json::to_string(self).context("Failed to serialize access history")?;
        fs::write(&path, data)
            .with_context(|| format!("Failed to write access history to {}", path.display()))?;
        debug!("Saved access history to {}", path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_appends_timestamps() {
        let mut log = AccessLog::new(2000);
        log.record("aa-80", 1);
        log.record("aa-80", 2);
        assert_eq!(log.get("aa-80"), vec![1, 2]);
    }

    #[test]
    fn record_caps_at_1000_dropping_oldest() {
        let mut log = AccessLog::new(1000);
        for i in 0..1100 {
            log.record("aa-80", i);
        }
        let got = log.get("aa-80");
        assert_eq!(got.len(), 1000);
        assert_eq!(got.first(), Some(&100));
        assert_eq!(got.last(), Some(&1099));
    }

    #[test]
    fn record_respects_configured_cap() {
        let mut log = AccessLog::new(3);
        for i in 0..10 {
            log.record("k", i);
        }
        let got = log.get("k");
        assert_eq!(got.len(), 3);
        assert_eq!(got, vec![7, 8, 9]);
    }

    #[test]
    fn get_missing_key_is_empty() {
        let log = AccessLog::new(2000);
        assert!(log.get("nope").is_empty());
    }

    #[test]
    fn record_disabled_when_cap_zero() {
        let mut log = AccessLog::new(0);
        log.record("k", 1);
        log.record("k", 2);
        assert!(log.get("k").is_empty());
    }

    #[test]
    fn service_key_format() {
        assert_eq!(service_key("AA:BB", 1234), "AA:BB-1234");
    }
}
