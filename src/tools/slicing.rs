//! Slicing and job tools.

use serde_json::{json, Value};

use crate::{
    engine::{PlanModel, SlicePlan},
    error::Error,
    safety::{Capability, Risk},
    slicing::SliceJobRequest,
    tools::{
        args::Args, json_result, no_summary, schema_object, Gate, ToolContext, ToolFuture, ToolSpec,
    },
    workspace::ArtifactKind,
};

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "orca_slice",
            title: "Slice models",
            description: "Start an OrcaSlicer slice job for one or more mesh artifacts using printer/filament/process essentials and overrides. Workspace custom profile artifacts contribute merged settings (staged as settings.json); overrides take precedence. Returns a job id immediately unless wait=true. Execution requires engine.slice_command; otherwise jobs fail with a clear configuration error.",
            allowed_args: &[
                "model_artifact_ids", "printer", "filament", "process", "project",
                "profiles", "overrides", "name", "wait", "wait_timeout_secs"
            ],
            capability: Capability::Process,
            risk: Risk::Medium,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "model_artifact_ids": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                        "printer": {"type": "string", "description": "Printer preset id/name/path (essential unless provided by project)."},
                        "filament": {"type": "string", "description": "Filament preset id/name/path (essential unless provided by project)."},
                        "process": {"type": "string", "description": "Process preset id/name/path (essential unless provided by project)."},
                        "project": {"type": "string", "description": "Optional project/profile path (e.g. .3mf) if supported by target."},
                        "profiles": {"type": "array", "items": {"type": "string"}},
                        "overrides": {"type": "object", "description": "Setting overrides (infill, temps, speeds, supports, walls, retraction, cooling, flow, adhesion, motion limits, etc.) merged last, after workspace profile settings."},
                        "name": {"type": "string"},
                        "wait": {"type": "boolean", "default": false},
                        "wait_timeout_secs": {"type": "integer", "minimum": 1, "maximum": 86400, "default": 900}
                    }),
                    &["model_artifact_ids"],
                )
            },
            handler: slice,
        },
        ToolSpec {
            name: "orca_slice_status",
            title: "Slice job status",
            description: "Get status/result metadata for a slice job.",
            allowed_args: &["job_id"],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || schema_object(json!({"job_id": {"type": "string"}}), &["job_id"]),
            handler: status,
        },
        ToolSpec {
            name: "orca_slice_wait",
            title: "Wait for slice job",
            description: "Wait for a slice job to complete or time out.",
            allowed_args: &["job_id", "timeout_secs"],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || schema_object(json!({"job_id": {"type": "string"}, "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 86400, "default": 900}}), &["job_id"]),
            handler: wait,
        },
        ToolSpec {
            name: "orca_slice_cancel",
            title: "Cancel slice job",
            description: "Request cancellation of a queued/running slice job.",
            allowed_args: &["job_id"],
            capability: Capability::Process,
            risk: Risk::Medium,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || schema_object(json!({"job_id": {"type": "string"}}), &["job_id"]),
            handler: cancel,
        },
        ToolSpec {
            name: "orca_slice_list",
            title: "List slice jobs",
            description: "List known slice jobs, newest first.",
            allowed_args: &["limit"],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || schema_object(json!({"limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50}}), &[]),
            handler: list,
        },
    ]
}

fn slice(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(
            Some(args),
            &[
                "model_artifact_ids",
                "printer",
                "filament",
                "process",
                "project",
                "profiles",
                "overrides",
                "name",
                "wait",
                "wait_timeout_secs",
            ],
        )?;
        let model_ids_val = args
            .value("model_artifact_ids")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if model_ids_val.is_empty() {
            return Err(Error::InvalidArguments(
                "model_artifact_ids must contain at least one mesh artifact id".into(),
            ));
        }
        let mut model_artifact_ids = Vec::new();
        let mut models = Vec::new();
        for (i, v) in model_ids_val.iter().enumerate() {
            let id = v.as_str().ok_or_else(|| {
                Error::InvalidArguments("model_artifact_ids must be strings".into())
            })?;
            let record = ctx.store.get(id)?;
            if !matches!(record.kind, ArtifactKind::Mesh) {
                return Err(Error::InvalidArguments(format!(
                    "artifact '{id}' is not a mesh"
                )));
            }
            let source_path = ctx.store.blob_path_for(&record);
            let file_name = format!("model_{i}.{}", record.format);
            model_artifact_ids.push(record.id.clone());
            models.push(PlanModel {
                artifact_id: record.id,
                source_path,
                file_name,
            });
        }

        let printer = args.optional_string("printer");
        let filament = args.optional_string("filament");
        let process = args.optional_string("process");
        let project = args.optional_string("project");
        let profiles = args
            .value("profiles")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(ToString::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let overrides = args.value("overrides").cloned().unwrap_or(json!({}));
        let name = args
            .optional_string("name")
            .unwrap_or_else(|| "slice".to_string());
        let wait_flag = args.optional_bool("wait").unwrap_or(false);
        let wait_timeout_secs = args
            .value("wait_timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(900)
            .clamp(1, 86_400);

        // v1 invariant: essentials required unless project provides context (target-dependent)
        let missing: Vec<&str> = [
            ("printer", printer.is_none()),
            ("filament", filament.is_none()),
            ("process", process.is_none()),
        ]
        .into_iter()
        .filter_map(|(k, m)| if m { Some(k) } else { None })
        .collect();
        if !missing.is_empty() && project.is_none() {
            return Err(Error::InvalidArguments(format!(
                "slice requires essential presets: missing {}. Provide printer, filament, and process (or a supported project). No invented defaults.",
                missing.join(", ")
            )));
        }

        let essentials = json!({"printer": printer, "filament": filament, "process": process, "project": project});
        let resolved_settings = merge_effective_settings(
            ctx,
            [printer.as_deref(), filament.as_deref(), process.as_deref()],
            &overrides,
        );
        let plan = SlicePlan {
            models,
            printer: printer.clone(),
            filament: filament.clone(),
            process: process.clone(),
            project: project.clone(),
            profiles: profiles.clone(),
            overrides: overrides.clone(),
            resolved_settings: resolved_settings.clone(),
            timeout: std::time::Duration::from_secs(ctx.config.engine.timeout_secs),
        };
        let request = SliceJobRequest {
            name: name.clone(),
            plan,
            model_artifacts: model_artifact_ids.clone(),
            essentials: essentials.clone(),
            profile_summary: json!({
                "profiles": profiles,
                "overrides": overrides,
                "resolved_settings": resolved_settings,
            }),
            request: json!({
                "name": name,
                "model_artifact_ids": model_artifact_ids,
                "essentials": essentials,
                "profiles": args.value("profiles").cloned().unwrap_or(json!([])),
                "overrides": args.value("overrides").cloned().unwrap_or(json!({})),
            }),
        };

        let mut record = ctx.jobs.start(request)?;
        if wait_flag {
            record = ctx
                .jobs
                .wait(
                    &record.id,
                    std::time::Duration::from_secs(wait_timeout_secs),
                )
                .await?;
        }
        Ok(json_result(&job_json(&record), None))
    })
}

fn status(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(Some(args), &["job_id"])?;
        let id = args.required_string("job_id")?;
        let record = ctx.jobs.status(&id)?;
        Ok(json_result(&job_json(&record), None))
    })
}

fn wait(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(Some(args), &["job_id", "timeout_secs"])?;
        let id = args.required_string("job_id")?;
        let timeout_secs = args
            .value("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(900)
            .clamp(1, 86_400);
        let record = ctx
            .jobs
            .wait(&id, std::time::Duration::from_secs(timeout_secs))
            .await?;
        Ok(json_result(&job_json(&record), None))
    })
}

fn cancel(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(Some(args), &["job_id"])?;
        let id = args.required_string("job_id")?;
        let record = ctx.jobs.cancel(&id)?;
        Ok(json_result(&job_json(&record), None))
    })
}

fn list(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(Some(args), &["limit"])?;
        let limit = args
            .value("limit")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .clamp(1, 500) as usize;
        let jobs = ctx.jobs.list(limit)?;
        let items: Vec<Value> = jobs.iter().map(job_json).collect();
        Ok(json_result(
            &json!({"count": items.len(), "jobs": items}),
            None,
        ))
    })
}

/// Merge settings from workspace custom profile artifacts, then overrides.
///
/// Native presets are left to the engine adapter; only workspace artifacts
/// (whose payload is defined by this server) can be merged safely here.
fn merge_effective_settings(
    ctx: &ToolContext,
    selections: [Option<&str>; 3],
    overrides: &Value,
) -> Value {
    let mut merged = serde_json::Map::new();
    for value in selections.into_iter().flatten() {
        if !value.starts_with("art_") {
            continue;
        }
        let Some(settings) = ctx
            .store
            .read_bytes(value)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|payload| payload.get("settings").cloned())
        else {
            continue;
        };
        if let Value::Object(map) = settings {
            for (key, val) in map {
                merged.insert(key, val);
            }
        }
    }
    if let Value::Object(map) = overrides {
        for (key, val) in map {
            merged.insert(key.clone(), val.clone());
        }
    }
    if merged.is_empty() {
        Value::Null
    } else {
        Value::Object(merged)
    }
}

fn job_json(record: &crate::slicing::JobRecord) -> Value {
    json!({
        "id": record.id,
        "name": record.name,
        "state": record.state.as_str(),
        "created_at": record.created_at,
        "started_at": record.started_at,
        "finished_at": record.finished_at,
        "duration_ms": record.duration_ms,
        "model_artifacts": record.model_artifacts,
        "essentials": record.essentials,
        "profile_summary": record.profile_summary,
        "gcode_artifact": record.gcode_artifact,
        "gcode_bytes": record.gcode_bytes,
        "error": record.error,
        "request": record.request,
        "logs": record.logs.as_ref().map(|l| json!({
            "stdout_len": l.stdout.len(),
            "stderr_len": l.stderr.len(),
        })),
    })
}
