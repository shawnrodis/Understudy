//! Append-only local action log (JSON lines).
//!
//! Every control-API action — including rejected/unauthorized attempts — is
//! appended to `actions.log` in the data directory. The Flutter UI's log
//! view reads this file through `GET /log`. Nothing is ever sent anywhere
//! else; the log lives only on the controlled PC.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::json::{escape, Value};

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append one action entry. `detail` is free-form JSON-safe text (paths,
/// commands, coordinates — whatever identifies the action).
pub fn append(data_dir: &Path, action: &str, ok: bool, detail: &str) {
    let _ = fs::create_dir_all(data_dir);
    let path = data_dir.join("actions.log");
    let line = format!(
        "{{\"ts\":{},\"action\":\"{}\",\"ok\":{},\"detail\":\"{}\"}}\n",
        now_unix(),
        escape(action),
        ok,
        escape(detail)
    );
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Read the last `limit` entries (newest last). Malformed lines are skipped.
pub fn read_last(data_dir: &Path, limit: usize) -> Vec<Value> {
    let path = data_dir.join("actions.log");
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(limit);
    lines[start..]
        .iter()
        .filter_map(|l| crate::json::parse(l).ok())
        .collect()
}
