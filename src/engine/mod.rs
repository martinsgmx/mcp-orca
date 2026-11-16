//! `OrcaSlicer` engine runner abstraction.
//!
//! Generic adapter with host (`binary`) and Docker runners. The concrete CLI is
//! configured with an argv template (`engine.slice_command`) using placeholders:
//!
//! - `{binary}` – configured binary (binary runner only)
//! - `{image}` – configured Docker image (docker runner only)
//! - `{jobdir}` – job working directory (also used for Docker mounts)
//! - `{gcode}` – expected output G-code path (`<jobdir>/output.gcode`)
//! - `{models}` – expands to one argument per staged model path
//! - `{model}` – first staged model path
//! - `{printer}`, `{filament}`, `{process}`, `{project}` – selected essentials
//! - `{overrides}` – path to `overrides.json` written in the job directory
//! - `{settings}` – path to `settings.json` (merged resolved settings)
//! - `{plan_json}` – path to `plan.json` written in the job directory
//!
//! Example (host): `slice_command = ["{binary}", "--slice", "{models}",
//! "--load", "{printer}", "--load", "{filament}", "--load", "{process}",
//! "--output", "{gcode}"]`
//!
//! Example (docker): `docker_command = ["docker", "run", "--rm", "-v",
//! "{jobdir}:{jobdir}", "-w", "{jobdir}"]`, `slice_command = ["{image}", ...]`.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{process::Command, time::timeout};
use tokio_util::sync::CancellationToken;

use crate::{
    config::{Config, EngineRunner},
    error::{Error, Result},
};

const MAX_LOG_BYTES: usize = 64 * 1024;

/// Result of probing the configured engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineInfo {
    pub runner: EngineRunner,
    pub binary: String,
    pub resolved_path: Option<String>,
    pub docker_image: Option<String>,
    pub available: bool,
    pub version: Option<String>,
    pub detail: String,
    pub slice_command_configured: bool,
}

/// One model in a slice plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanModel {
    pub artifact_id: String,
    pub source_path: PathBuf,
    pub file_name: String,
}

/// Everything needed to invoke the slicer for one job (adapter input).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlicePlan {
    pub models: Vec<PlanModel>,
    pub printer: Option<String>,
    pub filament: Option<String>,
    pub process: Option<String>,
    pub project: Option<String>,
    pub profiles: Vec<String>,
    pub overrides: Value,
    /// Merged settings (workspace custom profiles + overrides), when known.
    #[serde(default)]
    pub resolved_settings: Value,
    pub timeout: Duration,
}

/// Outcome of a successful engine run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SliceOutcome {
    pub gcode_path: String,
    pub command: Vec<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
    pub stdout: String,
    pub stderr: String,
}

/// Engine facade.
#[derive(Debug, Clone)]
pub struct Engine {
    config: Arc<Config>,
}

impl Engine {
    pub fn new(config: Arc<Config>) -> Result<Self> {
        Ok(Self { config })
    }

    /// Probe the configured runner without slicing anything.
    pub async fn probe(&self) -> EngineInfo {
        match self.config.engine.runner {
            EngineRunner::Binary => self.probe_binary().await,
            EngineRunner::Docker => self.probe_docker(),
        }
    }

    async fn probe_binary(&self) -> EngineInfo {
        let binary = self.config.engine.binary.clone();
        let resolved_path = which::which(&binary)
            .ok()
            .map(|p| p.to_string_lossy().to_string());
        let slice_command_configured = !self.config.engine.slice_command.is_empty();
        let mut info = EngineInfo {
            runner: EngineRunner::Binary,
            binary: binary.clone(),
            resolved_path: resolved_path.clone(),
            docker_image: None,
            available: false,
            version: None,
            detail: String::new(),
            slice_command_configured,
        };
        let Some(path) = resolved_path else {
            info.detail = format!(
                "host binary '{binary}' not found on PATH; configure engine.binary or use docker runner"
            );
            return info;
        };
        info.available = true;
        info.detail = format!("found host binary at {path}");
        if !slice_command_configured {
            info.detail
                .push_str("; engine.slice_command is empty (slicing not wired)");
        }
        for flag in ["--version", "-v", "version", "help", "--help"] {
            let mut cmd = Command::new(&binary);
            cmd.envs(&self.config.engine.env);
            cmd.arg(flag).stdout(Stdio::piped()).stderr(Stdio::piped());
            if let Ok(Ok(out)) = timeout(Duration::from_secs(5), cmd.output()).await {
                let mut text = String::from_utf8_lossy(&out.stdout).to_string();
                text.push('\n');
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                if let Some(v) = text
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .map(|l| l.chars().take(120).collect::<String>())
                {
                    info.version = Some(v);
                    break;
                }
            }
        }
        info
    }

    fn probe_docker(&self) -> EngineInfo {
        let image = self.config.engine.docker_image.clone();
        let mut info = EngineInfo {
            runner: EngineRunner::Docker,
            binary: "docker".to_string(),
            resolved_path: which::which("docker")
                .ok()
                .map(|p| p.to_string_lossy().to_string()),
            docker_image: image.clone(),
            available: false,
            version: None,
            detail: String::new(),
            slice_command_configured: !self.config.engine.slice_command.is_empty(),
        };
        if info.resolved_path.is_none() {
            info.detail = "docker not found on PATH".into();
            return info;
        }
        let Some(image) = image else {
            info.detail = "engine.docker_image is not configured".into();
            return info;
        };
        info.available = true;
        info.detail = format!("docker available; image '{image}' configured");
        if !info.slice_command_configured {
            info.detail
                .push_str("; engine.slice_command is empty (slicing not wired)");
        }
        info
    }

    /// Execute a slice plan inside `job_dir`.
    pub async fn slice(&self, plan: &SlicePlan, job_dir: &Path) -> Result<SliceOutcome> {
        self.slice_cancellable(plan, job_dir, None).await
    }

    /// Execute a slice plan, stopping early when `cancel` is triggered.
    pub async fn slice_cancellable(
        &self,
        plan: &SlicePlan,
        job_dir: &Path,
        cancel: Option<&CancellationToken>,
    ) -> Result<SliceOutcome> {
        std::fs::create_dir_all(job_dir)?;
        if let Some(token) = cancel {
            if token.is_cancelled() {
                return Err(Error::Job("slice cancelled before start".into()));
            }
        }

        if self.config.engine.slice_command.is_empty() {
            return Err(Error::Engine(format!(
                "engine.slice_command is not configured for runner={:?} (binary='{}', docker_image={:?}). Wire the OrcaSlicer CLI argv template per docs/DESIGN.md.",
                self.config.engine.runner, self.config.engine.binary, self.config.engine.docker_image
            )));
        }

        let started = Instant::now();

        // Stage models and plan/overrides files into the job directory.
        let mut staged_models = Vec::new();
        for model in &plan.models {
            let dest = job_dir.join(&model.file_name);
            std::fs::copy(&model.source_path, &dest).map_err(|err| {
                Error::Engine(format!(
                    "could not stage model {} into job dir: {err}",
                    model.source_path.display()
                ))
            })?;
            staged_models.push(dest);
        }

        let overrides_path = job_dir.join("overrides.json");
        std::fs::write(
            &overrides_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "printer": plan.printer,
                "filament": plan.filament,
                "process": plan.process,
                "project": plan.project,
                "profiles": plan.profiles,
                "overrides": plan.overrides,
                "resolved_settings": plan.resolved_settings,
            }))?,
        )?;
        let settings_path = job_dir.join("settings.json");
        std::fs::write(
            &settings_path,
            serde_json::to_vec_pretty(&plan.resolved_settings)?,
        )?;
        let plan_path = job_dir.join("plan.json");
        std::fs::write(&plan_path, serde_json::to_vec_pretty(plan)?)?;

        let output_path = job_dir.join("output.gcode");
        let _ = std::fs::remove_file(&output_path);

        let argv = self.build_argv(
            plan,
            job_dir,
            &staged_models,
            &overrides_path,
            &settings_path,
            &plan_path,
            &output_path,
        )?;
        let env = self.expand_env(
            plan,
            &job_dir.to_string_lossy(),
            &output_path.to_string_lossy(),
            &staged_models,
            &overrides_path,
            &settings_path,
            &plan_path,
            self.config.engine.docker_image.as_deref().unwrap_or(""),
        );
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        command.envs(env);
        command.current_dir(job_dir);
        command.kill_on_drop(true);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        let child = command
            .spawn()
            .map_err(|err| Error::Engine(format!("could not execute '{}': {err}", argv[0])))?;

        let output = run_with_cancel(child, plan.timeout, cancel).await?;
        let duration = started.elapsed();

        let stdout = truncate(&String::from_utf8_lossy(&output.stdout), MAX_LOG_BYTES);
        let stderr = truncate(&String::from_utf8_lossy(&output.stderr), MAX_LOG_BYTES);

        if !output.status.success() {
            let detail = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            return Err(Error::Engine(format!(
                "slicer exited with status {:?}: {}",
                output.status.code(),
                truncate(detail, 2000)
            )));
        }

        let meta = std::fs::metadata(&output_path).map_err(|_| {
            Error::Engine(format!(
                "slicer reported success but did not write {}",
                output_path.display()
            ))
        })?;
        if meta.len() == 0 {
            return Err(Error::Engine(
                "slicer reported success but G-code is empty".into(),
            ));
        }

        Ok(SliceOutcome {
            gcode_path: output_path.to_string_lossy().to_string(),
            command: argv,
            exit_code: output.status.code(),
            duration_ms: duration.as_millis(),
            stdout,
            stderr,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn expand_env(
        &self,
        plan: &SlicePlan,
        job_dir: &str,
        gcode: &str,
        models: &[PathBuf],
        overrides_path: &Path,
        settings_path: &Path,
        plan_path: &Path,
        image: &str,
    ) -> Vec<(String, String)> {
        let mut out = Vec::with_capacity(self.config.engine.env.len());
        for (key, value) in &self.config.engine.env {
            let expanded = expand_template(
                std::slice::from_ref(value),
                plan,
                job_dir,
                gcode,
                models,
                overrides_path,
                settings_path,
                plan_path,
                &self.config.engine.binary,
                image,
            );
            if let Some(first) = expanded.into_iter().next() {
                out.push((key.clone(), first));
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn build_argv(
        &self,
        plan: &SlicePlan,
        job_dir: &Path,
        models: &[PathBuf],
        overrides_path: &Path,
        settings_path: &Path,
        plan_path: &Path,
        output_path: &Path,
    ) -> Result<Vec<String>> {
        let job = job_dir.to_string_lossy().to_string();
        let gcode = output_path.to_string_lossy().to_string();
        let mut argv: Vec<String> = Vec::new();

        match self.config.engine.runner {
            EngineRunner::Binary => {}
            EngineRunner::Docker => {
                let image = self.config.engine.docker_image.as_deref().ok_or_else(|| {
                    Error::Engine("engine.docker_image is required for the docker runner".into())
                })?;
                let prefix = if self.config.engine.docker_command.is_empty() {
                    vec![
                        "docker".to_string(),
                        "run".to_string(),
                        "--rm".to_string(),
                        "-v".to_string(),
                        format!("{job}:{job}"),
                        "-w".to_string(),
                        job.clone(),
                    ]
                } else {
                    expand_template(
                        &self.config.engine.docker_command,
                        plan,
                        &job,
                        &gcode,
                        models,
                        overrides_path,
                        settings_path,
                        plan_path,
                        "",
                        image,
                    )
                };
                argv.extend(prefix);
                // The image is the container command entrypoint.
                // `slice_command` may include `{image}` explicitly; otherwise prepend it.
                let mut slice = self.config.engine.slice_command.clone();
                if slice.iter().any(|s| s.contains("{image}")) {
                    for arg in &mut slice {
                        if arg.contains("{image}") {
                            *arg = arg.replace("{image}", image);
                        }
                    }
                } else {
                    slice.insert(0, image.to_string());
                }
                argv.extend(expand_template(
                    &slice,
                    plan,
                    &job,
                    &gcode,
                    models,
                    overrides_path,
                    settings_path,
                    plan_path,
                    &self.config.engine.binary,
                    image,
                ));
                return Ok(argv);
            }
        }

        argv.extend(expand_template(
            &self.config.engine.slice_command,
            plan,
            &job,
            &gcode,
            models,
            overrides_path,
            settings_path,
            plan_path,
            &self.config.engine.binary,
            "",
        ));
        Ok(argv)
    }
}

#[allow(clippy::too_many_arguments)]
fn expand_template(
    template: &[String],
    plan: &SlicePlan,
    job_dir: &str,
    gcode: &str,
    models: &[PathBuf],
    overrides_path: &Path,
    settings_path: &Path,
    plan_path: &Path,
    binary: &str,
    image: &str,
) -> Vec<String> {
    let model_strings: Vec<String> = models
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let mut out = Vec::new();

    for arg in template {
        match arg.as_str() {
            "{models}" => {
                if model_strings.is_empty() {
                    out.push(String::new());
                } else {
                    out.extend(model_strings.iter().cloned());
                }
            }
            "{model}" => out.push(model_strings.first().cloned().unwrap_or_default()),
            _ => {
                let mut s = arg.clone();
                s = s.replace("{binary}", binary);
                s = s.replace("{image}", image);
                s = s.replace("{jobdir}", job_dir);
                s = s.replace("{gcode}", gcode);
                s = s.replace("{printer}", plan.printer.as_deref().unwrap_or(""));
                s = s.replace("{filament}", plan.filament.as_deref().unwrap_or(""));
                s = s.replace("{process}", plan.process.as_deref().unwrap_or(""));
                s = s.replace("{project}", plan.project.as_deref().unwrap_or(""));
                s = s.replace("{overrides}", &overrides_path.to_string_lossy());
                s = s.replace("{settings}", &settings_path.to_string_lossy());
                s = s.replace("{plan_json}", &plan_path.to_string_lossy());
                out.push(s);
            }
        }
    }
    out
}

async fn run_with_cancel(
    child: tokio::process::Child,
    limit: Duration,
    cancel: Option<&CancellationToken>,
) -> Result<std::process::Output> {
    let wait = child.wait_with_output();

    let outcome = if let Some(token) = cancel {
        tokio::select! {
            res = timeout(limit, wait) => res,
            () = token.cancelled() => {
                return Err(Error::Job("slice cancelled".into()));
            }
        }
    } else {
        timeout(limit, wait).await
    };

    match outcome {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(err)) => Err(Error::Engine(format!("failed to run slicer: {err}"))),
        Err(_) => Err(Error::Engine(format!(
            "slicer timed out after {} seconds",
            limit.as_secs()
        ))),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_configured_slice_command_and_produces_gcode() {
        use std::os::unix::fs::PermissionsExt;

        let td = tempdir().unwrap();
        let script = td.path().join("fake_slicer.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\n# fake slicer: first arg is output path\necho 'G1 X0 Y0' > \"$1\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut cfg = Config::default();
        cfg.engine.binary = script.to_string_lossy().to_string();
        cfg.engine.slice_command = vec!["{binary}".into(), "{gcode}".into(), "{models}".into()];
        let engine = Engine::new(Arc::new(cfg)).unwrap();

        let model = td.path().join("m.stl");
        std::fs::write(&model, b"solid x\nendsolid x\n").unwrap();
        let plan = SlicePlan {
            models: vec![PlanModel {
                artifact_id: "art_test".into(),
                source_path: model,
                file_name: "model_0.stl".into(),
            }],
            printer: Some("p".into()),
            filament: Some("f".into()),
            process: Some("pr".into()),
            project: None,
            profiles: vec![],
            overrides: serde_json::json!({"sparse_infill_density": 15}),
            resolved_settings: serde_json::json!({"sparse_infill_density": 15}),
            timeout: Duration::from_secs(10),
        };
        let job_dir = td.path().join("job");
        let outcome = engine.slice(&plan, &job_dir).await.unwrap();
        assert!(std::path::Path::new(&outcome.gcode_path).exists());
        let settings = std::fs::read_to_string(job_dir.join("settings.json")).unwrap();
        assert!(settings.contains("sparse_infill_density"));
        let plan_json = std::fs::read_to_string(job_dir.join("plan.json")).unwrap();
        assert!(plan_json.contains("art_test"));
        assert!(outcome.stdout.contains(""));
        let text = std::fs::read_to_string(&outcome.gcode_path).unwrap();
        assert!(text.contains("G1 X0 Y0"));
    }
}
