//! Local `OrcaSlicer` configuration extraction.
//!
//! This module reads the `OrcaSlicer` configuration that already exists on the
//! machine and turns it into structures `mcp-orca` can use:
//!
//! * [`opc`] parses the binary per-vendor preset caches (`system/*.opc`).
//! * [`local`] discovers user JSON presets and system caches, then resolves
//!   `inherits`/`includes` chains into effective settings.
//! * [`defaults`] records a first-time printer/filament/process selection in the
//!   managed workspace so later calls can reuse it as a default.
//!
//! Discovery is bounded and read-only: it never crawls the filesystem and never
//! writes into the `OrcaSlicer` installation.

pub mod defaults;
pub mod local;
pub mod opc;

use serde_json::{json, Map, Value};

use crate::orca_config::local::{ResolvedPreset, ResolvedSelection};

pub use defaults::Defaults;
pub use local::{
    bundled_profile_dirs, detect_config_dir, ChainStep, LocalConfig, LocalCounts, LocalPreset,
    LocalSource, ResolvedPreset as ResolvedLocalPreset,
    ResolvedSelection as ResolvedLocalSelection, ScanOptions, Selection,
};
pub use opc::{opc_files_in, vendor_from_path, OpcFile, OpcPreset};

/// A role's advisory essential keys.
struct EssentialKeys {
    role: &'static str,
    required: &'static [&'static str],
    optional: &'static [&'static str],
}

const ESSENTIAL_KEYS: &[EssentialKeys] = &[
    EssentialKeys {
        role: "printer",
        required: &[
            "nozzle_diameter",
            "printable_area",
            "printable_height",
            "gcode_flavor",
            "machine_start_gcode",
            "machine_end_gcode",
        ],
        optional: &[
            "printer_model",
            "printer_variant",
            "printer_structure",
            "printer_technology",
            "nozzle_type",
            "nozzle_volume",
            "extruder_count",
            "single_extruder_multi_material",
            "use_relative_e_distances",
            "bed_shape",
            "max_print_height",
            "layer_change_gcode",
            "time_lapse_gcode",
            "change_filament_gcode",
            "auxiliary_fan",
            "thumbnails",
        ],
    },
    EssentialKeys {
        role: "filament",
        required: &[
            "filament_type",
            "filament_diameter",
            "nozzle_temperature",
            "hot_plate_temp",
        ],
        optional: &[
            "filament_density",
            "filament_cost",
            "filament_flow_ratio",
            "filament_max_volumetric_speed",
            "filament_shrink",
            "filament_colour",
            "filament_settings_id",
            "filament_ids",
            "nozzle_temperature_initial_layer",
            "nozzle_temperature_range_low",
            "nozzle_temperature_range_high",
            "hot_plate_temp_initial_layer",
            "cool_plate_temp",
            "cool_plate_temp_initial_layer",
            "textured_plate_temp",
            "textured_plate_temp_initial_layer",
            "chamber_temperature",
            "chamber_min_temp",
            "pressure_advance",
            "fan_min_speed",
            "fan_max_speed",
            "fan_cooling_layer_time",
            "slow_down_layer_time",
            "filament_soluble",
            "filament_is_support",
        ],
    },
    EssentialKeys {
        role: "process",
        required: &[
            "layer_height",
            "wall_loops",
            "sparse_infill_density",
            "sparse_infill_pattern",
        ],
        optional: &[
            "initial_layer_print_height",
            "top_shell_layers",
            "bottom_shell_layers",
            "top_shell_thickness",
            "internal_solid_infill_pattern",
            "outer_wall_speed",
            "inner_wall_speed",
            "initial_layer_speed",
            "travel_speed",
            "infill_speed",
            "gap_fill_speed",
            "bridge_speed",
            "support_speed",
            "support_enable",
            "support_type",
            "support_style",
            "support_threshold_angle",
            "brim_width",
            "brim_type",
            "skirt_loops",
            "skirt_distance",
            "seam_position",
            "elephant_foot_compensation",
            "ironing_type",
            "retraction_length",
            "retraction_speed",
            "z_hop",
            "default_acceleration",
            "outer_wall_acceleration",
            "travel_acceleration",
            "compatible_printers",
            "compatible_printers_condition",
            "print_settings_id",
        ],
    },
];

/// Extract the settings `mcp-orca` needs for a correct print from a resolved
/// selection.
///
/// Returns one object per role with the essential settings that were found and
/// an advisory `missing_required` list. Nothing is invented: a key absent from
/// the resolved `OrcaSlicer` presets stays absent.
#[must_use]
pub fn essentials(resolved: &ResolvedSelection) -> Value {
    let mut roles = Map::new();
    for spec in ESSENTIAL_KEYS {
        let preset = match spec.role {
            "printer" => resolved.printer.as_ref(),
            "filament" => resolved.filament.as_ref(),
            "process" => resolved.process.as_ref(),
            _ => None,
        };
        roles.insert(spec.role.to_string(), role_essentials(spec, preset));
    }
    json!({ "roles": roles })
}

fn role_essentials(spec: &EssentialKeys, preset: Option<&ResolvedPreset>) -> Value {
    let mut settings = Map::new();
    let mut missing_required = Vec::new();
    if let Some(preset) = preset {
        for key in spec.required {
            match preset.settings.get(*key) {
                Some(value) => {
                    settings.insert((*key).to_string(), value.clone());
                }
                None => missing_required.push((*key).to_string()),
            }
        }
        for key in spec.optional {
            if let Some(value) = preset.settings.get(*key) {
                settings.insert((*key).to_string(), value.clone());
            }
        }
    } else {
        missing_required.extend(spec.required.iter().map(|key| (*key).to_string()));
    }

    json!({
        "resolved": preset.is_some(),
        "settings": settings,
        "missing_required": missing_required,
    })
}

/// Compact summary of the essential snapshot for result payloads.
#[must_use]
pub fn essentials_summary(snapshot: &Value) -> Value {
    let mut out = Map::new();
    if let Some(roles) = snapshot.get("roles").and_then(Value::as_object) {
        for (role, value) in roles {
            out.insert(
                role.clone(),
                json!({
                    "resolved": value.get("resolved").cloned().unwrap_or(Value::Bool(false)),
                    "setting_count": value
                        .get("settings")
                        .and_then(Value::as_object)
                        .map_or(0, Map::len),
                    "missing_required": value
                        .get("missing_required")
                        .cloned()
                        .unwrap_or_else(|| json!([])),
                }),
            );
        }
    }
    Value::Object(Map::from_iter([("roles".to_string(), Value::Object(out))]))
}
