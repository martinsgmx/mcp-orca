//! G-code inspection tools.

use serde_json::{json, Value};

use crate::{
    error::Error,
    gcode::{analyze_gcode, MAX_ANALYZE_BYTES},
    safety::{Capability, Risk},
    tools::{
        args::Args, json_result, no_summary, schema_object, Gate, ToolContext, ToolFuture, ToolSpec,
    },
};

pub fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "orca_gcode_inspect",
        title: "Inspect G-code",
        description: "Inspect a G-code artifact and report line/command counts, layer markers, nozzle/bed temperature maxima, filament usage, extrusion/travel move counts, move bounds, and header time estimates when present. Does not execute G-code.",
        allowed_args: &["artifact_id"],
        capability: Capability::Read,
        risk: Risk::Read,
        gate: Gate::Policy,
        summarize: no_summary,
        schema: || schema_object(json!({"artifact_id": {"type": "string"}}), &["artifact_id"]),
        handler: inspect,
    }]
}

fn inspect(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(Some(args), &["artifact_id"])?;
        let id = args.required_string("artifact_id")?;
        let record = ctx.store.get(&id)?;
        if !matches!(record.kind, crate::workspace::ArtifactKind::Gcode) {
            return Err(Error::InvalidArguments(format!(
                "artifact '{id}' is not gcode"
            )));
        }
        if record.bytes > MAX_ANALYZE_BYTES {
            return Ok(json_result(
                &json!({
                    "artifact": {"id": record.id, "name": record.name, "format": record.format, "bytes": record.bytes},
                    "analyzed": false,
                    "reason": format!(
                        "artifact is {} bytes; analysis is limited to {} bytes",
                        record.bytes, MAX_ANALYZE_BYTES
                    ),
                }),
                None,
            ));
        }
        let bytes = ctx.store.read_bytes(&id)?;
        let stats = analyze_gcode(&bytes);
        Ok(json_result(
            &json!({
                "artifact": {"id": record.id, "name": record.name, "format": record.format, "bytes": record.bytes},
                "analyzed": true,
                "gcode": stats,
                "notes": ["Statistics are derived from common G-code commands/markers; they do not validate toolpaths or execute G-code."]
            }),
            None,
        ))
    })
}
