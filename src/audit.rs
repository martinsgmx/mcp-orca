//! Audit logging.
//!
//! Every tool invocation that reaches the registry records one line in
//! `<workspace>/audit.jsonl`: timestamp (unix seconds), tool name, outcome, and a
//! short, already-sanitized detail string (never raw secrets). Writes are
//! best-effort and never fail a tool call; errors are surfaced on stderr via
//! `tracing`.

use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use serde::Serialize;

use crate::config::Config;

/// Outcome of an audited action.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    Denied,
    Failed,
}

impl Outcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Denied => "denied",
            Self::Failed => "failed",
        }
    }
}

/// One audit record, serialized as a single JSON line.
#[derive(Debug, Clone, Serialize)]
struct AuditRecord<'a> {
    ts: u64,
    tool: &'a str,
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
}

/// Append-only JSONL audit logger.
#[derive(Debug, Clone)]
pub struct AuditLogger {
    path: PathBuf,
}

impl AuditLogger {
    #[must_use]
    pub fn new(config: &Config) -> Self {
        let root = crate::workspace_expand::workspace_path(&config.workspace.root);
        Self {
            path: root.join("audit.jsonl"),
        }
    }

    /// Path of the audit log (useful for diagnostics).
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    // `Outcome` is a small enum; passing it by value keeps call sites readable.
    #[allow(clippy::needless_pass_by_value)]
    pub fn record(&self, tool: &str, outcome: Outcome, detail: Option<&str>) {
        if let Err(err) = self.try_record(tool, outcome, detail) {
            tracing::warn!(
                target: "mcp_orca::audit",
                error = %err,
                tool,
                "failed to append audit record"
            );
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn try_record(
        &self,
        tool: &str,
        outcome: Outcome,
        detail: Option<&str>,
    ) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let record = AuditRecord {
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            tool,
            outcome: outcome.as_str(),
            // Details are constructed by the registry from public error/timing
            // text; cap length so a pathological message cannot bloat the log.
            detail: detail.map(|d| &d[..d.len().min(512)]),
        };
        let line = serde_json::to_string(&record)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.lock_exclusive()?;
        let result = writeln!(file, "{line}");
        let _ = file.unlock();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn appends_one_json_line_per_record() {
        let td = tempdir().unwrap();
        let mut config = Config::default();
        config.workspace.root = td.path().join("ws").to_string_lossy().to_string();
        let logger = AuditLogger::new(&config);

        logger.record("orca_artifact_list", Outcome::Success, Some("1ms"));
        logger.record("orca_artifact_delete", Outcome::Denied, Some("policy"));

        let text = std::fs::read_to_string(logger.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["tool"], "orca_artifact_list");
        assert_eq!(first["outcome"], "success");
        assert_eq!(first["detail"], "1ms");
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["outcome"], "denied");
    }
}
