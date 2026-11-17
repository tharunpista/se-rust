//! A durable, append-only audit trail of gateway decisions (row 2.5).
//!
//! Lives here rather than in `se-permission` because that crate is a
//! pure decision function with no filesystem access, by design.

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use se_permission::{AuditRecord, AuditSink};

/// Appends one JSON line per decision. Records carry env-var names only
/// (see `AuditRecord`), so nothing here needs redacting.
pub struct FileAuditSink {
    file: Mutex<std::fs::File>,
}

impl FileAuditSink {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }
}

impl AuditSink for FileAuditSink {
    // shortcut: a failed write is dropped, not fatal, because `AuditSink`
    // returns nothing. Make `record` fallible and refuse the call if
    // audit completeness ever becomes a compliance requirement.
    fn record(&self, record: AuditRecord) {
        let line = serde_json::json!({
            "at_unix_ms": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            "session": record.session.to_string(),
            "task": record.task.to_string(),
            "agent": record.agent_id,
            "tool": record.tool_name,
            "resource": record.resource_scope,
            "effect": format!("{:?}", record.effect),
        });
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        // One write per line so a crash can truncate at most the last one.
        let _ = file.write_all(format!("{line}\n").as_bytes());
        let _ = file.flush();
    }
}

/// Reads an audit file back as JSON values, skipping a torn final line.
pub fn read(path: &Path) -> std::io::Result<Vec<serde_json::Value>> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}
