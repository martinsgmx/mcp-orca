//! Artifact import, inspection, export, and deletion tools.

use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use serde_json::{json, Value};

use crate::{
    error::{Error, Result},
    safety::{Capability, Risk},
    tools::{
        args::Args, json_result, no_summary, schema_object, Gate, ToolContext, ToolFuture, ToolSpec,
    },
    workspace::{expand_tilde, ArtifactKind, ArtifactRecord},
};

const MAX_TEXT_READ_BYTES: usize = 5 * 1024 * 1024;

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "orca_artifact_import",
            title: "Import an artifact",
            description: "Import a file (from an allowed import root) or base64 data into the managed workspace. Returns a content-addressed artifact (same bytes => same id).",
            allowed_args: &["path", "data", "name", "kind", "format", "metadata"],
            capability: Capability::WorkspaceWrite,
            risk: Risk::Low,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "path": {"type": "string", "description": "Absolute path to a file under configured workspace.import_roots."},
                        "data": {"type": "string", "description": "Base64-encoded file contents (alternative to path)."},
                        "name": {"type": "string", "description": "Display name for the artifact."},
                        "kind": {"type": "string", "enum": ["mesh", "gcode", "profile", "log", "job", "other"]},
                        "format": {"type": "string", "description": "File format/extension (inferred from name/path when omitted)."},
                        "metadata": {"type": "object"}
                    }),
                    &[],
                )
            },
            handler: import,
        },
        ToolSpec {
            name: "orca_artifact_list",
            title: "List artifacts",
            description: "List stored artifacts, newest first, optionally filtered by kind and a case-insensitive substring query.",
            allowed_args: &["kind", "query", "limit"],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "kind": {"type": "string", "enum": ["mesh", "gcode", "profile", "log", "job", "other"]},
                        "query": {"type": "string"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50}
                    }),
                    &[],
                )
            },
            handler: list,
        },
        ToolSpec {
            name: "orca_artifact_get",
            title: "Get artifact metadata",
            description: "Return metadata for one artifact id.",
            allowed_args: &["artifact_id"],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || schema_object(json!({"artifact_id": {"type": "string"}}), &["artifact_id"]),
            handler: get,
        },
        ToolSpec {
            name: "orca_artifact_read_text",
            title: "Read artifact text",
            description: "Read a UTF-8 text artifact (e.g. gcode/profile) with line/byte limits.",
            allowed_args: &["artifact_id", "start_line", "max_lines", "max_bytes"],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "artifact_id": {"type": "string"},
                        "start_line": {"type": "integer", "minimum": 1, "default": 1},
                        "max_lines": {"type": "integer", "minimum": 1, "maximum": 20000, "default": 400},
                        "max_bytes": {"type": "integer", "minimum": 1, "maximum": 5_242_880, "default": 262_144}
                    }),
                    &["artifact_id"],
                )
            },
            handler: read_text,
        },
        ToolSpec {
            name: "orca_artifact_export",
            title: "Export an artifact",
            description: "Copy an artifact to a destination path under configured workspace.export_roots. May require approval per policy.",
            allowed_args: &["artifact_id", "path", "overwrite"],
            capability: Capability::Export,
            risk: Risk::High,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "artifact_id": {"type": "string"},
                        "path": {"type": "string", "description": "Absolute destination path under workspace.export_roots."},
                        "overwrite": {"type": "boolean", "default": false}
                    }),
                    &["artifact_id", "path"],
                )
            },
            handler: export,
        },
        ToolSpec {
            name: "orca_artifact_update_metadata",
            title: "Update artifact metadata",
            description: "Update non-content metadata on a workspace artifact (name, kind, metadata patch). Artifact bytes and content-addressed id are unchanged. Use this to repair misclassified kind or attach metadata; it does not rewrite file contents.",
            allowed_args: &["artifact_id", "name", "kind", "metadata_patch"],
            capability: Capability::WorkspaceWrite,
            risk: Risk::Low,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "artifact_id": {"type": "string"},
                        "name": {"type": "string", "description": "Optional new display name."},
                        "kind": {"type": "string", "enum": ["mesh", "gcode", "profile", "log", "job", "other"], "description": "Optional corrected artifact kind."},
                        "metadata_patch": {"type": "object", "description": "Object merged into existing metadata (top-level keys). If existing metadata is not an object, it is replaced."}
                    }),
                    &["artifact_id"],
                )
            },
            handler: update_metadata,
        },
        ToolSpec {
            name: "orca_artifact_delete",
            title: "Delete an artifact",
            description: "Remove an artifact from the workspace. May require approval per policy.",
            allowed_args: &["artifact_id"],
            capability: Capability::Destructive,
            risk: Risk::High,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || schema_object(json!({"artifact_id": {"type": "string"}}), &["artifact_id"]),
            handler: delete,
        },
    ]
}

fn record_json(_ctx: &ToolContext, record: &ArtifactRecord) -> Value {
    json!({
        "id": record.id,
        "sha256": record.sha256,
        "name": record.name,
        "kind": record.kind.as_str(),
        "format": record.format,
        "bytes": record.bytes,
        "created_at": record.created_at,
        "source": record.source,
        "metadata": record.metadata,
    })
}

fn import(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(
            Some(args),
            &["path", "data", "name", "kind", "format", "metadata"],
        )?;
        let requested_kind_str = args.optional_string("kind");
        let requested_kind = match requested_kind_str.as_deref() {
            Some(k) => Some(k.parse::<ArtifactKind>().map_err(Error::InvalidArguments)?),
            None => None,
        };
        let metadata = args.value("metadata").cloned().unwrap_or(Value::Null);
        let mut kind_override_reason: Option<String> = None;

        let (record, source_label) = if let Some(path_str) = args.optional_string("path") {
            let path = expand_tilde(&path_str);
            validate_import_path(ctx, &path)?;
            let name = args
                .optional_string("name")
                .or_else(|| {
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .map(ToString::to_string)
                })
                .unwrap_or_else(|| "artifact".to_string());
            let format = args.optional_string("format").unwrap_or_default();
            let effective_format = if format.is_empty() {
                path.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                format.clone()
            };
            let stored_kind = resolve_import_kind(
                requested_kind,
                &effective_format,
                &name,
                None,
                &mut kind_override_reason,
            )?;
            let rec = ctx.store.import_file(
                &name,
                stored_kind,
                &format,
                &path,
                Some(path.to_string_lossy().to_string()),
                metadata,
            )?;
            (rec, Some(path.to_string_lossy().to_string()))
        } else if let Some(data) = args.optional_string("data") {
            let name = args.optional_string("name").ok_or_else(|| {
                Error::InvalidArguments("name is required when importing from base64 data".into())
            })?;
            let format = args.optional_string("format").unwrap_or_default();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.as_bytes())
                .map_err(|e| Error::InvalidArguments(format!("invalid base64 data: {e}")))?;
            if bytes.len() > ctx.config.workspace.max_import_bytes {
                return Err(Error::Artifact(format!(
                    "data is {} bytes, above the {} byte import limit",
                    bytes.len(),
                    ctx.config.workspace.max_import_bytes
                )));
            }
            let stored_kind = resolve_import_kind(
                requested_kind,
                &format,
                &name,
                Some(&bytes),
                &mut kind_override_reason,
            )?;
            let rec = ctx.store.import_bytes(
                &name,
                stored_kind,
                &format,
                &bytes,
                Some("base64".into()),
                metadata,
            )?;
            (rec, Some("base64".into()))
        } else {
            return Err(Error::InvalidArguments(
                "provide either 'path' or 'data'".into(),
            ));
        };

        let mut result = json!({
            "artifact": record_json(ctx, &record),
            "source": source_label,
            "requested_kind": requested_kind.map(ArtifactKind::as_str),
            "stored_kind": record.kind.as_str(),
        });
        if let Some(reason) = kind_override_reason {
            result["kind_override_reason"] = json!(reason);
        }
        Ok(json_result(&result, None))
    })
}

fn list(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(Some(args), &["kind", "query", "limit"])?;
        let kind = match args.optional_string("kind") {
            Some(k) => Some(k.parse::<ArtifactKind>().map_err(Error::InvalidArguments)?),
            None => None,
        };
        let query = args.optional_string("query");
        let limit = args
            .value("limit")
            .and_then(Value::as_u64)
            .map_or(50usize, |v| v as usize)
            .clamp(1, 500);
        let records = ctx.store.list(kind, query.as_deref(), limit)?;
        let artifacts: Vec<Value> = records.iter().map(|r| record_json(ctx, r)).collect();
        Ok(json_result(
            &json!({"count": artifacts.len(), "artifacts": artifacts}),
            None,
        ))
    })
}

fn get(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(Some(args), &["artifact_id"])?;
        let id = args.required_string("artifact_id")?;
        let record = ctx.store.get(&id)?;
        Ok(json_result(
            &json!({"artifact": record_json(ctx, &record)}),
            None,
        ))
    })
}

fn read_text(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(
            Some(args),
            &["artifact_id", "start_line", "max_lines", "max_bytes"],
        )?;
        let id = args.required_string("artifact_id")?;
        let start_line = args
            .value("start_line")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let max_lines = args
            .value("max_lines")
            .and_then(Value::as_u64)
            .unwrap_or(400)
            .clamp(1, 20_000) as usize;
        let max_bytes = args
            .value("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(262_144)
            .clamp(1, MAX_TEXT_READ_BYTES as u64) as usize;

        let record = ctx.store.get(&id)?;
        if record.bytes > ctx.config.workspace.max_artifact_bytes as u64 {
            return Err(Error::Artifact("artifact too large to read as text".into()));
        }
        let bytes = ctx.store.read_bytes(&id)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| Error::Artifact("artifact is not valid UTF-8 text".into()))?;
        let lines: Vec<&str> = text.lines().collect();
        let start_idx = start_line.saturating_sub(1).min(lines.len());
        let mut out = String::new();
        let mut used = 0usize;
        let mut count = 0usize;
        for (i, line) in lines.iter().enumerate().skip(start_idx).take(max_lines) {
            let rendered = format!("{:>6}: {}\n", i + 1, line);
            if used + rendered.len() > max_bytes {
                break;
            }
            used += rendered.len();
            out.push_str(&rendered);
            count += 1;
        }
        let truncated = start_idx + count < lines.len();
        Ok(json_result(
            &json!({
                "artifact": record_json(ctx, &record),
                "start_line": start_line,
                "returned_lines": count,
                "truncated": truncated,
                "text": out,
            }),
            None,
        ))
    })
}

fn export(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(Some(args), &["artifact_id", "path", "overwrite"])?;
        let id = args.required_string("artifact_id")?;
        let path = expand_tilde(&args.required_string("path")?);
        let overwrite = args.optional_bool("overwrite").unwrap_or(false);
        validate_export_path(ctx, &path)?;
        let record = ctx.store.export(&id, &path, overwrite)?;
        Ok(json_result(
            &json!({"artifact": record_json(ctx, &record), "exported_to": path}),
            None,
        ))
    })
}

fn update_metadata(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(Some(args), &["artifact_id", "name", "kind", "metadata_patch"])?;
        let id = args.required_string("artifact_id")?;
        let before = ctx.store.get(&id)?;
        let name = args.optional_string("name");
        let kind = match args.optional_string("kind") {
            Some(k) => Some(k.parse::<ArtifactKind>().map_err(Error::InvalidArguments)?),
            None => None,
        };
        let metadata_patch = args.value("metadata_patch").cloned();
        if name.is_none() && kind.is_none() && metadata_patch.is_none() {
            return Err(Error::InvalidArguments(
                "provide at least one of name, kind, or metadata_patch".into(),
            ));
        }
        let record = ctx.store.update_metadata(&id, name, kind, metadata_patch)?;
        Ok(json_result(
            &json!({
                "updated": true,
                "artifact": record_json(ctx, &record),
                "previous": {
                    "name": before.name,
                    "kind": before.kind.as_str(),
                    "metadata": before.metadata,
                },
                "note": "Artifact bytes and content-addressed id are unchanged; only metadata was updated."
            }),
            None,
        ))
    })
}

fn delete(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(Some(args), &["artifact_id"])?;
        let id = args.required_string("artifact_id")?;
        let record = ctx.store.delete(&id)?;
        Ok(json_result(
            &json!({"deleted": true, "artifact": record_json(ctx, &record)}),
            None,
        ))
    })
}

fn resolve_import_kind(
    requested: Option<ArtifactKind>,
    format: &str,
    name: &str,
    bytes: Option<&[u8]>,
    override_reason: &mut Option<String>,
) -> Result<ArtifactKind> {
    let fmt = format.trim().trim_start_matches('.').to_ascii_lowercase();
    let name_fmt = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let eff = if fmt.is_empty() { name_fmt.as_str() } else { fmt.as_str() };

    let mesh_ext = matches!(eff, "stl" | "obj" | "3mf" | "amf" | "step" | "stp");
    let gcode_ext = matches!(eff, "gcode" | "gco" | "g" | "nc" | "ngc");
    let looks_mesh = mesh_ext || bytes.is_some_and(bytes_look_like_mesh);
    let looks_gcode = gcode_ext || bytes.is_some_and(bytes_look_like_gcode);

    if let Some(requested) = requested {
        match requested {
            ArtifactKind::Mesh => {
                if looks_mesh || eff.is_empty() {
                    // Caller explicitly requested mesh; preserve intent.
                    if !looks_mesh && eff.is_empty() {
                        *override_reason = Some(
                            "stored as mesh because caller requested kind=mesh; format/content could not be positively identified as mesh"
                                .into(),
                        );
                    }
                    return Ok(ArtifactKind::Mesh);
                }
                return Err(Error::InvalidArguments(format!(
                    "requested kind 'mesh' is incompatible with format/content ('{}')",
                    if eff.is_empty() { "unknown" } else { eff }
                )));
            }
            ArtifactKind::Gcode => {
                if looks_gcode || eff.is_empty() {
                    return Ok(ArtifactKind::Gcode);
                }
                return Err(Error::InvalidArguments(format!(
                    "requested kind 'gcode' is incompatible with format/content ('{}')",
                    if eff.is_empty() { "unknown" } else { eff }
                )));
            }
            _ => return Ok(requested),
        }
    }

    // No explicit kind: infer from obvious signals, default to Other.
    if looks_mesh {
        *override_reason = Some(format!(
            "no kind provided; inferred mesh from format/content '{}'",
            if eff.is_empty() { "detected" } else { eff }
        ));
        return Ok(ArtifactKind::Mesh);
    }
    if looks_gcode {
        *override_reason = Some(format!(
            "no kind provided; inferred gcode from format/content '{}'",
            if eff.is_empty() { "detected" } else { eff }
        ));
        return Ok(ArtifactKind::Gcode);
    }
    Ok(ArtifactKind::Other)
}

fn bytes_look_like_mesh(bytes: &[u8]) -> bool {
    if bytes.len() < 5 {
        return false;
    }
    // Binary STL: 80 header + u32 tri count + 50*n == len
    if bytes.len() >= 84 {
        let tri = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
        let expected = 84usize.saturating_add(tri.saturating_mul(50));
        if expected == bytes.len() {
            return true;
        }
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).to_ascii_lowercase();
    let trimmed = head.trim_start();
    if trimmed.starts_with("solid") && head.contains("facet") && head.contains("vertex") {
        return true;
    }
    // OBJ
    let has_v = head.lines().any(|l| l.trim_start().starts_with("v "));
    let has_f = head.lines().any(|l| l.trim_start().starts_with("f "));
    if has_v && has_f {
        return true;
    }
    // 3MF/AMF are zip/xml-based; be conservative, don't guess from bytes alone.
    false
}

fn bytes_look_like_gcode(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    let mut saw_gcode = 0u32;
    for line in head.lines().take(40) {
        let l = line.trim().to_ascii_uppercase();
        if l.is_empty() || l.starts_with(';') {
            continue;
        }
        let code = l.split(';').next().unwrap_or("").trim();
        if (code.starts_with('G') || code.starts_with('M') || code.starts_with('T'))
            && code[1..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        {
            saw_gcode += 1;
            if saw_gcode >= 2 {
                return true;
            }
        }
    }
    false
}

fn validate_import_path(ctx: &ToolContext, path: &Path) -> Result<()> {
    let abs = absolutize(path)?;
    if ctx.config.workspace.import_roots.is_empty() {
        return Err(Error::Policy(
            "path-based imports are disabled (workspace.import_roots is empty); use base64 data import".into(),
        ));
    }
    if !is_under_any(&abs, &ctx.config.workspace.import_roots) {
        return Err(Error::Policy(format!(
            "import path '{}' is outside configured workspace.import_roots",
            abs.display()
        )));
    }
    Ok(())
}

fn validate_export_path(ctx: &ToolContext, path: &Path) -> Result<()> {
    let abs = absolutize(path)?;
    if ctx.config.workspace.export_roots.is_empty() {
        return Err(Error::Policy(
            "exports are disabled (workspace.export_roots is empty). Configure workspace.export_roots in mcp-orca config to allow exports; this server does not mutate config. For same-host workflows, an explicit user-authorized filesystem copy may be used outside this tool.".into(),
        ));
    }
    if !is_under_any(&abs, &ctx.config.workspace.export_roots) {
        return Err(Error::Policy(format!(
            "export path '{}' is outside configured workspace.export_roots ({}). Choose a destination under an allowed export root or update config.",
            abs.display(),
            ctx.config.workspace.export_roots.join(", ")
        )));
    }
    Ok(())
}

fn is_under_any(path: &Path, roots: &[String]) -> bool {
    roots.iter().any(|root| {
        let root = absolutize(&expand_tilde(root)).unwrap_or_else(|_| expand_tilde(root));
        path.starts_with(root)
    })
}

fn absolutize(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    let base = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir()?
    };
    for comp in base.components().chain(path.components()) {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(std::path::MAIN_SEPARATOR_STR);
    }
    Ok(out)
}
