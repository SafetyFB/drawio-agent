//! Per-file task history (R5): every ask is appended as one JSON line to
//! `<doc>.history.jsonl` next to the diagram — full trajectory (engine
//! events), usage/cost, final reply, and the resulting xml snapshot, so a
//! past state can be inspected and restored. `ctx-save`/`ctx-load` (CLI)
//! and the history panel export/import (web) move whole sessions as JSON.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRec {
    /// unix seconds
    pub ts: u64,
    pub user: String,
    #[serde(default)]
    pub reply: String,
    #[serde(default)]
    pub tool_calls: u32,
    #[serde(default)]
    pub usage_in: u64,
    #[serde(default)]
    pub usage_out: u64,
    #[serde(default)]
    pub cost_yuan: f64,
    /// Engine trajectory events (same JSON lines the UI streams live).
    #[serde(default)]
    pub events: Vec<serde_json::Value>,
    /// Canonical xml right after this ask.
    pub xml: String,
    /// Optional error text when the ask failed.
    #[serde(default)]
    pub error: Option<String>,
}

impl HistoryRec {
    pub fn summary(&self) -> String {
        let t = std::time::Duration::from_secs(self.ts);
        let dt = chrono_lite(t);
        format!(
            "#{} {} | {} | in {} / out {} {}",
            dt,
            one_line(&self.user, 60),
            self.tool_calls,
            self.usage_in,
            self.usage_out,
            if self.cost_yuan > 0.0 {
                format!("≈ ¥{:.4}", self.cost_yuan)
            } else {
                String::new()
            }
        )
    }
}

fn one_line(s: &str, n: usize) -> String {
    let t: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let t: String = t.chars().take(n).collect();
    if t.chars().count() < s.chars().count() {
        format!("{t}…")
    } else {
        t
    }
}

/// Minimal local-time formatter (no chrono dep): YYYY-MM-DD HH:MM:SS.
fn chrono_lite(secs: std::time::Duration) -> String {
    let s = secs.as_secs() as i64;
    let days = s.div_euclid(86400);
    let rem = s.rem_euclid(86400);
    // days since epoch → civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// `<docdir>/<stem>.history.jsonl`
pub fn history_path(doc_path: &Path) -> PathBuf {
    let stem = doc_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "diagram".into());
    doc_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{stem}.history.jsonl"))
}

pub fn append(path: &Path, rec: &HistoryRec) -> Result<(), String> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open history: {e}"))?;
    let line = serde_json::to_string(rec).map_err(|e| format!("ser: {e}"))?;
    writeln!(f, "{line}").map_err(|e| format!("write history: {e}"))
}

/// Newest first, best-effort skip of corrupt lines.
pub fn list(path: &Path, limit: usize) -> Vec<HistoryRec> {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut recs: Vec<HistoryRec> = raw
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    recs.reverse();
    recs.truncate(limit);
    recs
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whole-session context bundle for save/load (R5): transcript + doc + usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionBundle {
    pub version: u32,
    pub file: String,
    pub saved_at: u64,
    /// Multi-turn memory transcript (text parts only; images elided).
    pub messages: Vec<crate::chat::Message>,
    pub usage_in: u64,
    pub usage_out: u64,
    pub cost_yuan: f64,
    /// Canonical xml at save time.
    pub xml: String,
}

impl SessionBundle {
    /// Export messages with image parts elided (they never belong in a
    /// durable transcript — re-view when needed).
    pub fn strip_images(msgs: &[crate::chat::Message]) -> Vec<crate::chat::Message> {
        msgs.iter()
            .map(|m| crate::chat::Message {
                role: m.role.clone(),
                parts: m
                    .parts
                    .iter()
                    .map(|p| match p {
                        crate::chat::Part::Text(t) => crate::chat::Part::Text(t.clone()),
                        crate::chat::Part::ImagePng(_) => {
                            crate::chat::Part::Text("（截图已省略）".into())
                        }
                    })
                    .collect(),
            })
            .collect()
    }
}
