//! Call log.
//!
//! The Kotlin build kept these rows in SQLite because it was inside an app with
//! a database helper already on hand. A CLI has no such constraint, and the
//! records are append-only and small, so a JSON-lines file with a bounded tail
//! is both simpler and easier to inspect with `wbhub calls --json | jq`.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One recorded call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
    /// Epoch milliseconds, matching the Android build's column.
    pub timestamp: i64,
    pub model: String,
    /// `ok` or `failed`.
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub prompt_tokens: i64,
    #[serde(default)]
    pub completion_tokens: i64,
    #[serde(default)]
    pub credits: f64,
    /// Empty for this device: the local path is the assumed one, so a recorded
    /// address always means "someone else ran this".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_ip: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_label: String,
    /// The model's price multiplier, or -1 when it is not listed.
    #[serde(default)]
    pub multiplier: f64,
}

impl CallRecord {
    pub fn failed(timestamp: i64, model: &str, detail: &str) -> CallRecord {
        CallRecord {
            timestamp,
            model: model.to_string(),
            outcome: "failed".into(),
            detail: Some(detail.chars().take(120).collect()),
            prompt_tokens: 0,
            completion_tokens: 0,
            credits: 0.0,
            source_ip: String::new(),
            account_id: String::new(),
            account_label: String::new(),
            multiplier: -1.0,
        }
    }

    pub fn total_tokens(&self) -> i64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// Append-only log with a bounded number of retained rows.
///
/// Cloning is cheap (a path plus a limit) and lets a stream hold its own handle
/// so a client disconnect can still be recorded from `Drop`.
#[derive(Clone)]
pub struct CallLog {
    path: PathBuf,
    limit: usize,
    /// Appends since the last trim, so the common case does not re-read the
    /// whole file just to discover it is still under the limit.
    appends: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl CallLog {
    pub fn open(dir: &Path, limit: usize) -> CallLog {
        let path = dir.join("calls.jsonl");
        CallLog {
            path,
            limit: limit.max(1),
            appends: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one record, then trims the file when it has outgrown the limit.
    ///
    /// A failure to record is never fatal: losing a log line must not take down
    /// a call that already succeeded.
    pub fn append(&self, record: &CallRecord) {
        let line = match serde_json::to_string(record) {
            Ok(line) => line,
            Err(_) => return,
        };
        let write = || -> Result<()> {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .with_context(|| format!("打开 {}", self.path.display()))?;
            writeln!(file, "{line}")?;
            Ok(())
        };
        if write().is_err() {
            return;
        }
        // Re-reading the whole file on every append is O(n) per call for no
        // benefit: only a run that is actually over the limit needs a trim, and
        // the counter says when that can possibly be true.
        let since_trim = self
            .appends
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if since_trim >= self.limit.max(1) {
            self.appends.store(0, std::sync::atomic::Ordering::Relaxed);
            self.trim_if_needed();
        }
    }

    fn trim_if_needed(&self) {
        let all = match self.read_all() {
            Ok(all) => all,
            Err(_) => return,
        };
        if all.len() <= self.limit {
            return;
        }
        let keep = &all[all.len() - self.limit..];
        let mut text = String::new();
        for record in keep {
            if let Ok(line) = serde_json::to_string(record) {
                text.push_str(&line);
                text.push('\n');
            }
        }
        let _ = crate::store::write_private(&self.path, &text);
    }

    /// Every retained record, oldest first.
    pub fn read_all(&self) -> Result<Vec<CallRecord>> {
        let file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(_) => return Ok(Vec::new()),
        };
        let mut records = Vec::new();
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { continue };
            if line.trim().is_empty() {
                continue;
            }
            // A half-written trailing line is skipped rather than failing the
            // whole read, because the file is appended to while it is read.
            if let Ok(record) = serde_json::from_str::<CallRecord>(&line) {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// The newest `count` records, newest first.
    pub fn recent(&self, count: usize) -> Result<Vec<CallRecord>> {
        let mut all = self.read_all()?;
        all.reverse();
        all.truncate(count);
        Ok(all)
    }

    pub fn clear(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("删除 {}", self.path.display())),
        }
    }

    /// Totals across every retained record.
    pub fn summary(&self) -> Result<Summary> {
        let mut summary = Summary::default();
        for record in self.read_all()? {
            summary.calls += 1;
            summary.prompt_tokens += record.prompt_tokens;
            summary.completion_tokens += record.completion_tokens;
            summary.credits += record.credits;
            if record.outcome == "ok" {
                summary.succeeded += 1;
            } else {
                summary.failed += 1;
            }
            *summary.by_model.entry(record.model.clone()).or_insert(0) += 1;
        }
        Ok(summary)
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub calls: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub credits: f64,
    pub by_model: std::collections::BTreeMap<String, u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wbhub-calls-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record(model: &str, ok: bool) -> CallRecord {
        let mut record = CallRecord::failed(1, model, "x");
        if ok {
            record.outcome = "ok".into();
            record.detail = None;
            record.prompt_tokens = 10;
            record.completion_tokens = 20;
            record.credits = 0.5;
        }
        record
    }

    #[test]
    fn records_round_trip_through_the_file() {
        let dir = scratch("roundtrip");
        let log = CallLog::open(&dir, 100);
        log.append(&record("hy3", true));
        log.append(&record("glm-5.2", false));
        let all = log.read_all().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].model, "glm-5.2");
        assert_eq!(all[0].total_tokens(), 30);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_log_is_trimmed_to_its_limit() {
        let dir = scratch("trim");
        let log = CallLog::open(&dir, 3);
        // Three writes reach the limit, so a trim runs on the third; the next
        // three fill it again before the following trim.
        for index in 0..9 {
            log.append(&record(&format!("m{index}"), true));
        }
        let all = log.read_all().unwrap();
        assert!(all.len() <= 3, "retained {} rows", all.len());
        assert_eq!(
            all.last().unwrap().model,
            "m8",
            "the newest row must always survive"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_returns_newest_first() {
        let dir = scratch("recent");
        let log = CallLog::open(&dir, 100);
        for index in 0..4 {
            log.append(&record(&format!("m{index}"), true));
        }
        let recent = log.recent(2).unwrap();
        assert_eq!(recent[0].model, "m3");
        assert_eq!(recent[1].model, "m2");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_line_does_not_break_the_read() {
        let dir = scratch("corrupt");
        let log = CallLog::open(&dir, 100);
        log.append(&record("hy3", true));
        let mut text = fs::read_to_string(log.path()).unwrap();
        text.push_str("{not json at all\n");
        fs::write(log.path(), text).unwrap();
        assert_eq!(log.read_all().unwrap().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn summary_aggregates_by_outcome_and_model() {
        let dir = scratch("summary");
        let log = CallLog::open(&dir, 100);
        log.append(&record("hy3", true));
        log.append(&record("hy3", true));
        log.append(&record("glm-5.2", false));
        let summary = log.summary().unwrap();
        assert_eq!(summary.calls, 3);
        assert_eq!(summary.succeeded, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.credits, 1.0);
        assert_eq!(summary.by_model["hy3"], 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_an_absent_log_is_not_an_error() {
        let dir = scratch("clear");
        let log = CallLog::open(&dir, 100);
        assert!(log.clear().is_ok());
        log.append(&record("hy3", true));
        assert!(log.clear().is_ok());
        assert!(log.read_all().unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
