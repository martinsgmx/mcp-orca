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
        description: "Inspect a G-code artifact and report computed metrics (heuristic, from commands/markers) and slicer summary metrics (from G-code comments when present). Prefer slicer_summary for user-facing layer/time/filament reporting when available. Reports discrepancies as warnings. Does not execute G-code.",
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

        let slicer_summary = stats.slicer_summary.clone();
        let has_summary = slicer_summary.as_ref().is_some_and(|s| {
            s.layers.is_some()
                || s.filament_used_mm.is_some()
                || s.filament_used_cm3.is_some()
                || s.filament_used_g.is_some()
                || s.estimated_time_secs.is_some()
        });
        let source_used = if has_summary {
            "dual: computed + slicer_summary (prefer slicer_summary for user-facing totals)"
        } else {
            "computed_only (heuristic; no slicer summary comments found)"
        };

        let computed = json!({
            "layers": stats.layers,
            "filament_used_mm": stats.filament_used_mm,
            "estimated_time_secs": stats.estimated_time_secs,
            "nozzle_temperature_max_c": stats.nozzle_temperature_max_c,
            "bed_temperature_max_c": stats.bed_temperature_max_c,
            "extrude_moves": stats.extrude_moves,
            "travel_moves": stats.travel_moves,
            "move_bounds_min": stats.move_bounds_min,
            "move_bounds_max": stats.move_bounds_max,
            "source": "heuristic_gcode_commands_and_markers",
        });

        let mut warnings = stats.warnings.clone();
        if !has_summary {
            warnings.push(
                "No slicer summary comments found; computed values are heuristic and may differ from slicer estimates."
                    .into(),
            );
        }

        Ok(json_result(
            &json!({
                "artifact": {"id": record.id, "name": record.name, "format": record.format, "bytes": record.bytes},
                "analyzed": true,
                "source_used": source_used,
                "computed": computed,
                "slicer_summary": slicer_summary,
                "safety": stats.safety,
                "warnings": warnings,
                "gcode": stats,
                "notes": [
                    "Computed fields are derived from G-code commands/markers and may be heuristic.",
                    "When slicer_summary is present, prefer its layers/filament/time values for user-facing reporting.",
                    "Safety fields are heuristic scans for dangerous/common commands; they do not validate toolpaths or execute G-code."
                ]
            }),
            None,
        ))
    })
}
