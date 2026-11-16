//! Profile discovery, inspection, resolution, and workspace custom profile tools.

use std::path::Path;

use serde_json::{json, Map, Value};

use crate::{
    error::Error,
    profiles::ProfileKind,
    safety::{Capability, Risk},
    tools::{
        args::Args, json_result, no_summary, schema_object, Gate, ToolContext, ToolFuture, ToolSpec,
    },
};

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "orca_profile_list",
            title: "List profiles/presets",
            description: "List discovered OrcaSlicer presets/profiles from bounded sources (explicit paths + OS-detected common defaults, if enabled) and workspace custom profiles. No whole-system scan.",
            allowed_args: &["kind", "query", "limit"],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "kind": {"type": "string", "enum": ["printer", "filament", "process", "project", "bundle", "unknown"]},
                        "query": {"type": "string", "description": "Case-insensitive substring matched against name/id/path."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 5000, "default": 200}
                    }),
                    &[],
                )
            },
            handler: list,
        },
        ToolSpec {
            name: "orca_profile_get",
            title: "Get profile/preset",
            description: "Get one discovered profile entry by id, path, or name. Returns indexed metadata; native preset parsing is left to the target engine adapter.",
            allowed_args: &["profile_id"],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || schema_object(json!({"profile_id": {"type": "string"}}), &["profile_id"]),
            handler: get,
        },
        ToolSpec {
            name: "orca_profile_refresh",
            title: "Refresh profile index",
            description: "Rescan bounded profile discovery sources and workspace custom profiles.",
            allowed_args: &[],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: crate::tools::empty_schema,
            handler: refresh,
        },
        ToolSpec {
            name: "orca_profile_resolve",
            title: "Resolve effective settings",
            description: "Resolve effective settings from selected printer/filament/process presets and overrides. Requires essentials (printer+filament+process) unless a supported project is given. Workspace custom profile artifacts (art_*) contribute settings; native presets are resolved by reference; overrides merge last.",
            allowed_args: &["printer", "filament", "process", "profiles", "overrides", "project"],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "printer": {"type": "string"},
                        "filament": {"type": "string"},
                        "process": {"type": "string"},
                        "profiles": {"type": "array", "items": {"type": "string"}},
                        "overrides": {"type": "object", "description": "Setting overrides by key (typed/known + pass-through)."},
                        "project": {"type": "string", "description": "Optional project/profile path (e.g. .3mf) if supported by target."}
                    }),
                    &[],
                )
            },
            handler: resolve,
        },
        ToolSpec {
            name: "orca_profile_create",
            title: "Create custom profile/override (workspace)",
            description: "Create a workspace-managed custom profile/override artifact. Stored in workspace; no mutation of user/system Orca installs.",
            allowed_args: &["name", "kind", "base_profile", "settings", "metadata"],
            capability: Capability::WorkspaceWrite,
            risk: Risk::Low,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "name": {"type": "string"},
                        "kind": {"type": "string", "enum": ["printer", "filament", "process", "unknown"]},
                        "base_profile": {"type": "string"},
                        "settings": {"type": "object"},
                        "metadata": {"type": "object"}
                    }),
                    &["name"],
                )
            },
            handler: create_custom,
        },
        ToolSpec {
            name: "orca_profile_update",
            title: "Update custom profile/override (workspace)",
            description: "Update a workspace-managed custom profile artifact by creating a new version (content-addressed). External on-disk profiles are never edited in place.",
            allowed_args: &["artifact_id", "name", "kind", "base_profile", "settings", "metadata"],
            capability: Capability::WorkspaceWrite,
            risk: Risk::Low,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "artifact_id": {"type": "string", "description": "Existing custom profile artifact id to base update on (optional)."},
                        "name": {"type": "string"},
                        "kind": {"type": "string", "enum": ["printer", "filament", "process", "unknown"]},
                        "base_profile": {"type": "string"},
                        "settings": {"type": "object"},
                        "metadata": {"type": "object"}
                    }),
                    &["name"],
                )
            },
            handler: update_custom,
        },
    ]
}

fn list(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(Some(args), &["kind", "query", "limit"])?;
        let kind = match args.optional_string("kind") {
            Some(k) => Some(k.parse::<ProfileKind>().map_err(Error::InvalidArguments)?),
            None => None,
        };
        let query = args
            .optional_string("query")
            .map(|q| q.to_ascii_lowercase());
        let limit = args
            .value("limit")
            .and_then(Value::as_u64)
            .unwrap_or(200)
            .clamp(1, 5000) as usize;

        let catalog = ctx.profiles.catalog()?;
        let mut entries = Vec::new();
        for e in &catalog.entries {
            if let Some(k) = kind {
                if e.kind != k {
                    continue;
                }
            }
            if let Some(q) = &query {
                let hay = format!("{} {} {} {}", e.id, e.name, e.path, e.kind.as_str())
                    .to_ascii_lowercase();
                if !hay.contains(q) {
                    continue;
                }
            }
            entries.push(json!({
                "id": e.id,
                "name": e.name,
                "kind": e.kind.as_str(),
                "path": e.path,
                "format": e.format,
                "bytes": e.bytes,
                "modified_at": e.modified_at,
                "source": e.source,
            }));
            if entries.len() >= limit {
                break;
            }
        }

        Ok(json_result(
            &json!({
                "scanned_dirs": catalog.scanned_dirs,
                "count": entries.len(),
                "entries": entries,
                "warnings": catalog.warnings,
            }),
            None,
        ))
    })
}

fn get(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(Some(args), &["profile_id"])?;
        let id = args.required_string("profile_id")?;

        // Workspace-managed custom profiles are artifacts, not discovered files.
        if id.starts_with("art_") {
            if let Ok(record) = ctx.store.get(&id) {
                if matches!(record.kind, crate::workspace::ArtifactKind::Profile) {
                    let payload: Value = serde_json::from_slice(&ctx.store.read_bytes(&id)?)
                        .unwrap_or_else(|_| json!({}));
                    return Ok(json_result(
                        &json!({
                            "profile": {
                                "id": record.id,
                                "name": record.name,
                                "kind": record.kind.as_str(),
                                "format": record.format,
                                "bytes": record.bytes,
                                "source": "workspace_artifact",
                                "base_profile": payload.get("base_profile").cloned().unwrap_or(Value::Null),
                                "settings": payload.get("settings").cloned().unwrap_or(Value::Null),
                                "metadata": payload.get("metadata").cloned().unwrap_or(Value::Null),
                            },
                            "note": "Workspace-managed custom profile artifact; settings are merged by orca_profile_resolve."
                        }),
                        None,
                    ));
                }
            }
        }

        let catalog = ctx.profiles.catalog()?;
        let entry = crate::profiles::ProfileIndex::find_entry(&catalog, &id).ok_or_else(|| {
            Error::Profile(format!(
                "profile '{id}' not found; try orca_profile_refresh"
            ))
        })?;
        Ok(json_result(
            &json!({
                "profile": {
                    "id": entry.id,
                    "name": entry.name,
                    "kind": entry.kind.as_str(),
                    "path": entry.path,
                    "format": entry.format,
                    "bytes": entry.bytes,
                    "modified_at": entry.modified_at,
                    "source": entry.source,
                },
                "note": "Native preset parsing is target-dependent; use orca_profile_resolve for merged workspace settings and overrides."
            }),
            None,
        ))
    })
}

fn refresh(ctx: &ToolContext, _args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let catalog = ctx.profiles.refresh()?;
        Ok(json_result(
            &json!({
                "scanned_dirs": catalog.scanned_dirs,
                "count": catalog.entries.len(),
                "warnings": catalog.warnings,
            }),
            None,
        ))
    })
}

fn resolve(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(
            Some(args),
            &[
                "printer",
                "filament",
                "process",
                "profiles",
                "overrides",
                "project",
            ],
        )?;
        let printer = args.optional_string("printer");
        let filament = args.optional_string("filament");
        let process = args.optional_string("process");
        let project = args.optional_string("project");
        let overrides = args.value("overrides").cloned().unwrap_or(json!({}));

        // v1 invariant: essentials required (printer + filament + process) unless a project fully provides them (target-dependent).
        let missing: Vec<&str> = [
            ("printer", printer.is_none()),
            ("filament", filament.is_none()),
            ("process", process.is_none()),
        ]
        .into_iter()
        .filter_map(|(k, m)| if m { Some(k) } else { None })
        .collect();

        if !missing.is_empty() && project.is_none() {
            return Err(Error::Profile(format!(
                "cannot resolve essentials: missing {}. Provide printer, filament, and process presets (ids/names/paths/workspace artifacts) or a supported project; no invented defaults.",
                missing.join(", ")
            )));
        }

        let catalog = ctx.profiles.catalog()?;
        let mut warnings = Vec::new();
        let mut effective: Map<String, Value> = Map::new();
        let mut applied_from: Vec<Value> = Vec::new();

        for (role, value) in [
            ("printer", printer.clone()),
            ("filament", filament.clone()),
            ("process", process.clone()),
        ] {
            let Some(value) = value else { continue };
            let (info, settings) = resolve_selection(ctx, &catalog, &value, &mut warnings);
            applied_from.push(json!({"role": role, "selection": info}));
            if let Some(settings) = settings {
                for (key, val) in settings {
                    effective.insert(key, val);
                }
            }
        }

        if let Value::Object(map) = overrides.clone() {
            for (key, val) in map {
                effective.insert(key, val);
            }
        } else if !overrides.is_null() {
            warnings.push("overrides was not an object and was ignored".into());
        }

        // Additional profile layers are passed through for the engine adapter.
        let profiles = args
            .value("profiles")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let settings_complete = applied_from
            .iter()
            .all(|v| v["selection"]["settings_source"].as_str() == Some("workspace_artifact"));

        Ok(json_result(
            &json!({
                "status": if warnings.is_empty() { "resolved" } else { "resolved_with_warnings" },
                "resolved": true,
                "selection": {
                    "printer": printer,
                    "filament": filament,
                    "process": process,
                    "project": project,
                    "profiles": profiles,
                },
                "resolution": applied_from,
                "overrides": overrides,
                "effective_settings": if effective.is_empty() { Value::Null } else { Value::Object(effective) },
                "settings_complete": settings_complete,
                "warnings": warnings,
                "discovery": {
                    "os_defaults_enabled": ctx.config.profiles.use_os_defaults,
                    "search_paths": ctx.config.profiles.search_paths,
                    "indexed_entries": catalog.entries.len(),
                },
                "notes": [
                    "Workspace custom profile artifacts (art_*) contribute their settings. Native Orca preset formats are indexed/resolved by reference only; parse them with the target CLI (engine.slice_command).",
                    "Overrides are merged last and take precedence."
                ]
            }),
            None,
        ))
    })
}

fn resolve_selection(
    ctx: &ToolContext,
    catalog: &crate::profiles::ProfileCatalog,
    value: &str,
    warnings: &mut Vec<String>,
) -> (Value, Option<Map<String, Value>>) {
    if value.starts_with("art_") {
        match ctx.store.get(value) {
            Ok(record) => {
                let settings = ctx
                    .store
                    .read_bytes(value)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .and_then(|payload| {
                        payload.get("settings").and_then(Value::as_object).cloned()
                    });
                let settings_source = if settings.is_some() {
                    "workspace_artifact"
                } else {
                    "workspace_artifact_without_settings"
                };
                if settings.is_none() {
                    warnings.push(format!(
                        "workspace artifact '{value}' has no parsable 'settings' object"
                    ));
                }
                return (
                    json!({
                        "input": value,
                        "source": "workspace_artifact",
                        "artifact_id": record.id,
                        "name": record.name,
                        "kind": record.kind.as_str(),
                        "format": record.format,
                        "settings_source": settings_source,
                    }),
                    settings,
                );
            }
            Err(_) => {
                warnings.push(format!(
                    "workspace artifact '{value}' was not found; passing selection through unresolved"
                ));
            }
        }
    }

    if let Some(entry) = catalog.get(value) {
        return (
            json!({
                "input": value,
                "source": "discovered_profile",
                "profile_id": entry.id,
                "name": entry.name,
                "kind": entry.kind.as_str(),
                "path": entry.path,
                "format": entry.format,
                "settings_source": "native_reference",
            }),
            None,
        );
    }

    if Path::new(value).is_file() {
        return (
            json!({
                "input": value,
                "source": "explicit_path",
                "path": value,
                "settings_source": "native_reference",
            }),
            None,
        );
    }

    warnings.push(format!(
        "selection '{value}' was not found in the profile index and is not a file; passing it to the engine unresolved"
    ));
    (
        json!({
            "input": value,
            "source": "unresolved",
            "settings_source": "none",
        }),
        None,
    )
}

fn create_custom(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(
            Some(args),
            &["name", "kind", "base_profile", "settings", "metadata"],
        )?;
        let name = args.required_string("name")?;
        let kind = args
            .optional_string("kind")
            .unwrap_or_else(|| "unknown".to_string());
        let base_profile = args.optional_string("base_profile");
        let settings = args.value("settings").cloned().unwrap_or(json!({}));
        let metadata = args.value("metadata").cloned().unwrap_or(json!({}));

        // Store as a workspace-managed custom profile artifact (server-defined JSON).
        let payload = json!({
            "schema": "mcp-orca.custom-profile.v1",
            "name": name,
            "kind": kind,
            "base_profile": base_profile,
            "settings": settings,
            "metadata": metadata,
            "note": "Server-defined workspace format; never written into user/system Orca installs.",
        });
        let bytes = serde_json::to_vec_pretty(&payload)?;
        let file_name = format!("{}.orca-profile.json", sanitize_simple(&name));
        let record = ctx.store.import_bytes(
            &file_name,
            crate::workspace::ArtifactKind::Profile,
            "json",
            &bytes,
            Some("workspace:custom-profile".into()),
            json!({"custom_profile": true, "kind": kind}),
        )?;

        Ok(json_result(
            &json!({
                "created": true,
                "artifact": {
                    "id": record.id,
                    "name": record.name,
                    "kind": record.kind.as_str(),
                    "format": record.format,
                    "bytes": record.bytes,
                },
                "workspace_profiles_dir": ctx.store.profiles_dir(),
                "safety": "Stored in workspace; does not mutate user/system Orca installs. Any future in-place edit must create a timestamped backup first."
            }),
            None,
        ))
    })
}

fn update_custom(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(
            Some(args),
            &[
                "artifact_id",
                "name",
                "kind",
                "base_profile",
                "settings",
                "metadata",
            ],
        )?;
        let prior = if let Some(id) = args.optional_string("artifact_id") {
            Some(ctx.store.get(&id)?)
        } else {
            None
        };
        let name = args
            .optional_string("name")
            .or_else(|| prior.as_ref().map(|r| r.name.clone()))
            .unwrap_or_else(|| "custom-profile".into());
        let kind = args
            .optional_string("kind")
            .unwrap_or_else(|| "unknown".to_string());
        let base_profile = args.optional_string("base_profile");
        let settings = args.value("settings").cloned().unwrap_or(json!({}));
        let metadata = args.value("metadata").cloned().unwrap_or(json!({}));

        let payload = json!({
            "schema": "mcp-orca.custom-profile.v1",
            "name": name,
            "kind": kind,
            "base_profile": base_profile,
            "settings": settings,
            "metadata": metadata,
            "updated_from": prior.as_ref().map(|r| &r.id),
            "note": "Workspace-managed custom profile version. Does not mutate user/system Orca installs.",
        });
        let bytes = serde_json::to_vec_pretty(&payload)?;
        let file_name = format!("{}.orca-profile.json", sanitize_simple(&name));
        let record = ctx.store.import_bytes(
            &file_name,
            crate::workspace::ArtifactKind::Profile,
            "json",
            &bytes,
            Some("workspace:custom-profile-update".into()),
            json!({"custom_profile": true, "kind": kind}),
        )?;

        Ok(json_result(
            &json!({
                "updated": true,
                "artifact": {
                    "id": record.id,
                    "name": record.name,
                    "kind": record.kind.as_str(),
                    "format": record.format,
                    "bytes": record.bytes,
                },
                "previous_artifact_id": prior.map(|r| r.id),
                "safety": "New workspace artifact created (content-addressed). No in-place mutation of external profiles."
            }),
            None,
        ))
    })
}

fn sanitize_simple(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        s = "custom".into();
    }
    if s.len() > 80 {
        s.truncate(80);
    }
    s
}
