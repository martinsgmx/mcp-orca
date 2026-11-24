//! Tools that extract configuration from a local `OrcaSlicer` installation.
//!
//! These tools are read-only except for `orca_config_setup`, which records the
//! first-time printer/filament/process selection in the managed workspace. The
//! recorded defaults are then reused by `orca_slice` when a role is omitted.

use std::{collections::BTreeMap, path::PathBuf};

use serde_json::{json, Map, Value};

use crate::{
    error::Error,
    orca_config::{
        essentials, essentials_summary, Defaults, LocalConfig, LocalPreset, ResolvedLocalPreset,
        ScanOptions, Selection,
    },
    profiles::ProfileKind,
    safety::{Capability, Risk},
    tools::{
        args::Args, json_result, no_summary, schema_object, Gate, ToolContext, ToolFuture, ToolSpec,
    },
};

const DEFAULT_MAX_PRESETS: usize = 20_000;
const MAX_MAX_PRESETS: usize = 200_000;

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "orca_config_scan",
            title: "Scan local OrcaSlicer config",
            description: "Scan the local OrcaSlicer configuration directory (user JSON presets plus system *.opc preset caches) with bounded, read-only discovery. Reports discovered roles/counts, the currently active OrcaSlicer selection, and whether first-time defaults are already saved.",
            allowed_args: &["config_dir", "include_bundled", "max_presets"],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "config_dir": {"type": "string", "description": "Explicit OrcaSlicer config directory. Defaults to OS detection (or ORCA_CONFIG_DIR)."},
                        "include_bundled": {"type": "boolean", "default": false, "description": "Also scan bundled resources/profiles (env ORCA_PROFILES_DIR / ORCA_RESOURCES_DIR)."},
                        "max_presets": {"type": "integer", "minimum": 1, "maximum": 200_000, "default": 20000}
                    }),
                    &[],
                )
            },
            handler: scan,
        },
        ToolSpec {
            name: "orca_config_profiles",
            title: "List local OrcaSlicer profiles",
            description: "List locally discovered OrcaSlicer printer/filament/process presets from user JSON and system *.opc caches, with optional kind filter and substring filter.",
            allowed_args: &["kind", "filter", "limit", "config_dir", "include_bundled", "max_presets"],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "kind": {"type": "string", "enum": ["printer", "filament", "process"], "description": "Only list this role."},
                        "filter": {"type": "string", "description": "Case-insensitive substring match on preset name or vendor."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 5000, "default": 100},
                        "config_dir": {"type": "string"},
                        "include_bundled": {"type": "boolean", "default": false},
                        "max_presets": {"type": "integer", "minimum": 1, "maximum": 200_000, "default": 20000}
                    }),
                    &[],
                )
            },
            handler: profiles,
        },
        ToolSpec {
            name: "orca_config_resolve",
            title: "Resolve local OrcaSlicer settings",
            description: "Resolve one local preset (kind + name) or a printer/filament/process selection, following inherits/includes, and extract the settings needed for a correct print. Uses the local OrcaSlicer config; nothing is invented and merged settings can be returned in full.",
            allowed_args: &[
                "kind", "name", "printer", "filament", "process", "overrides",
                "include_settings", "config_dir", "include_bundled", "max_presets",
            ],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "kind": {"type": "string", "enum": ["printer", "filament", "process"], "description": "Resolve a single preset of this role (requires name)."},
                        "name": {"type": "string", "description": "Preset name for single-preset resolution."},
                        "printer": {"type": "string"},
                        "filament": {"type": "string"},
                        "process": {"type": "string"},
                        "overrides": {"type": "object", "description": "Setting overrides merged last."},
                        "include_settings": {"type": "boolean", "default": false, "description": "Include the full resolved merged settings map."},
                        "config_dir": {"type": "string"},
                        "include_bundled": {"type": "boolean", "default": false},
                        "max_presets": {"type": "integer", "minimum": 1, "maximum": 200_000, "default": 20000}
                    }),
                    &[],
                )
            },
            handler: resolve,
        },
        ToolSpec {
            name: "orca_config_setup",
            title: "Set up local OrcaSlicer defaults",
            description: "First-time setup: detect or accept a printer/filament/process selection from the local OrcaSlicer configuration, resolve it, extract printing essentials, and save it in the workspace so orca_slice can reuse it as default essentials. Explicit arguments win over the detected OrcaSlicer selection.",
            allowed_args: &[
                "printer", "filament", "process", "overrides", "detect", "save", "reset",
                "config_dir", "include_bundled", "max_presets",
            ],
            capability: Capability::WorkspaceWrite,
            risk: Risk::Medium,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "printer": {"type": "string", "description": "Printer preset name; falls back to the detected OrcaSlicer selection."},
                        "filament": {"type": "string", "description": "Filament preset name; falls back to the detected OrcaSlicer selection."},
                        "process": {"type": "string", "description": "Process preset name; falls back to the detected OrcaSlicer selection."},
                        "overrides": {"type": "object", "description": "Default setting overrides (informational; recorded with the selection)."},
                        "detect": {"type": "boolean", "default": true, "description": "Fill omitted roles from the active OrcaSlicer selection."},
                        "save": {"type": "boolean", "default": true, "description": "Persist the defaults under the workspace."},
                        "reset": {"type": "boolean", "default": false, "description": "Remove saved defaults instead of saving."},
                        "config_dir": {"type": "string"},
                        "include_bundled": {"type": "boolean", "default": false},
                        "max_presets": {"type": "integer", "minimum": 1, "maximum": 200_000, "default": 20000}
                    }),
                    &[],
                )
            },
            handler: setup,
        },
        ToolSpec {
            name: "orca_config_defaults",
            title: "Read saved OrcaSlicer defaults",
            description: "Return the first-time printer/filament/process defaults saved in the workspace, including their resolved essentials snapshot and whether setup is complete.",
            allowed_args: &[],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || schema_object(json!({}), &[]),
            handler: defaults_info,
        },
    ]
}

struct ScanArgs {
    config_dir: Option<String>,
    include_bundled: bool,
    max_presets: usize,
}

impl ScanArgs {
    fn parse(args: &Args) -> Self {
        Self {
            config_dir: args.optional_string("config_dir"),
            include_bundled: args.optional_bool("include_bundled").unwrap_or(false),
            max_presets: args.value("max_presets").and_then(Value::as_u64).map_or(
                DEFAULT_MAX_PRESETS,
                |value| {
                    usize::try_from(value)
                        .unwrap_or(DEFAULT_MAX_PRESETS)
                        .clamp(1, MAX_MAX_PRESETS)
                },
            ),
        }
    }

    fn scan(&self) -> crate::error::Result<LocalConfig> {
        LocalConfig::scan(&ScanOptions {
            config_dir: self.config_dir.as_ref().map(PathBuf::from),
            include_bundled: self.include_bundled,
            max_presets: self.max_presets,
        })
    }

    /// Scan and fail with guidance when no configuration directory was found.
    fn scan_required(&self) -> crate::error::Result<LocalConfig> {
        let mut config = self.scan()?;
        if config.config_dir.is_none() && config.presets.is_empty() {
            let hint = "no OrcaSlicer configuration directory found. Pass config_dir (e.g. ~/.config/OrcaSlicer), set ORCA_CONFIG_DIR, or point include_bundled at the OrcaSlicer resources/profiles directory.";
            config.warnings.push(hint.to_string());
            return Err(Error::Profile(hint.into()));
        }
        Ok(config)
    }
}

fn scan(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(
            Some(args),
            &["config_dir", "include_bundled", "max_presets"],
        )?;
        let scan_args = ScanArgs::parse(&args);
        // A scan never fails only because nothing was found; report that instead.
        let config = scan_args.scan()?;
        let saved = Defaults::load(&ctx.config.workspace.root)?;
        let defaults_path = Defaults::path(&ctx.config.workspace.root);

        let roles = json!({
            "printer": config.counts.printers,
            "filament": config.counts.filaments,
            "process": config.counts.processes,
        });
        Ok(json_result(
            &json!({
                "config_dir": config.config_dir,
                "user_dirs": config.user_dirs,
                "system_dirs": config.system_dirs,
                "bundle_dirs": config.bundle_dirs,
                "counts": config.counts,
                "roles": roles,
                "current_selection": config.current_selection,
                "defaults": {
                    "path": defaults_path,
                    "configured": saved.is_some(),
                    "complete": saved.as_ref().is_some_and(Defaults::is_complete),
                    "selection": saved.as_ref().map(|d| d.selection.clone()),
                    "updated_at": saved.as_ref().and_then(|d| d.updated_at),
                },
                "warnings": config.warnings,
                "next": [
                    "orca_config_profiles: list presets for a role",
                    "orca_config_resolve: inspect merged settings for a selection",
                    "orca_config_setup: record printer/filament/process defaults for later orca_slice calls"
                ]
            }),
            None,
        ))
    })
}

fn profiles(_ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(
            Some(args),
            &[
                "kind",
                "filter",
                "limit",
                "config_dir",
                "include_bundled",
                "max_presets",
            ],
        )?;
        let scan_args = ScanArgs::parse(&args);
        let kind = parse_optional_kind(&args)?;
        let filter = args
            .optional_string("filter")
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        let limit = args
            .value("limit")
            .and_then(Value::as_u64)
            .map_or(100, |value| {
                usize::try_from(value).unwrap_or(100).clamp(1, 5000)
            });

        let config = scan_args.scan()?;
        let mut matched = 0usize;
        let mut items = Vec::new();
        for preset in &config.presets {
            if kind.is_some_and(|kind| preset.kind != kind) {
                continue;
            }
            if let Some(filter) = &filter {
                let name = preset.name.to_ascii_lowercase();
                let vendor = preset
                    .vendor
                    .as_deref()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if !name.contains(filter) && !vendor.contains(filter) {
                    continue;
                }
            }
            matched += 1;
            if items.len() < limit {
                items.push(preset_summary(preset));
            }
        }

        Ok(json_result(
            &json!({
                "config_dir": config.config_dir,
                "kind": kind.map(ProfileKind::as_str),
                "filter": filter,
                "matched": matched,
                "returned": items.len(),
                "limit": limit,
                "profiles": items,
                "warnings": config.warnings,
            }),
            None,
        ))
    })
}

fn resolved_summary(preset: &ResolvedLocalPreset) -> Value {
    json!({
        "name": preset.name,
        "kind": preset.kind,
        "found": preset.found,
        "source": preset.source,
        "path": preset.path,
        "vendor": preset.vendor,
        "inherits": preset.inherits,
        "includes": preset.includes,
        "setting_count": preset.settings.len(),
        "chain": preset.chain,
        "include_chain": preset.include_chain,
        "warnings": preset.warnings,
    })
}

fn preset_summary(preset: &LocalPreset) -> Value {
    json!({
        "name": preset.name,
        "kind": preset.kind,
        "source": preset.source,
        "vendor": preset.vendor,
        "path": preset.path,
        "inherits": preset.inherits,
        "includes": preset.includes,
        "setting_count": preset.settings.len(),
    })
}

fn parse_optional_kind(args: &Args) -> crate::error::Result<Option<ProfileKind>> {
    let Some(value) = args.optional_string("kind") else {
        return Ok(None);
    };
    let Ok(kind) = value.parse::<ProfileKind>() else {
        return Err(Error::InvalidArguments(format!(
            "unknown kind '{value}' (expected printer, filament, or process)"
        )));
    };
    if matches!(
        kind,
        ProfileKind::Printer | ProfileKind::Filament | ProfileKind::Process
    ) {
        Ok(Some(kind))
    } else {
        Err(Error::InvalidArguments(format!(
            "unsupported kind '{value}' (expected printer, filament, or process)"
        )))
    }
}

fn parse_required_kind(args: &Args) -> crate::error::Result<ProfileKind> {
    parse_optional_kind(args)?.ok_or_else(|| {
        Error::InvalidArguments("single-preset resolution requires 'kind' and 'name'".into())
    })
}

fn parse_overrides(args: &Args) -> crate::error::Result<Map<String, Value>> {
    match args.value("overrides") {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(map)) => Ok(map.clone()),
        Some(_) => Err(Error::InvalidArguments(
            "overrides must be an object".into(),
        )),
    }
}

fn selection_from_args(args: &Args) -> Selection {
    Selection {
        printer: args.optional_string("printer"),
        filament: args.optional_string("filament"),
        process: args.optional_string("process"),
    }
}

fn resolve(_ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(
            Some(args),
            &[
                "kind",
                "name",
                "printer",
                "filament",
                "process",
                "overrides",
                "include_settings",
                "config_dir",
                "include_bundled",
                "max_presets",
            ],
        )?;
        let scan_args = ScanArgs::parse(&args);
        let overrides = parse_overrides(&args)?;
        let include_settings = args.optional_bool("include_settings").unwrap_or(false);
        let config = scan_args.scan_required()?;

        // Single-preset mode when kind is supplied.
        if args.optional_string("kind").is_some() {
            let kind = parse_required_kind(&args)?;
            let name = args.required_string("name")?;
            let resolved = config.resolve(kind, &name);
            let mut selection = Selection::default();
            match kind {
                ProfileKind::Printer => selection.printer = Some(name.clone()),
                ProfileKind::Filament => selection.filament = Some(name.clone()),
                ProfileKind::Process => selection.process = Some(name.clone()),
                _ => {}
            }
            let resolved_selection = config.resolve_selection(&selection, &overrides);
            let snapshot = essentials(&resolved_selection);
            let mut payload = json!({
                "mode": "preset",
                "config_dir": config.config_dir,
                "preset": resolved_summary(&resolved),
                "essentials": snapshot,
                "warnings": resolved.warnings,
            });
            if include_settings {
                payload["settings"] = json!(resolved.settings);
            }
            return Ok(json_result(&payload, None));
        }

        let selection = selection_from_args(&args);
        if selection.printer.is_none()
            && selection.filament.is_none()
            && selection.process.is_none()
        {
            return Err(Error::InvalidArguments(
                "provide either kind+name for one preset, or printer/filament/process for a selection"
                    .into(),
            ));
        }
        let resolved = config.resolve_selection(&selection, &overrides);
        let snapshot = essentials(&resolved);
        let mut payload = json!({
            "mode": "selection",
            "config_dir": config.config_dir,
            "selection": resolved.selection,
            "missing_roles": resolved.missing,
            "printer": resolved.printer.as_ref().map(resolved_summary),
            "filament": resolved.filament.as_ref().map(resolved_summary),
            "process": resolved.process.as_ref().map(resolved_summary),
            "essentials": snapshot,
            "merged_setting_count": resolved.merged.as_object().map_or(0, Map::len),
            "warnings": resolved.warnings,
        });
        if include_settings {
            payload["settings"] = resolved.merged;
        }
        Ok(json_result(&payload, None))
    })
}

fn setup(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(
            Some(args),
            &[
                "printer",
                "filament",
                "process",
                "overrides",
                "detect",
                "save",
                "reset",
                "config_dir",
                "include_bundled",
                "max_presets",
            ],
        )?;
        let scan_args = ScanArgs::parse(&args);
        let overrides_map = parse_overrides(&args)?;
        let save = args.optional_bool("save").unwrap_or(true);
        if args.optional_bool("reset").unwrap_or(false) {
            let removed = Defaults::clear(&ctx.config.workspace.root)?;
            return Ok(json_result(
                &json!({
                    "reset": true,
                    "removed": removed,
                    "path": Defaults::path(&ctx.config.workspace.root),
                    "next": "run orca_config_setup again to record defaults"
                }),
                None,
            ));
        }

        let detect = args.optional_bool("detect").unwrap_or(true);
        let mut explicit = selection_from_args(&args);
        let explicit_used =
            explicit.printer.is_some() || explicit.filament.is_some() || explicit.process.is_some();
        let config = scan_args.scan_required()?;

        if detect {
            if let Some(detected) = &config.current_selection {
                fill_missing(&mut explicit, detected);
            }
        }
        let missing = explicit.missing();
        if !missing.is_empty() {
            let hint = if detect {
                " The active OrcaSlicer selection was incomplete; pass the missing role explicitly."
            } else {
                " Pass printer, filament, and process explicitly."
            };
            return Err(Error::InvalidArguments(format!(
                "setup requires a complete printer/filament/process selection; missing {}.{hint} Use orca_config_profiles to list candidates.",
                missing.join(", ")
            )));
        }

        let overrides: BTreeMap<String, Value> = overrides_map.clone().into_iter().collect();
        let resolved = config.resolve_selection(&explicit, &overrides_map);
        let mut unresolved = Vec::new();
        for (role, preset) in [
            ("printer", resolved.printer.as_ref()),
            ("filament", resolved.filament.as_ref()),
            ("process", resolved.process.as_ref()),
        ] {
            if preset.is_none_or(|preset| !preset.found) {
                unresolved.push(role);
            }
        }
        if !unresolved.is_empty() {
            return Err(Error::Profile(format!(
                "setup could not resolve {}; check names with orca_config_profiles",
                unresolved.join(", ")
            )));
        }

        let snapshot = essentials(&resolved);
        let source = if explicit_used { "user" } else { "detected" };
        let mut defaults = Defaults {
            config_dir: config.config_dir.clone(),
            selection: explicit.clone(),
            overrides,
            essentials: Some(snapshot.clone()),
            source: Some(source.into()),
            notes: vec![
                "Recorded by orca_config_setup from the local OrcaSlicer configuration.".into(),
                "orca_slice uses these roles when printer/filament/process are omitted.".into(),
            ],
            ..Defaults::default()
        };

        let path = if save {
            Some(defaults.save(&ctx.config.workspace.root)?)
        } else {
            None
        };

        Ok(json_result(
            &json!({
                "saved": save,
                "path": path,
                "source": source,
                "defaults": defaults,
                "essentials": snapshot,
                "essentials_summary": essentials_summary(&snapshot),
                "warnings": config.warnings,
                "next": if save {
                    "orca_slice may now omit printer/filament/process; explicit arguments still win"
                } else {
                    "re-run with save=true to persist these defaults"
                }
            }),
            None,
        ))
    })
}

fn fill_missing(selection: &mut Selection, detected: &Selection) {
    if selection.printer.is_none() {
        selection.printer.clone_from(&detected.printer);
    }
    if selection.filament.is_none() {
        selection.filament.clone_from(&detected.filament);
    }
    if selection.process.is_none() {
        selection.process.clone_from(&detected.process);
    }
}

fn defaults_info(ctx: &ToolContext, _args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let path = Defaults::path(&ctx.config.workspace.root);
        let saved = Defaults::load(&ctx.config.workspace.root)?;
        let Some(saved) = saved else {
            return Ok(json_result(
                &json!({
                    "configured": false,
                    "path": path,
                    "next": "run orca_config_setup to record printer/filament/process defaults"
                }),
                None,
            ));
        };

        Ok(json_result(
            &json!({
                "configured": true,
                "path": path,
                "complete": saved.is_complete(),
                "version": saved.version,
                "source": saved.source,
                "config_dir": saved.config_dir,
                "selection": saved.selection,
                "overrides": saved.overrides,
                "created_at": saved.created_at,
                "updated_at": saved.updated_at,
                "essentials_summary": saved.essentials.as_ref().map(essentials_summary),
                "notes": saved.notes,
            }),
            None,
        ))
    })
}
