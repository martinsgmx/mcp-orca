//! Managed workspace and content-addressed artifact store.
//!
//! Layout under `workspace.root`:
//! - `blobs/<sha256>.<ext>`   immutable artifact bytes
//! - `meta/<artifact_id>.json` artifact metadata
//! - `jobs/<job_id>/`          slice job inputs, logs, and outputs
//! - `profiles/`               custom profiles/overrides written by tools
//! - `tmp/`                    atomic-write staging

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    config::WorkspaceConfig,
    error::{Error, Result},
};

/// Validate a job id for safe filesystem use.
pub fn validate_job_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 120 {
        return Err(Error::Artifact("invalid job id".into()));
    }
    let ok = id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !ok || !id.starts_with("job_") {
        return Err(Error::Artifact(format!("invalid job id '{id}'")));
    }
    Ok(())
}

/// Broad category of an artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Mesh,
    Gcode,
    Profile,
    Log,
    Job,
    Other,
}

impl ArtifactKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mesh => "mesh",
            Self::Gcode => "gcode",
            Self::Profile => "profile",
            Self::Log => "log",
            Self::Job => "job",
            Self::Other => "other",
        }
    }
}

impl std::str::FromStr for ArtifactKind {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mesh" | "model" | "stl" | "3mf" | "obj" | "step" | "stp" => Ok(Self::Mesh),
            "gcode" | "g-code" | "gc" => Ok(Self::Gcode),
            "profile" | "config" | "preset" => Ok(Self::Profile),
            "log" => Ok(Self::Log),
            "job" => Ok(Self::Job),
            "other" | "file" | "artifact" => Ok(Self::Other),
            other => Err(format!(
                "unknown artifact kind '{other}' (expected mesh, gcode, profile, log, job or other)"
            )),
        }
    }
}

/// Metadata for one stored artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub id: String,
    pub sha256: String,
    pub name: String,
    pub kind: ArtifactKind,
    pub format: String,
    pub bytes: u64,
    pub created_at: u64,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub metadata: Value,
}

/// Content-addressed artifact store.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
    blobs: PathBuf,
    meta: PathBuf,
    jobs: PathBuf,
    profiles: PathBuf,
    tmp: PathBuf,
    config: WorkspaceConfig,
}

impl ArtifactStore {
    /// Open (and create) the store under `config.root`.
    pub fn open(config: &WorkspaceConfig) -> Result<Self> {
        let root = expand_tilde(&config.root);
        let store = Self {
            blobs: root.join("blobs"),
            meta: root.join("meta"),
            jobs: root.join("jobs"),
            profiles: root.join("profiles"),
            tmp: root.join("tmp"),
            root,
            config: config.clone(),
        };
        for dir in [
            &store.root,
            &store.blobs,
            &store.meta,
            &store.jobs,
            &store.profiles,
            &store.tmp,
        ] {
            fs::create_dir_all(dir)?;
        }
        Ok(store)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn jobs_dir(&self) -> &Path {
        &self.jobs
    }

    #[must_use]
    pub fn profiles_dir(&self) -> &Path {
        &self.profiles
    }

    #[must_use]
    pub fn tmp_dir(&self) -> &Path {
        &self.tmp
    }

    /// Create and return a job directory for `job_id`.
    pub fn create_job_dir(&self, job_id: &str) -> Result<PathBuf> {
        validate_job_id(job_id)?;
        let dir = self.jobs.join(job_id);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Return the job directory path (may not exist yet).
    pub fn job_dir(&self, job_id: &str) -> PathBuf {
        self.jobs.join(job_id)
    }

    /// Store bytes as an artifact, deduplicating identical content.
    pub fn import_bytes(
        &self,
        name: &str,
        kind: ArtifactKind,
        format: &str,
        bytes: &[u8],
        source: Option<String>,
        metadata: Value,
    ) -> Result<ArtifactRecord> {
        if bytes.is_empty() {
            return Err(Error::Artifact("cannot store an empty artifact".into()));
        }
        if bytes.len() > self.config.max_artifact_bytes {
            return Err(Error::Artifact(format!(
                "artifact is {} bytes, above the {} byte limit",
                bytes.len(),
                self.config.max_artifact_bytes
            )));
        }

        let name = sanitize_name(name, format);
        let format = normalize_format(format, &name);
        let sha256 = sha256_hex(bytes);
        let id = format!("art_{}", &sha256[..32]);

        if let Ok(existing) = self.get(&id) {
            return Ok(existing);
        }

        if self.list(None, None, usize::MAX)?.len() >= self.config.max_artifacts {
            return Err(Error::Artifact(format!(
                "artifact store is full ({} artifacts); delete unused artifacts first",
                self.config.max_artifacts
            )));
        }

        let blob = self.blob_path(&sha256, &format);
        if !blob.exists() {
            let temp = self
                .tmp
                .join(format!(".{}.{}.tmp", sha256, Uuid::new_v4().simple()));
            {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temp)?;
                file.write_all(bytes)?;
                file.sync_all()?;
            }
            fs::rename(&temp, &blob)?;
        }

        let record = ArtifactRecord {
            id,
            sha256,
            name,
            kind,
            format,
            bytes: bytes.len() as u64,
            created_at: unix_now(),
            source,
            metadata,
        };
        self.write_record(&record)?;
        Ok(record)
    }

    /// Read a file and store it.
    pub fn import_file(
        &self,
        name: &str,
        kind: ArtifactKind,
        format: &str,
        path: &Path,
        source: Option<String>,
        metadata: Value,
    ) -> Result<ArtifactRecord> {
        let size = fs::metadata(path)?.len();
        if size > self.config.max_import_bytes as u64 {
            return Err(Error::Artifact(format!(
                "file is {size} bytes, above the {} byte import limit",
                self.config.max_import_bytes
            )));
        }
        let bytes = fs::read(path)?;
        self.import_bytes(name, kind, format, &bytes, source, metadata)
    }

    /// Load metadata by id.
    pub fn get(&self, id: &str) -> Result<ArtifactRecord> {
        let path = self.meta.join(format!("{id}.json"));
        let text = fs::read_to_string(&path)
            .map_err(|_| Error::Artifact(format!("artifact '{id}' not found")))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Resolve blob path for a record.
    #[must_use]
    pub fn blob_path_for(&self, record: &ArtifactRecord) -> PathBuf {
        self.blob_path(&record.sha256, &record.format)
    }

    /// Read artifact bytes.
    pub fn read_bytes(&self, id: &str) -> Result<Vec<u8>> {
        let record = self.get(id)?;
        Ok(fs::read(self.blob_path_for(&record))?)
    }

    /// List artifacts, newest first.
    pub fn list(
        &self,
        kind: Option<ArtifactKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ArtifactRecord>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.meta)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let record: ArtifactRecord = match serde_json::from_str(&text) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if let Some(k) = kind {
                if record.kind != k {
                    continue;
                }
            }
            if let Some(q) = query {
                let q = q.to_ascii_lowercase();
                let hay = format!(
                    "{} {} {} {}",
                    record.id, record.name, record.sha256, record.format
                )
                .to_ascii_lowercase();
                if !hay.contains(&q) {
                    continue;
                }
            }
            records.push(record);
        }
        records.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        records.truncate(limit);
        Ok(records)
    }

    /// Delete an artifact record and its blob if unreferenced by other records.
    pub fn delete(&self, id: &str) -> Result<ArtifactRecord> {
        let record = self.get(id)?;
        let meta_path = self.meta.join(format!("{id}.json"));
        fs::remove_file(meta_path)?;
        // Remove blob only if no other record references same sha/format path.
        let blob = self.blob_path_for(&record);
        let mut still_referenced = false;
        if let Ok(entries) = fs::read_dir(&self.meta) {
            for entry in entries.flatten() {
                if let Ok(text) = fs::read_to_string(entry.path()) {
                    if let Ok(other) = serde_json::from_str::<ArtifactRecord>(&text) {
                        if other.sha256 == record.sha256 && other.format == record.format {
                            still_referenced = true;
                            break;
                        }
                    }
                }
            }
        }
        if !still_referenced && blob.exists() {
            let _ = fs::remove_file(blob);
        }
        Ok(record)
    }

    /// Export artifact bytes to a destination path.
    pub fn export(&self, id: &str, dest: &Path, overwrite: bool) -> Result<ArtifactRecord> {
        let record = self.get(id)?;
        if dest.exists() && !overwrite {
            return Err(Error::Artifact(format!(
                "destination '{}' already exists (set overwrite=true to replace)",
                dest.display()
            )));
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = self.read_bytes(id)?;
        let temp = dest.with_extension(format!(
            "{}.tmp-{}",
            dest.extension().and_then(|e| e.to_str()).unwrap_or("part"),
            Uuid::new_v4().simple()
        ));
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&temp, dest)?;
        Ok(record)
    }

    fn blob_path(&self, sha256: &str, format: &str) -> PathBuf {
        self.blobs.join(format!("{sha256}.{format}"))
    }

    fn write_record(&self, record: &ArtifactRecord) -> Result<()> {
        let path = self.meta.join(format!("{}.json", record.id));
        let temp = self
            .tmp
            .join(format!("{}.json.tmp", Uuid::new_v4().simple()));
        let text = serde_json::to_string_pretty(record)?;
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&temp, &path)?;
        Ok(())
    }
}

#[must_use]
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn normalize_format(format: &str, name: &str) -> String {
    let fmt = format.trim().trim_start_matches('.').to_ascii_lowercase();
    if !fmt.is_empty() {
        return safe_ext(&fmt);
    }
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(safe_ext)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "bin".to_string())
}

fn safe_ext(ext: &str) -> String {
    let cleaned: String = ext
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(16)
        .collect();
    if cleaned.is_empty() {
        "bin".to_string()
    } else {
        cleaned
    }
}

fn sanitize_name(name: &str, fallback_ext: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact");
    let mut cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        cleaned = format!("artifact.{}", safe_ext(fallback_ext));
    }
    if cleaned.len() > 120 {
        cleaned.truncate(120);
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkspaceConfig;
    use tempfile::tempdir;

    fn cfg(root: &Path) -> WorkspaceConfig {
        WorkspaceConfig {
            root: root.to_string_lossy().to_string(),
            import_roots: vec![],
            export_roots: vec![],
            max_import_bytes: 1024 * 1024,
            max_artifact_bytes: 1024 * 1024,
            max_artifacts: 100,
            keep_jobs: 10,
        }
    }

    #[test]
    fn import_list_get_delete_roundtrip() {
        let td = tempdir().unwrap();
        let store = ArtifactStore::open(&cfg(td.path())).unwrap();
        let rec = store
            .import_bytes(
                "cube.stl",
                ArtifactKind::Mesh,
                "stl",
                b"solid x\nendsolid x\n",
                None,
                serde_json::json!({"k": 1}),
            )
            .unwrap();
        assert!(rec.id.starts_with("art_"));
        let got = store.get(&rec.id).unwrap();
        assert_eq!(got.name, "cube.stl");
        let list = store.list(Some(ArtifactKind::Mesh), None, 10).unwrap();
        assert_eq!(list.len(), 1);
        let bytes = store.read_bytes(&rec.id).unwrap();
        assert!(bytes.starts_with(b"solid"));
        let deleted = store.delete(&rec.id).unwrap();
        assert_eq!(deleted.id, rec.id);
        assert!(store.get(&rec.id).is_err());
    }

    #[test]
    fn dedup_same_bytes_same_id() {
        let td = tempdir().unwrap();
        let store = ArtifactStore::open(&cfg(td.path())).unwrap();
        let a = store
            .import_bytes(
                "a.gcode",
                ArtifactKind::Gcode,
                "gcode",
                b"G1 X0\n",
                None,
                serde_json::Value::Null,
            )
            .unwrap();
        let b = store
            .import_bytes(
                "b.gcode",
                ArtifactKind::Gcode,
                "gcode",
                b"G1 X0\n",
                None,
                serde_json::Value::Null,
            )
            .unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(store.list(None, None, 10).unwrap().len(), 1);
    }
}
