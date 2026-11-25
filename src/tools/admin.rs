//! Admin, approval, and introspection tools.

use serde_json::{json, Value};

use crate::{
    error::Error,
    safety::{Capability, Risk},
    tools::{
        args::Args, json_result, no_summary, schema_object, Gate, ToolContext, ToolFuture, ToolSpec,
    },
};

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "orca_server_info",
            title: "Server info",
            description: "Return server and configuration summary.",
            allowed_args: &[],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || schema_object(json!({}), &[]),
            handler: server_info,
        },
        ToolSpec {
            name: "orca_engine_info",
            title: "Engine info",
            description: "Return engine runner probe/configuration and availability.",
            allowed_args: &[],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || schema_object(json!({}), &[]),
            handler: engine_info,
        },
        ToolSpec {
            name: "orca_policy_info",
            title: "Policy info",
            description: "Return effective policy and approval settings.",
            allowed_args: &[],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || schema_object(json!({}), &[]),
            handler: policy_info,
        },
        ToolSpec {
            name: "orca_pending_approvals",
            title: "Pending approvals",
            description: "List pending, unexpired approval requests for gated actions.",
            allowed_args: &[],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || schema_object(json!({}), &[]),
            handler: pending_approvals,
        },
        ToolSpec {
            name: "orca_approval_decide",
            title: "Decide an approval request",
            description: "Approve or deny a pending approval request. After approval, retry the identical gated call within the approval TTL; the approval is consumed once.",
            allowed_args: &["approval_id", "approve", "note"],
            capability: Capability::WorkspaceWrite,
            risk: Risk::Medium,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "approval_id": {"type": "string"},
                        "approve": {"type": "boolean"},
                        "note": {"type": "string"}
                    }),
                    &["approval_id", "approve"],
                )
            },
            handler: approval_decide,
        },
        ToolSpec {
            name: "orca_doctor",
            title: "Doctor",
            description: "Run configuration and environment diagnostics.",
            allowed_args: &[],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || schema_object(json!({}), &[]),
            handler: doctor,
        },
    ]
}

fn server_info(ctx: &ToolContext, _args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        Ok(json_result(
            &json!({
                "name": "mcp-orca",
                "version": crate::VERSION,
                "config": ctx.config.public_view(),
                "status": "ready"
            }),
            None,
        ))
    })
}

fn engine_info(ctx: &ToolContext, _args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let probe = ctx.engine.probe().await;
        Ok(json_result(
            &serde_json::to_value(probe).unwrap_or(json!({})),
            None,
        ))
    })
}

fn policy_info(ctx: &ToolContext, _args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let export_roots = &ctx.config.workspace.export_roots;
        let import_roots = &ctx.config.workspace.import_roots;
        let export_possible = !export_roots.is_empty() && ctx.config.policy.allow_export;
        let import_possible = !import_roots.is_empty();
        Ok(json_result(
            &json!({
                "policy": ctx.config.policy,
                "approval": {"mode": ctx.config.approval.mode, "ttl_secs": ctx.config.approval.ttl_secs},
                "workspace": {
                    "export_possible": export_possible,
                    "export_roots_configured": !export_roots.is_empty(),
                    "export_roots": export_roots,
                    "export_block_reason": if export_roots.is_empty() {
                        Some("workspace.export_roots is empty; configure at least one allowed export root to enable orca_artifact_export")
                    } else if !ctx.config.policy.allow_export {
                        Some("policy.allow_export is false")
                    } else {
                        None
                    },
                    "import_possible": import_possible,
                    "import_roots_configured": !import_roots.is_empty(),
                    "import_roots": import_roots,
                    "import_note": if import_roots.is_empty() {
                        "Path-based imports are disabled (workspace.import_roots is empty); base64 data import still works."
                    } else {
                        "Path-based imports are allowed only under configured import_roots; base64 data import also works."
                    }
                },
                "notes": [
                    "Export and destructive actions require approval by policy: pending request, decide with orca_approval_decide, then retry the identical call.",
                    "approval.mode=in_band (default) or both allows MCP decisions; out_of_band requires `mcp-orca approvals approve|deny <id>` on the host.",
                    "No printer communication and no G-code execution."
                ]
            }),
            None,
        ))
    })
}

fn pending_approvals(ctx: &ToolContext, _args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let pending = ctx.approvals.list_pending()?;
        let items: Vec<Value> = pending
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "tool": r.tool,
                    "reason": r.reason,
                    "created_at": r.created_at,
                    "expires_at": r.expires_at,
                })
            })
            .collect();
        Ok(json_result(
            &json!({"count": items.len(), "pending": items}),
            None,
        ))
    })
}

fn approval_decide(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        if matches!(
            ctx.config.approval.mode,
            crate::config::ApprovalMode::OutOfBand
        ) {
            return Err(Error::Approval(
                "approval mode is out_of_band: decisions are not accepted over MCP. Use `mcp-orca approvals approve <id>` or `mcp-orca approvals deny <id>` on the host."
                    .into(),
            ));
        }
        let args = Args::parse(Some(args), &["approval_id", "approve", "note"])?;
        let id = args.required_string("approval_id")?;
        let approve = args
            .optional_bool("approve")
            .ok_or_else(|| Error::InvalidArguments("missing or invalid 'approve'".into()))?;
        let note = args.optional_string("note");
        let request = ctx.approvals.decide(&id, approve, note)?;
        Ok(json_result(
            &json!({
                "approval": {
                    "id": request.id,
                    "tool": request.tool,
                    "status": request.status.as_str(),
                    "created_at": request.created_at,
                    "expires_at": request.expires_at,
                    "note": request.note,
                },
                "next": if approve {
                    "retry the identical gated call within the approval TTL; it will consume this approval"
                } else {
                    "the gated action remains blocked"
                }
            }),
            None,
        ))
    })
}

fn doctor(ctx: &ToolContext, _args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let probe = ctx.engine.probe().await;
        let profiles = ctx.profiles.catalog().ok();
        let profile_count = profiles.as_ref().map_or(0, |c| c.entries.len());
        let pending = ctx.approvals.list_pending().map(|p| p.len()).unwrap_or(0);
        let export_roots = &ctx.config.workspace.export_roots;
        let import_roots = &ctx.config.workspace.import_roots;
        let export_possible = !export_roots.is_empty() && ctx.config.policy.allow_export;
        let export_detail = if export_roots.is_empty() {
            "export possible: no, reason: workspace.export_roots is empty. Configure export_roots in config to enable orca_artifact_export; this server does not mutate config."
        } else if !ctx.config.policy.allow_export {
            "export possible: no, reason: policy.allow_export is false."
        } else {
            "export possible: yes (approval-gated per policy)."
        };
        Ok(json_result(
            &json!({
                "ok": true,
                "checks": [
                    {"name": "config", "ok": true},
                    {"name": "workspace", "ok": true, "root": ctx.store.root()},
                    {"name": "engine", "ok": probe.available, "runner": probe.runner, "binary": probe.binary, "detected_binary": probe.detected_binary, "resolved_path": probe.resolved_path, "docker_image": probe.docker_image, "version": probe.version, "detail": probe.detail},
                    {"name": "profiles", "ok": true, "indexed_entries": profile_count, "note": "bounded discovery (explicit + OS-detected defaults if enabled), no whole-system scan"},
                    {"name": "export", "ok": export_possible, "export_possible": export_possible, "export_roots": export_roots, "detail": export_detail},
                    {"name": "import", "ok": true, "path_import_possible": !import_roots.is_empty(), "import_roots": import_roots, "detail": if import_roots.is_empty() { "Path-based imports disabled (workspace.import_roots empty); base64 data import is available." } else { "Path-based imports enabled under configured import_roots; base64 data import is available." }},
                    {"name": "approvals", "ok": true, "pending": pending, "note": "gated calls create pending requests; decide with orca_approval_decide and retry the identical call"},
                    {"name": "audit", "ok": true, "path": ctx.audit.path(), "note": "append-only JSONL; one record per registry call"}
                ],
                "notes": [
                    "Slice execution requires engine.preset=\"orca\" or engine.slice_command, plus a reachable binary/image for the configured runner.",
                    "Essentials required for slicing: printer + filament + process (no invented defaults).",
                    export_detail
                ]
            }),
            None,
        ))
    })
}
