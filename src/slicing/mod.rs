//! Background slice jobs with persisted lifecycle, cancellation, and pruning.

use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::{
    engine::{Engine, SliceOutcome, SlicePlan},
    error::{Error, Result},
    workspace::{validate_job_id, ArtifactKind, ArtifactStore},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl JobState {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobLogs {
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub name: String,
    pub state: JobState,
    pub created_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub duration_ms: Option<u64>,
    pub model_artifacts: Vec<String>,
    pub essentials: Value,
    pub profile_summary: Value,
    pub gcode_artifact: Option<String>,
    pub gcode_bytes: Option<u64>,
    pub logs: Option<JobLogs>,
    pub error: Option<String>,
    pub request: Value,
}

#[derive(Debug, Clone)]
pub struct SliceJobRequest {
    pub name: String,
    pub plan: SlicePlan,
    pub model_artifacts: Vec<String>,
    pub essentials: Value,
    pub profile_summary: Value,
    pub request: Value,
}

#[derive(Clone)]
pub struct JobManager {
    store: Arc<ArtifactStore>,
    engine: Arc<Engine>,
    semaphore: Arc<Semaphore>,
    keep_jobs: usize,
    tokens: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

impl JobManager {
    #[must_use]
    pub fn new(
        store: Arc<ArtifactStore>,
        engine: Arc<Engine>,
        max_concurrent: usize,
        keep_jobs: usize,
    ) -> Self {
        Self {
            store,
            engine,
            semaphore: Arc::new(Semaphore::new(max_concurrent.max(1))),
            keep_jobs,
            tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn start(&self, request: SliceJobRequest) -> Result<JobRecord> {
        let id = format!("job_{}", uuid::Uuid::new_v4().simple());
        let job_dir = self.store.create_job_dir(&id)?;
        let now = unix_now();
        let record = JobRecord {
            id: id.clone(),
            name: request.name.clone(),
            state: JobState::Queued,
            created_at: now,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            model_artifacts: request.model_artifacts.clone(),
            essentials: request.essentials.clone(),
            profile_summary: request.profile_summary.clone(),
            gcode_artifact: None,
            gcode_bytes: None,
            logs: None,
            error: None,
            request: request.request.clone(),
        };
        write_record(&job_dir, &record)?;

        let manager = self.clone();
        tokio::spawn(async move {
            manager.run(id, request.plan).await;
        });
        Ok(record)
    }

    async fn run(&self, id: String, plan: SlicePlan) {
        let Ok(_permit) = self.semaphore.clone().acquire_owned().await else {
            return;
        };

        let job_dir = self.store.job_dir(&id);
        let Ok(mut record) = read_record(&job_dir) else {
            return;
        };
        if record.state == JobState::Cancelled {
            return;
        }

        let token = CancellationToken::new();
        if let Ok(mut tokens) = self.tokens.lock() {
            tokens.insert(id.clone(), token.clone());
        }

        record.state = JobState::Running;
        record.started_at = Some(unix_now());
        let _ = write_record(&job_dir, &record);

        let started = Instant::now();
        let outcome = self
            .engine
            .slice_cancellable(&plan, &job_dir, Some(&token))
            .await;

        if let Ok(mut tokens) = self.tokens.lock() {
            tokens.remove(&id);
        }

        match outcome {
            Ok(out) => {
                self.apply_success(&job_dir, &mut record, out, started.elapsed());
            }
            Err(err) => {
                record.state = if token.is_cancelled() {
                    JobState::Cancelled
                } else {
                    JobState::Failed
                };
                record.error = Some(err.public_message());
            }
        }

        record.finished_at = Some(unix_now());
        record.duration_ms = Some(started.elapsed().as_millis() as u64);
        let _ = write_record(&job_dir, &record);
        let _ = self.prune();
    }

    fn apply_success(
        &self,
        job_dir: &Path,
        record: &mut JobRecord,
        outcome: SliceOutcome,
        _dur: Duration,
    ) {
        let gcode_path = Path::new(&outcome.gcode_path);
        let bytes = fs::metadata(gcode_path).ok().map(|m| m.len());
        let artifact = self
            .store
            .import_file(
                &format!("{}.gcode", record.id),
                ArtifactKind::Gcode,
                "gcode",
                gcode_path,
                Some(format!("slice job {}", record.id)),
                json!({"job_id": record.id, "name": record.name, "command": outcome.command}),
            )
            .map(|r| r.id);
        match artifact {
            Ok(art) => {
                record.state = JobState::Completed;
                record.gcode_artifact = Some(art);
                record.gcode_bytes = bytes;
                record.logs = Some(JobLogs {
                    stdout: outcome.stdout,
                    stderr: outcome.stderr,
                });
            }
            Err(err) => {
                record.state = JobState::Failed;
                record.error = Some(err.public_message());
                record.logs = Some(JobLogs {
                    stdout: outcome.stdout,
                    stderr: outcome.stderr,
                });
            }
        }
        let _ = job_dir; // reserved for future per-job assets
    }

    pub fn status(&self, id: &str) -> Result<JobRecord> {
        validate_job_id(id)?;
        read_record(&self.store.job_dir(id))
    }

    pub async fn wait(&self, id: &str, timeout: Duration) -> Result<JobRecord> {
        let started = Instant::now();
        loop {
            let record = self.status(id)?;
            if record.state.is_terminal() || started.elapsed() >= timeout {
                return Ok(record);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    pub fn cancel(&self, id: &str) -> Result<JobRecord> {
        let mut record = self.status(id)?;
        if record.state.is_terminal() {
            return Err(Error::Job(format!(
                "job '{id}' has already finished ({})",
                record.state.as_str()
            )));
        }
        if let Ok(tokens) = self.tokens.lock() {
            if let Some(token) = tokens.get(id) {
                token.cancel();
            }
        }
        record.state = JobState::Cancelled;
        record.finished_at = Some(unix_now());
        record.error = Some("cancelled by request".into());
        write_record(&self.store.job_dir(id), &record)?;
        Ok(record)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<JobRecord>> {
        let mut records = Vec::new();
        let jobs_dir = self.store.jobs_dir();
        if !jobs_dir.exists() {
            return Ok(records);
        }
        for entry in fs::read_dir(jobs_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Ok(record) = read_record(&entry.path()) {
                records.push(record);
            }
        }
        records.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        records.truncate(limit);
        Ok(records)
    }

    fn prune(&self) -> Result<()> {
        if self.keep_jobs == 0 {
            return Ok(());
        }
        let mut jobs = self.list(usize::MAX)?;
        jobs.retain(|j| j.state.is_terminal());
        let keep = self.keep_jobs.max(1);
        for job in jobs.into_iter().skip(keep) {
            let dir = self.store.job_dir(&job.id);
            let _ = fs::remove_dir_all(dir);
        }
        Ok(())
    }
}

fn job_file(job_dir: &Path) -> std::path::PathBuf {
    job_dir.join("job.json")
}

fn write_record(job_dir: &Path, record: &JobRecord) -> Result<()> {
    fs::create_dir_all(job_dir)?;
    let text = serde_json::to_vec_pretty(record)?;
    fs::write(job_file(job_dir), text)?;
    Ok(())
}

fn read_record(job_dir: &Path) -> Result<JobRecord> {
    let text = fs::read_to_string(job_file(job_dir)).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            Error::Job(format!(
                "job '{}' was not found",
                job_dir.file_name().unwrap_or_default().to_string_lossy()
            ))
        } else {
            Error::Io(err)
        }
    })?;
    Ok(serde_json::from_str(&text)?)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
