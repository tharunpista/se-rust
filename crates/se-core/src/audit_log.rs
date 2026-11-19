//! A durable, append-only audit trail of gateway decisions (row 2.5).
//!
//! Lives here rather than in `se-permission` because that crate is a
//! pure decision function with no filesystem access, by design.

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use se_permission::{AuditRecord, AuditSink};

/// Appends one JSON line per decision. Each line goes through the same
/// credential redaction as the session log before it is written, since
/// a resource scope (a command line, a URL) can carry a pasted key.
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
    /// A failed write is returned, and the gateway then refuses the
    /// action: nothing runs without its decision on record.
    fn record(&self, record: AuditRecord) -> Result<(), String> {
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
        let line = se_provider::credentials::redact_secrets(&line.to_string());
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        // One write per line so a crash can truncate at most the last one.
        file.write_all(format!("{line}\n").as_bytes())
            .and_then(|()| file.flush())
            .map_err(|e| e.to_string())
    }
}

/// Stands in when the audit file could not be opened: every record
/// fails, so the gateway refuses every tool call and provider switch
/// instead of letting them run unrecorded. Plain chat still works.
pub struct UnavailableAuditSink(pub String);

impl AuditSink for UnavailableAuditSink {
    fn record(&self, _record: AuditRecord) -> Result<(), String> {
        Err(format!("the audit log is unavailable: {}", self.0))
    }
}

/// Reads an audit file back as JSON values, skipping a torn final line.
pub fn read(path: &Path) -> std::io::Result<Vec<serde_json::Value>> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_in_a_record_is_written_redacted() {
        let dir = std::env::temp_dir().join(format!("se-audit-redact-{}", std::process::id()));
        let path = dir.join("audit.jsonl");
        let _ = std::fs::remove_file(&path);
        let sink = FileAuditSink::open(&path).unwrap();
        // Fake keys, shaped like real ones.
        let scope = "curl -H 'Authorization: Bearer ghp_0123456789abcdefghijABCDEFGHIJ012345' \
                     OPENAI=sk-test-FAKE1234567890abcdef";
        sink.record(AuditRecord {
            agent_id: "a".into(),
            session: se_protocol::SessionId::from_raw(1),
            task: se_protocol::TaskId::from_raw(1),
            tool_name: "run_shell".into(),
            resource_scope: scope.into(),
            effect: se_permission::Effect::Allow,
        })
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!written.contains("ghp_0123456789"), "{written}");
        assert!(!written.contains("sk-test-FAKE"), "{written}");
        assert!(written.contains("[REDACTED]"), "{written}");
        // Still one parseable JSON line.
        assert_eq!(written.lines().count(), 1);
        serde_json::from_str::<serde_json::Value>(written.trim()).unwrap();
    }
}
