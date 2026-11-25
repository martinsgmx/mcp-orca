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

/// Slicer-emitted summary values parsed from G-code comments/config blocks.
/// These are preferred for user-facing reporting when present.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SlicerSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layers: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_used_mm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_used_cm3: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_used_g: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_time_secs: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub keys_found: Vec<String>,
}

/// Aggregate statistics for one G-code artifact.
#[derive(Debug, Clone, Serialize, Default)]
pub struct GcodeStats {
    pub lines: u64,
    pub commands: u64,
    pub comments: u64,
    /// Computed layer marker count (`;LAYER_CHANGE` etc.), heuristic.
    pub layers: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_temperature_max_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bed_temperature_max_c: Option<f64>,
    /// Computed filament extrusion length from E moves, heuristic.
    pub filament_used_mm: f64,
    pub extrude_moves: u64,
    pub travel_moves: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_bounds_min: Option<[f64; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_bounds_max: Option<[f64; 3]>,
    /// Computed/parsed time estimate (legacy `;TIME:`), heuristic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_time_secs: Option<f64>,
    pub truncated: bool,
    /// Slicer summary values parsed from comments, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slicer_summary: Option<SlicerSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
    /// Safety-relevant command presence flags (heuristic scan, not execution).
    pub safety: GcodeSafety,
}

/// Safety-relevant signals from G-code scanning.
#[derive(Debug, Clone, Serialize, Default)]
pub struct GcodeSafety {
    pub has_m112: bool,
    pub has_m500: bool,
    pub has_m501: bool,
    pub has_m502: bool,
    pub g28_count: u64,
    pub has_m84: bool,
    pub has_m104_s0: bool,
    pub has_m140_s0: bool,
    pub heater_off_present: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

/// Analyze G-code text. Input is lossily decoded as UTF-8.
#[must_use]
pub fn analyze_gcode(bytes: &[u8]) -> GcodeStats {
    let text = String::from_utf8_lossy(bytes);
    let mut stats = GcodeStats::default();
    let mut summary = SlicerSummary::default();
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
            parse_slicer_summary_comment(comment, &mut summary);
            continue;
        }

        // Strip inline comments before tokenizing.
        let (code_part, inline_comment) = match line.split_once(';') {
            Some((code, comment)) => (code.trim(), Some(comment)),
            None => (line, None),
        };
        if let Some(comment) = inline_comment {
            parse_slicer_summary_comment(comment, &mut summary);
        }
        if code_part.is_empty() {
            stats.comments += 1;
            continue;
        }
        stats.commands += 1;

        let mut tokens = code_part.split_whitespace();
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
                    if temp <= 0.0 {
                        stats.safety.has_m104_s0 = true;
                    }
                }
            }
            "M140" | "M190" => {
                if let Some(temp) = get('S') {
                    stats.bed_temperature_max_c =
                        Some(stats.bed_temperature_max_c.map_or(temp, |m| m.max(temp)));
                    if temp <= 0.0 {
                        stats.safety.has_m140_s0 = true;
                    }
                }
            }
            "G28" => {
                stats.safety.g28_count += 1;
            }
            "M84" | "M18" => {
                stats.safety.has_m84 = true;
            }
            "M112" => {
                stats.safety.has_m112 = true;
            }
            "M500" => {
                stats.safety.has_m500 = true;
            }
            "M501" => {
                stats.safety.has_m501 = true;
            }
            "M502" => {
                stats.safety.has_m502 = true;
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

    finalize_summary(&mut stats, summary);
    finalize_safety(&mut stats);
    stats
}

fn finalize_safety(stats: &mut GcodeStats) {
    stats.safety.heater_off_present = stats.safety.has_m104_s0 || stats.safety.has_m140_s0;
    if stats.safety.has_m112 {
        stats.safety.warnings.push(
            "Contains M112 (emergency stop). Review before sending to a printer.".into(),
        );
    }
    if stats.safety.has_m500 {
        stats.safety.warnings.push(
            "Contains M500 (save settings to EEPROM). Review before sending to a printer.".into(),
        );
    }
    if stats.safety.has_m502 {
        stats.safety.warnings.push(
            "Contains M502 (reset settings). Review before sending to a printer.".into(),
        );
    }
    if stats.safety.g28_count > 0 {
        stats.safety.warnings.push(format!(
            "Contains G28 (home) {} time(s); confirm homing is safe for the target printer/state.",
            stats.safety.g28_count
        ));
    }
}

fn finalize_summary(stats: &mut GcodeStats, mut summary: SlicerSummary) {
    summary.keys_found.sort();
    summary.keys_found.dedup();
    let has_summary = summary.layers.is_some()
        || summary.filament_used_mm.is_some()
        || summary.filament_used_cm3.is_some()
        || summary.filament_used_g.is_some()
        || summary.estimated_time_secs.is_some();
    if !has_summary {
        stats.slicer_summary = None;
        return;
    }

    // Discrepancy warnings when computed and slicer summary differ materially.
    if let Some(sl) = summary.layers {
        let computed = stats.layers;
        if computed != sl {
            stats.warnings.push(format!(
                "layer count differs: computed={computed} (from LAYER_CHANGE markers), slicer_summary={sl} (from comments); prefer slicer_summary when present"
            ));
        }
    }
    if let Some(sf) = summary.filament_used_mm {
        let computed = stats.filament_used_mm;
        let tol = (sf.abs() * 0.02).max(1.0);
        if (computed - sf).abs() > tol {
            stats.warnings.push(format!(
                "filament used differs: computed={computed:.3}mm (from E moves, heuristic), slicer_summary={sf:.3}mm (from comments); prefer slicer_summary when present"
            ));
        }
    }
    if let Some(st) = summary.estimated_time_secs {
        if let Some(ct) = stats.estimated_time_secs {
            let tol = (st.abs() * 0.05).max(30.0);
            if (ct - st).abs() > tol {
                stats.warnings.push(format!(
                    "estimated time differs: computed={ct:.0}s, slicer_summary={st:.0}s; prefer slicer_summary when present"
                ));
            }
        }
    }

    stats.slicer_summary = Some(summary);
}

fn parse_slicer_summary_comment(comment: &str, summary: &mut SlicerSummary) {
    let raw = comment.trim();
    let lower = raw.to_ascii_lowercase();

    // Common OrcaSlicer/PrusaSlicer/Slic3r summary formats.
    if let Some(v) = parse_kv_u64(&lower, "total layer number") {
        summary.layers = Some(v);
        summary.keys_found.push("total layer number".into());
        return;
    }
    if let Some(v) = parse_kv_u64(&lower, "total layers count") {
        summary.layers = Some(v);
        summary.keys_found.push("total layers count".into());
        return;
    }
    if let Some(v) = parse_kv_f64(&lower, "filament used [mm]") {
        summary.filament_used_mm = Some(v);
        summary.keys_found.push("filament used [mm]".into());
        return;
    }
    if let Some(v) = parse_kv_f64(&lower, "filament used [cm3]") {
        summary.filament_used_cm3 = Some(v);
        summary.keys_found.push("filament used [cm3]".into());
        return;
    }
    if let Some(v) = parse_kv_f64(&lower, "filament used [g]") {
        summary.filament_used_g = Some(v);
        summary.keys_found.push("filament used [g]".into());
        return;
    }
    if lower.contains("estimated printing time") {
        if let Some(secs) = parse_time_value(&lower) {
            summary.estimated_time_secs = Some(secs);
            summary.keys_found.push("estimated printing time".into());
        }
        return;
    }

    // Also support "filament used = 1234.5mm" style
    if lower.starts_with("filament used") && !lower.contains("[mm]") && !lower.contains("[cm3]") && !lower.contains("[g]") {
        if let Some((_, rhs)) = split_kv(&lower) {
            let rhs = rhs.trim();
            if let Some(num) = rhs.strip_suffix("mm") {
                if let Ok(v) = num.trim().parse::<f64>() {
                    summary.filament_used_mm = Some(v);
                    summary.keys_found.push("filament used".into());
                }
            }
        }
    }
}

fn split_kv(s: &str) -> Option<(&str, &str)> {
    if let Some((k, v)) = s.split_once('=') {
        return Some((k.trim(), v.trim()));
    }
    if let Some((k, v)) = s.split_once(':') {
        return Some((k.trim(), v.trim()));
    }
    None
}

fn parse_kv_u64(lower: &str, key: &str) -> Option<u64> {
    let (k, v) = split_kv(lower)?;
    if k != key && !k.starts_with(key) {
        return None;
    }
    // only exact key match to avoid false positives
    if k != key {
        return None;
    }
    v.split_whitespace()
        .next()
        .and_then(|n| n.parse::<u64>().ok())
}

fn parse_kv_f64(lower: &str, key: &str) -> Option<f64> {
    let (k, v) = split_kv(lower)?;
    if k != key {
        return None;
    }
    v.split_whitespace()
        .next()
        .and_then(|n| n.parse::<f64>().ok())
}

fn parse_time_value(lower: &str) -> Option<f64> {
    let (_, rhs) = split_kv(lower)?;
    let rhs = rhs.trim();
    // Try plain seconds
    if let Ok(secs) = rhs.parse::<f64>() {
        return Some(secs);
    }
    // Formats like "1h 2m 3s", "1d 2h 3m 4s", "12m 34s"
    let mut total = 0f64;
    let mut saw = false;
    let mut num = String::new();
    for ch in rhs.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
        } else if ch.is_ascii_alphabetic() {
            if num.is_empty() {
                continue;
            }
            let Ok(n) = num.parse::<f64>() else {
                num.clear();
                continue;
            };
            num.clear();
            match ch {
                'd' => {
                    total += n * 86400.0;
                    saw = true;
                }
                'h' => {
                    total += n * 3600.0;
                    saw = true;
                }
                'm' => {
                    total += n * 60.0;
                    saw = true;
                }
                's' => {
                    total += n;
                    saw = true;
                }
                _ => {}
            }
        } else if !num.is_empty() && ch.is_whitespace() {
            // number without unit - ambiguous, ignore unless nothing saw
        }
    }
    if saw {
        Some(total)
    } else {
        None
    }
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
        assert!(stats.slicer_summary.is_none());
    }

    #[test]
    fn parses_slicer_summary_and_reports_discrepancy() {
        let gcode = b";FLAVOR:Marlin\n;LAYER_CHANGE\nG1 X0 Y0 E1\n;LAYER_CHANGE\nG1 X1 Y1 E1\n; total layer number: 277\n; total layers count = 277\n; filament used [mm] = 1298.53\n; estimated printing time (normal mode) = 48m 17s\n";
        let stats = analyze_gcode(gcode);
        assert_eq!(stats.layers, 2); // computed from markers
        let summary = stats.slicer_summary.expect("summary present");
        assert_eq!(summary.layers, Some(277));
        assert_eq!(summary.filament_used_mm, Some(1298.53));
        assert_eq!(summary.estimated_time_secs, Some(2897.0));
        assert!(stats.warnings.iter().any(|w| w.contains("layer count differs")));
        assert!(stats
            .warnings
            .iter()
            .any(|w| w.contains("filament used differs")));
    }
}
