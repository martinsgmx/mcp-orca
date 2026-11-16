//! G-code artifact inspection helpers.
//!
//! This is a conservative, format-agnostic analysis: it reads the common
//! `G-code` dialect `OrcaSlicer` emits (G0/G1 moves, M82/M83 extrusion modes,
//! M104/M109/M140/M190 temperatures, `;LAYER_CHANGE` markers) and reports
//! counts, temperature maxima, filament usage, move bounds, and layer markers.
//! It does not simulate or execute G-code.

use serde::Serialize;

/// Maximum bytes analyzed by [`analyze_gcode`]; larger inputs report size only.
pub const MAX_ANALYZE_BYTES: u64 = 64 * 1024 * 1024;

/// Aggregate statistics for one G-code artifact.
#[derive(Debug, Clone, Serialize, Default)]
pub struct GcodeStats {
    pub lines: u64,
    pub commands: u64,
    pub comments: u64,
    pub layers: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_temperature_max_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bed_temperature_max_c: Option<f64>,
    pub filament_used_mm: f64,
    pub extrude_moves: u64,
    pub travel_moves: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_bounds_min: Option<[f64; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_bounds_max: Option<[f64; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_time_secs: Option<f64>,
    pub truncated: bool,
}

/// Analyze G-code text. Input is lossily decoded as UTF-8.
#[must_use]
pub fn analyze_gcode(bytes: &[u8]) -> GcodeStats {
    let text = String::from_utf8_lossy(bytes);
    let mut stats = GcodeStats::default();
    let mut absolute_extrusion = true;
    let mut last_e: f64 = 0.0;
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    let mut saw_move = false;

    for raw in text.lines() {
        stats.lines += 1;
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix(';') {
            stats.comments += 1;
            let comment_upper = comment.to_ascii_uppercase();
            if comment_upper.contains("LAYER_CHANGE") {
                stats.layers += 1;
            } else if let Some(rest) = comment_upper.strip_prefix("TIME:") {
                if let Ok(secs) = rest.trim().parse::<f64>() {
                    stats.estimated_time_secs = Some(secs);
                }
            }
            continue;
        }

        // Strip inline comments before tokenizing.
        let code = line.split(';').next().unwrap_or("").trim();
        if code.is_empty() {
            stats.comments += 1;
            continue;
        }
        stats.commands += 1;

        let mut tokens = code.split_whitespace();
        let Some(command) = tokens.next() else {
            continue;
        };
        let command = command.to_ascii_uppercase();
        let mut params: Vec<(char, f64)> = Vec::new();
        for token in tokens {
            let mut chars = token.chars();
            let Some(axis) = chars.next() else { continue };
            if let Ok(value) = chars.as_str().parse::<f64>() {
                params.push((axis.to_ascii_uppercase(), value));
            }
        }
        let get = |axis: char| params.iter().find(|(a, _)| *a == axis).map(|(_, v)| *v);

        match command.as_str() {
            "M82" => absolute_extrusion = true,
            "M83" => absolute_extrusion = false,
            "G92" => {
                if let Some(e) = get('E') {
                    last_e = e;
                }
            }
            "M104" | "M109" => {
                if let Some(temp) = get('S') {
                    stats.nozzle_temperature_max_c =
                        Some(stats.nozzle_temperature_max_c.map_or(temp, |m| m.max(temp)));
                }
            }
            "M140" | "M190" => {
                if let Some(temp) = get('S') {
                    stats.bed_temperature_max_c =
                        Some(stats.bed_temperature_max_c.map_or(temp, |m| m.max(temp)));
                }
            }
            "G0" | "G1" | "G2" | "G3" => {
                for (axis, index) in [('X', 0usize), ('Y', 1), ('Z', 2)] {
                    if let Some(value) = get(axis) {
                        min[index] = min[index].min(value);
                        max[index] = max[index].max(value);
                        saw_move = true;
                    }
                }
                match get('E') {
                    Some(e) => {
                        let delta = if absolute_extrusion { e - last_e } else { e };
                        if delta > 0.0 {
                            stats.filament_used_mm += delta;
                            stats.extrude_moves += 1;
                        }
                        if absolute_extrusion {
                            last_e = e;
                        }
                    }
                    None => stats.travel_moves += 1,
                }
            }
            _ => {}
        }
    }

    if saw_move {
        stats.move_bounds_min = Some(min);
        stats.move_bounds_max = Some(max);
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_temps_layers_extrusion_and_bounds() {
        let gcode = b";LAYER_CHANGE\nM104 S210\nM140 S60\nG1 X0 Y0 Z0.2\nG1 X10 E1.5\nG92 E0\nG1 X20 E2\nM83\nG1 X30 E0.5\n";
        let stats = analyze_gcode(gcode);
        assert_eq!(stats.layers, 1);
        assert_eq!(stats.nozzle_temperature_max_c, Some(210.0));
        assert_eq!(stats.bed_temperature_max_c, Some(60.0));
        assert!((stats.filament_used_mm - 4.0).abs() < 1e-9);
        assert_eq!(stats.extrude_moves, 3);
        assert_eq!(stats.travel_moves, 1);
        assert_eq!(stats.move_bounds_min, Some([0.0, 0.0, 0.2]));
        assert_eq!(stats.move_bounds_max, Some([30.0, 0.0, 0.2]));
    }

    #[test]
    fn handles_empty_input() {
        let stats = analyze_gcode(b"");
        assert_eq!(stats.lines, 0);
        assert_eq!(stats.commands, 0);
        assert!(stats.filament_used_mm.abs() < f64::EPSILON);
        assert!(stats.move_bounds_min.is_none());
    }
}
