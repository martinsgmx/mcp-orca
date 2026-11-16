//! Approval store for gated actions.
//!
//! Export and destructive tools require approval by policy. The first attempt
//! records a pending request and fails with an explanatory error; an operator
//! confirms or denies it with `orca_approval_decide`; a retry of the identical
//! call consumes the approval and proceeds. Requests expire after
//! `approval.ttl_secs`.

use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

/// State of an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Consumed,
}

impl ApprovalStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Consumed => "consumed",
        }
    }
}

/// One approval request, persisted under `approvals/<id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub tool: String,
    pub fingerprint: String,
    pub reason: String,
    pub status: ApprovalStatus,
    pub created_at: u64,
    pub expires_at: u64,
    #[serde(default)]
    pub decided_at: Option<u64>,
    #[serde(default)]
    pub note: Option<String>,
}

impl ApprovalRequest {
    #[must_use]
    pub fn is_expired(&self, now: u64) -> bool {
        self.expires_at <= now
    }
}

/// File-backed approval store.
#[derive(Debug, Clone)]
pub struct ApprovalStore {
    dir: PathBuf,
    ttl_secs: u64,
}

impl ApprovalStore {
    pub fn new(dir: impl Into<PathBuf>, ttl_secs: u64) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            ttl_secs: ttl_secs.max(1),
        })
    }

    /// Return an existing pending request for the same action, or create one.
    pub fn request(&self, tool: &str, fingerprint: &str, reason: &str) -> Result<ApprovalRequest> {
        let now = unix_now();
        for mut existing in self.list_all()? {
            if existing.tool == tool && existing.fingerprint == fingerprint {
                if existing.status == ApprovalStatus::Pending && existing.is_expired(now) {
                    existing.status = ApprovalStatus::Denied;
                    existing.decided_at = Some(now);
                    existing.note = Some("expired".into());
                    self.write(&existing)?;
                } else if existing.status == ApprovalStatus::Pending {
                    return Ok(existing);
                }
            }
        }
        let request = ApprovalRequest {
            id: format!("apr_{}", Uuid::new_v4().simple()),
            tool: tool.to_string(),
            fingerprint: fingerprint.to_string(),
            reason: reason.to_string(),
            status: ApprovalStatus::Pending,
            created_at: now,
            expires_at: now.saturating_add(self.ttl_secs),
            decided_at: None,
            note: None,
        };
        self.write(&request)?;
        Ok(request)
    }

    /// Confirm or deny a pending request.
    pub fn decide(&self, id: &str, approve: bool, note: Option<String>) -> Result<ApprovalRequest> {
        let mut request = self.get(id)?;
        if request.status != ApprovalStatus::Pending {
            return Err(Error::Approval(format!(
                "approval '{}' is not pending (status: {})",
                request.id,
                request.status.as_str()
            )));
        }
        let now = unix_now();
        if request.is_expired(now) {
            request.status = ApprovalStatus::Denied;
            request.decided_at = Some(now);
            request.note = Some("expired".into());
            self.write(&request)?;
            return Err(Error::Approval(format!(
                "approval '{}' expired before decision",
                request.id
            )));
        }
        request.status = if approve {
            ApprovalStatus::Approved
        } else {
            ApprovalStatus::Denied
        };
        request.decided_at = Some(now);
        request.note = note;
        self.write(&request)?;
        Ok(request)
    }

    /// Consume an approved, unexpired request for this exact action.
    pub fn consume(&self, tool: &str, fingerprint: &str) -> Result<Option<ApprovalRequest>> {
        let now = unix_now();
        for mut request in self.list_all()? {
            if request.tool != tool || request.fingerprint != fingerprint {
                continue;
            }
            if request.status != ApprovalStatus::Approved {
                continue;
            }
            if request.is_expired(now) {
                request.status = ApprovalStatus::Denied;
                request.decided_at = Some(now);
                request.note = Some("expired".into());
                self.write(&request)?;
                continue;
            }
            request.status = ApprovalStatus::Consumed;
            self.write(&request)?;
            return Ok(Some(request));
        }
        Ok(None)
    }

    pub fn get(&self, id: &str) -> Result<ApprovalRequest> {
        validate_approval_id(id)?;
        let path = self.dir.join(format!("{id}.json"));
        let text = fs::read_to_string(&path)
            .map_err(|_| Error::Approval(format!("approval '{id}' not found")))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Pending, unexpired requests.
    pub fn list_pending(&self) -> Result<Vec<ApprovalRequest>> {
        let now = unix_now();
        let mut out: Vec<ApprovalRequest> = self
            .list_all()?
            .into_iter()
            .filter(|r| r.status == ApprovalStatus::Pending && !r.is_expired(now))
            .collect();
        out.sort_by_key(|r| r.created_at);
        Ok(out)
    }

    fn list_all(&self) -> Result<Vec<ApprovalRequest>> {
        let mut out = Vec::new();
        if !self.dir.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(request) = serde_json::from_str::<ApprovalRequest>(&text) {
                out.push(request);
            }
        }
        out.sort_by_key(|r| r.created_at);
        Ok(out)
    }

    fn write(&self, request: &ApprovalRequest) -> Result<()> {
        let path = self.dir.join(format!("{}.json", request.id));
        let temp = self.dir.join(format!(".{}.tmp", request.id));
        fs::write(&temp, serde_json::to_vec_pretty(request)?)?;
        fs::rename(temp, path)?;
        Ok(())
    }
}

fn validate_approval_id(id: &str) -> Result<()> {
    let ok = id.starts_with("apr_")
        && id.len() <= 80
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !ok {
        return Err(Error::Approval(format!("invalid approval id '{id}'")));
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn approve_then_consume_once() {
        let td = tempdir().unwrap();
        let store = ApprovalStore::new(td.path(), 60).unwrap();
        let request = store
            .request("orca_artifact_delete", "fp", "delete")
            .unwrap();
        assert_eq!(request.status, ApprovalStatus::Pending);
        assert_eq!(store.list_pending().unwrap().len(), 1);

        // Same action reuses the pending request instead of piling up.
        let again = store
            .request("orca_artifact_delete", "fp", "delete")
            .unwrap();
        assert_eq!(again.id, request.id);

        let decided = store.decide(&request.id, true, None).unwrap();
        assert_eq!(decided.status, ApprovalStatus::Approved);
        assert_eq!(store.list_pending().unwrap().len(), 0);

        let consumed = store
            .consume("orca_artifact_delete", "fp")
            .unwrap()
            .expect("approved request should be consumable");
        assert_eq!(consumed.id, request.id);
        assert!(store
            .consume("orca_artifact_delete", "fp")
            .unwrap()
            .is_none());
        assert_eq!(
            store.get(&request.id).unwrap().status,
            ApprovalStatus::Consumed
        );
    }

    #[test]
    fn deny_blocks_consume() {
        let td = tempdir().unwrap();
        let store = ApprovalStore::new(td.path(), 60).unwrap();
        let request = store
            .request("orca_artifact_export", "fp", "export")
            .unwrap();
        store.decide(&request.id, false, Some("no".into())).unwrap();
        assert!(store
            .consume("orca_artifact_export", "fp")
            .unwrap()
            .is_none());
    }

    #[test]
    fn expired_request_is_not_pending() {
        let td = tempdir().unwrap();
        let store = ApprovalStore::new(td.path(), 1).unwrap();
        let request = store
            .request("orca_artifact_export", "fp", "export")
            .unwrap();
        // Rewrite with an expiry in the past.
        let mut stale = request.clone();
        stale.expires_at = 0;
        store.write(&stale).unwrap();
        assert!(store.list_pending().unwrap().is_empty());
        assert!(store
            .consume("orca_artifact_export", "fp")
            .unwrap()
            .is_none());
    }
}
