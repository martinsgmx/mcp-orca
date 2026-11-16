//! Mesh preparation tools: inspect, transform, merge (STL).

use serde_json::{json, Value};

use crate::{
    error::{Error, Result},
    safety::{Capability, Risk},
    tools::{
        args::Args, json_result, no_summary, schema_object, Gate, ToolContext, ToolFuture, ToolSpec,
    },
};

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "orca_mesh_inspect",
            title: "Inspect mesh",
            description: "Inspect a mesh artifact and report bounds, dimensions, triangle/vertex counts, and format. Supports STL (ASCII and binary) and Wavefront OBJ (ASCII, polygons fan-triangulated).",
            allowed_args: &["artifact_id"],
            capability: Capability::Read,
            risk: Risk::Read,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || schema_object(json!({"artifact_id": {"type": "string"}}), &["artifact_id"]),
            handler: inspect,
        },
        ToolSpec {
            name: "orca_mesh_transform",
            title: "Transform mesh",
            description: "Create a derived mesh artifact by applying translate/rotate/scale (uniform or per-axis) and/or mirror to an STL or OBJ source. Transform order: scale/mirror -> rotate -> translate. Output is always a binary STL artifact.",
            allowed_args: &["artifact_id", "translate", "rotate_deg", "scale", "mirror", "name"],
            capability: Capability::WorkspaceWrite,
            risk: Risk::Low,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "artifact_id": {"type": "string"},
                        "translate": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3, "description": "[x,y,z] translation"},
                        "rotate_deg": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3, "description": "[x,y,z] rotation in degrees"},
                        "scale": {"description": "Uniform number or [x,y,z] factors", "oneOf": [{"type": "number"}, {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}]},
                        "mirror": {"type": "array", "items": {"type": "boolean"}, "minItems": 3, "maxItems": 3, "description": "[x,y,z] mirror flags"},
                        "name": {"type": "string"}
                    }),
                    &["artifact_id"],
                )
            },
            handler: transform,
        },
        ToolSpec {
            name: "orca_mesh_merge",
            title: "Merge meshes",
            description: "Combine multiple STL/OBJ mesh artifacts into one derived binary STL artifact. Triangles are concatenated in the given order without automatic placement offsets.",
            allowed_args: &["artifact_ids", "name"],
            capability: Capability::WorkspaceWrite,
            risk: Risk::Low,
            gate: Gate::Policy,
            summarize: no_summary,
            schema: || {
                schema_object(
                    json!({
                        "artifact_ids": {"type": "array", "items": {"type": "string"}, "minItems": 2},
                        "name": {"type": "string"}
                    }),
                    &["artifact_ids"],
                )
            },
            handler: merge,
        },
    ]
}

fn inspect(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(Some(args), &["artifact_id"])?;
        let id = args.required_string("artifact_id")?;
        let record = ctx.store.get(&id)?;
        if !matches!(record.kind, crate::workspace::ArtifactKind::Mesh) {
            return Err(Error::InvalidArguments(format!(
                "artifact '{id}' is not a mesh"
            )));
        }
        let bytes = ctx.store.read_bytes(&id)?;
        let stats = crate::geometry::inspect_mesh_bytes(&bytes, &record.format);
        Ok(json_result(
            &json!({
                "artifact": {"id": record.id, "name": record.name, "format": record.format, "bytes": record.bytes},
                "mesh": stats,
            }),
            None,
        ))
    })
}

fn transform(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(
            Some(args),
            &[
                "artifact_id",
                "translate",
                "rotate_deg",
                "scale",
                "mirror",
                "name",
            ],
        )?;
        let id = args.required_string("artifact_id")?;
        let record = ctx.store.get(&id)?;
        if !matches!(record.kind, crate::workspace::ArtifactKind::Mesh) {
            return Err(Error::InvalidArguments(format!(
                "artifact '{id}' is not a mesh"
            )));
        }

        let translate = vec3_arg(&args, "translate")?.unwrap_or([0.0; 3]);
        let rotate_deg = vec3_arg(&args, "rotate_deg")?.unwrap_or([0.0; 3]);
        let scale = match args.value("scale") {
            None | Some(Value::Null) => [1.0; 3],
            Some(Value::Number(n)) => {
                let v = n
                    .as_f64()
                    .ok_or_else(|| Error::InvalidArguments("scale must be a number".into()))?;
                [v; 3]
            }
            Some(value) => parse_vec3_value(value, "scale")?,
        };
        let mirror = match args.value("mirror") {
            None | Some(Value::Null) => [false; 3],
            Some(value) => {
                let array = value.as_array().ok_or_else(|| {
                    Error::InvalidArguments("mirror must be an array of 3 booleans".into())
                })?;
                if array.len() != 3 {
                    return Err(Error::InvalidArguments(
                        "mirror must be an array of 3 booleans".into(),
                    ));
                }
                let mut out = [false; 3];
                for (i, item) in array.iter().enumerate() {
                    out[i] = item.as_bool().ok_or_else(|| {
                        Error::InvalidArguments("mirror must be an array of 3 booleans".into())
                    })?;
                }
                out
            }
        };

        let spec = crate::geometry::TransformSpec {
            translate,
            rotate_deg,
            scale,
            mirror,
        };
        if spec.is_identity() {
            return Err(Error::InvalidArguments(
                "transform is a no-op; provide translate, rotate_deg, scale and/or mirror".into(),
            ));
        }
        for value in scale {
            if !value.is_finite() || value == 0.0 {
                return Err(Error::InvalidArguments(
                    "scale factors must be finite and non-zero".into(),
                ));
            }
        }

        let bytes = ctx.store.read_bytes(&id)?;
        let triangles = crate::geometry::parse_mesh(&bytes, &record.format).ok_or_else(|| {
            Error::Process(format!(
                "could not parse artifact '{id}' (format '{}'); mesh transform supports STL and OBJ",
                record.format
            ))
        })?;
        let transformed = crate::geometry::transform_triangles(&triangles, &spec);
        let output = crate::geometry::write_binary_stl(&transformed);
        let stats = crate::geometry::stats_for_triangles(&transformed);

        let name = args
            .optional_string("name")
            .unwrap_or_else(|| format!("{}-transformed.stl", record.name.trim_end_matches(".stl")));
        let derived = ctx.store.import_bytes(
            &name,
            crate::workspace::ArtifactKind::Mesh,
            "stl",
            &output,
            Some(format!("mesh transform of {id}")),
            json!({
                "derived_from": id,
                "operation": "transform",
                "translate": translate,
                "rotate_deg": rotate_deg,
                "scale": scale,
                "mirror": mirror,
            }),
        )?;

        Ok(json_result(
            &json!({
                "artifact": {"id": derived.id, "name": derived.name, "format": derived.format, "bytes": derived.bytes},
                "source_artifact": id,
                "transform": {
                    "translate": translate,
                    "rotate_deg": rotate_deg,
                    "scale": scale,
                    "mirror": mirror,
                    "order": "scale/mirror -> rotate -> translate"
                },
                "mesh": stats,
            }),
            None,
        ))
    })
}

fn merge(ctx: &ToolContext, args: Value) -> ToolFuture<'_> {
    Box::pin(async move {
        let args = Args::parse(Some(args), &["artifact_ids", "name"])?;
        let ids: Vec<String> = args
            .value("artifact_ids")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|v| {
                        v.as_str().map(ToString::to_string).ok_or_else(|| {
                            Error::InvalidArguments(
                                "artifact_ids must be an array of strings".into(),
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        if ids.len() < 2 {
            return Err(Error::InvalidArguments(
                "artifact_ids must contain at least 2 mesh artifacts".into(),
            ));
        }

        let mut meshes = Vec::with_capacity(ids.len());
        for id in &ids {
            let record = ctx.store.get(id)?;
            if !matches!(record.kind, crate::workspace::ArtifactKind::Mesh) {
                return Err(Error::InvalidArguments(format!(
                    "artifact '{id}' is not a mesh"
                )));
            }
            let bytes = ctx.store.read_bytes(id)?;
            let triangles =
                crate::geometry::parse_mesh(&bytes, &record.format).ok_or_else(|| {
                    Error::Process(format!(
                        "could not parse artifact '{id}' (format '{}'); mesh merge supports STL and OBJ",
                        record.format
                    ))
                })?;
            meshes.push(triangles);
        }

        let merged = crate::geometry::merge_triangles(&meshes);
        let output = crate::geometry::write_binary_stl(&merged);
        let stats = crate::geometry::stats_for_triangles(&merged);

        let name = args
            .optional_string("name")
            .unwrap_or_else(|| "merged.stl".to_string());
        let derived = ctx.store.import_bytes(
            &name,
            crate::workspace::ArtifactKind::Mesh,
            "stl",
            &output,
            Some(format!("mesh merge of {}", ids.join(", "))),
            json!({"derived_from": ids, "operation": "merge"}),
        )?;

        Ok(json_result(
            &json!({
                "artifact": {"id": derived.id, "name": derived.name, "format": derived.format, "bytes": derived.bytes},
                "sources": ids,
                "mesh": stats,
                "note": "Triangles are concatenated in the given order with their existing coordinates; no automatic placement offsets are applied.",
            }),
            None,
        ))
    })
}

fn vec3_arg(args: &Args, key: &str) -> Result<Option<[f64; 3]>> {
    match args.value(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => Ok(Some(parse_vec3_value(value, key)?)),
    }
}

fn parse_vec3_value(value: &Value, key: &str) -> Result<[f64; 3]> {
    let array = value
        .as_array()
        .ok_or_else(|| Error::InvalidArguments(format!("{key} must be an array of 3 numbers")))?;
    if array.len() != 3 {
        return Err(Error::InvalidArguments(format!(
            "{key} must be an array of 3 numbers"
        )));
    }
    let mut out = [0f64; 3];
    for (i, item) in array.iter().enumerate() {
        out[i] = item.as_f64().ok_or_else(|| {
            Error::InvalidArguments(format!("{key} must be an array of 3 numbers"))
        })?;
        if !out[i].is_finite() {
            return Err(Error::InvalidArguments(format!(
                "{key} values must be finite"
            )));
        }
    }
    Ok(out)
}
