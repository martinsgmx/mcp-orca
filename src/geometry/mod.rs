//! Mesh inspection and preparation helpers.
//!
//! Current support: STL (ASCII + binary) and Wavefront OBJ (ASCII) input.
//! Inspection is format-preserving; transform/merge always emit binary STL.
//! Other mesh formats are reported as unsupported rather than guessed at.

use std::io::{Cursor, Read};

use serde::Serialize;

/// One triangle with three vertices, millimetres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Triangle {
    pub vertices: [[f32; 3]; 3],
}

/// Affine transform request applied in this order: scale/mirror -> rotate -> translate.
#[derive(Debug, Clone)]
pub struct TransformSpec {
    pub translate: [f64; 3],
    pub rotate_deg: [f64; 3],
    pub scale: [f64; 3],
    pub mirror: [bool; 3],
}

impl Default for TransformSpec {
    fn default() -> Self {
        Self {
            translate: [0.0; 3],
            rotate_deg: [0.0; 3],
            scale: [1.0; 3],
            mirror: [false; 3],
        }
    }
}

impl TransformSpec {
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.translate.iter().all(|v| *v == 0.0)
            && self.rotate_deg.iter().all(|v| *v == 0.0)
            && self.scale.iter().all(|v| (*v - 1.0).abs() < f64::EPSILON)
            && self.mirror.iter().all(|v| !*v)
    }

    fn matrix(&self) -> [[f64; 3]; 3] {
        let scale_x = if self.mirror[0] {
            -self.scale[0]
        } else {
            self.scale[0]
        };
        let scale_y = if self.mirror[1] {
            -self.scale[1]
        } else {
            self.scale[1]
        };
        let scale_z = if self.mirror[2] {
            -self.scale[2]
        } else {
            self.scale[2]
        };

        let ([sin_x, sin_y, sin_z], [cos_x, cos_y, cos_z]) = (
            self.rotate_deg.map(f64::to_radians).map(f64::sin),
            self.rotate_deg.map(f64::to_radians).map(f64::cos),
        );

        // Rotate X, then Y, then Z, then scale when multiplying a column vector.
        let rot = [
            [
                cos_z * cos_y,
                cos_z * sin_y * sin_x - sin_z * cos_x,
                cos_z * sin_y * cos_x + sin_z * sin_x,
            ],
            [
                sin_z * cos_y,
                sin_z * sin_y * sin_x + cos_z * cos_x,
                sin_z * sin_y * cos_x - cos_z * sin_x,
            ],
            [-sin_y, cos_y * sin_x, cos_y * cos_x],
        ];

        [
            [
                rot[0][0] * scale_x,
                rot[0][1] * scale_y,
                rot[0][2] * scale_z,
            ],
            [
                rot[1][0] * scale_x,
                rot[1][1] * scale_y,
                rot[1][2] * scale_z,
            ],
            [
                rot[2][0] * scale_x,
                rot[2][1] * scale_y,
                rot[2][2] * scale_z,
            ],
        ]
    }
}

/// Parse STL bytes into triangles. Returns `None` for non-STL or malformed input.
#[must_use]
pub fn parse_stl(bytes: &[u8]) -> Option<Vec<Triangle>> {
    if let Some(triangles) = parse_binary_stl(bytes) {
        return Some(triangles);
    }
    parse_ascii_stl(bytes)
}

/// Parse a mesh by format hint (`stl` or `obj`), falling back to sniffing.
#[must_use]
pub fn parse_mesh(bytes: &[u8], format_hint: &str) -> Option<Vec<Triangle>> {
    let fmt = format_hint.trim().to_ascii_lowercase();
    if fmt == "obj" {
        return parse_obj(bytes);
    }
    if fmt == "stl" || looks_like_stl(bytes) {
        return parse_stl(bytes);
    }
    parse_stl(bytes).or_else(|| parse_obj(bytes))
}

/// Parse Wavefront OBJ text into triangles (fan-triangulating polygons).
#[must_use]
pub fn parse_obj(bytes: &[u8]) -> Option<Vec<Triangle>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut triangles = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("v ") {
            let mut parts = rest.split_whitespace();
            let x = parts.next()?.parse::<f32>().ok()?;
            let y = parts.next()?.parse::<f32>().ok()?;
            let z = parts.next()?.parse::<f32>().ok()?;
            positions.push([x, y, z]);
        } else if let Some(rest) = line.strip_prefix("f ") {
            let indices: Vec<usize> = rest
                .split_whitespace()
                .map(|token| {
                    let index = token.split('/').next().unwrap_or(token);
                    let raw = index.parse::<i64>().ok()?;
                    match raw.cmp(&0) {
                        std::cmp::Ordering::Greater => Some(raw as usize - 1),
                        std::cmp::Ordering::Less => {
                            positions.len().checked_sub(raw.unsigned_abs() as usize)
                        }
                        std::cmp::Ordering::Equal => None,
                    }
                })
                .collect::<Option<Vec<_>>>()?;
            if indices.len() < 3 {
                continue;
            }
            let first = *positions.get(indices[0])?;
            for pair in indices[1..].windows(2) {
                let second = *positions.get(pair[0])?;
                let third = *positions.get(pair[1])?;
                triangles.push(Triangle {
                    vertices: [first, second, third],
                });
            }
        }
    }
    if triangles.is_empty() {
        None
    } else {
        Some(triangles)
    }
}

fn parse_binary_stl(bytes: &[u8]) -> Option<Vec<Triangle>> {
    if bytes.len() < 84 {
        return None;
    }
    let count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
    if 84usize.checked_add(count.checked_mul(50)?)? != bytes.len() {
        return None;
    }
    let mut triangles = Vec::with_capacity(count);
    let mut cursor = Cursor::new(&bytes[84..]);
    for _ in 0..count {
        let mut rec = [0u8; 50];
        cursor.read_exact(&mut rec).ok()?;
        let mut vertices = [[0f32; 3]; 3];
        let mut off = 12usize;
        for vertex in &mut vertices {
            for value in vertex.iter_mut() {
                *value = f32::from_le_bytes([rec[off], rec[off + 1], rec[off + 2], rec[off + 3]]);
                off += 4;
            }
        }
        triangles.push(Triangle { vertices });
    }
    Some(triangles)
}

fn parse_ascii_stl(bytes: &[u8]) -> Option<Vec<Triangle>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let lower = text.trim_start().to_ascii_lowercase();
    if !lower.starts_with("solid") {
        return None;
    }
    let mut vertices: Vec<[f32; 3]> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.len() >= 6 && line[..6].eq_ignore_ascii_case("vertex") {
            let mut parts = line.split_whitespace();
            parts.next();
            let x = parts.next()?.parse::<f32>().ok()?;
            let y = parts.next()?.parse::<f32>().ok()?;
            let z = parts.next()?.parse::<f32>().ok()?;
            vertices.push([x, y, z]);
        }
    }
    if vertices.is_empty() || vertices.len() % 3 != 0 {
        return None;
    }
    Some(
        vertices
            .chunks_exact(3)
            .map(|chunk| Triangle {
                vertices: [chunk[0], chunk[1], chunk[2]],
            })
            .collect(),
    )
}

/// Translate/rotate/scale/mirror triangles.
#[must_use]
pub fn transform_triangles(triangles: &[Triangle], spec: &TransformSpec) -> Vec<Triangle> {
    let m = spec.matrix();
    triangles
        .iter()
        .map(|triangle| {
            let mut vertices = [[0f32; 3]; 3];
            for (src, dst) in triangle.vertices.iter().zip(vertices.iter_mut()) {
                for axis in 0..3 {
                    dst[axis] = (m[axis][0] * f64::from(src[0])
                        + m[axis][1] * f64::from(src[1])
                        + m[axis][2] * f64::from(src[2])
                        + spec.translate[axis]) as f32;
                }
            }
            Triangle { vertices }
        })
        .collect()
}

/// Concatenate triangle lists without placement changes.
#[must_use]
pub fn merge_triangles(meshes: &[Vec<Triangle>]) -> Vec<Triangle> {
    let total: usize = meshes.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    for mesh in meshes {
        out.extend_from_slice(mesh);
    }
    out
}

/// Serialize triangles as binary STL with recomputed facet normals.
#[must_use]
pub fn write_binary_stl(triangles: &[Triangle]) -> Vec<u8> {
    let mut out = Vec::with_capacity(84 + triangles.len() * 50);
    let mut header = [0u8; 80];
    let tag = b"mcp-orca binary STL";
    header[..tag.len()].copy_from_slice(tag);
    out.extend_from_slice(&header);
    out.extend_from_slice(&(triangles.len() as u32).to_le_bytes());
    for triangle in triangles {
        let normal = facet_normal(triangle);
        for value in normal {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for vertex in triangle.vertices {
            for value in vertex {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    out
}

fn facet_normal(triangle: &Triangle) -> [f32; 3] {
    let [p0, p1, p2] = triangle.vertices;
    let edge_a = [p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]];
    let edge_b = [p2[0] - p0[0], p2[1] - p0[1], p2[2] - p0[2]];
    let cross = [
        edge_a[1] * edge_b[2] - edge_a[2] * edge_b[1],
        edge_a[2] * edge_b[0] - edge_a[0] * edge_b[2],
        edge_a[0] * edge_b[1] - edge_a[1] * edge_b[0],
    ];
    let len = (cross[0] * cross[0] + cross[1] * cross[1] + cross[2] * cross[2]).sqrt();
    if len > f32::EPSILON {
        [cross[0] / len, cross[1] / len, cross[2] / len]
    } else {
        [0.0, 0.0, 0.0]
    }
}

/// Bounds and counts for a triangle list.
#[must_use]
pub fn stats_for_triangles(triangles: &[Triangle]) -> MeshStats {
    if triangles.is_empty() {
        return MeshStats {
            format: Some("stl".into()),
            triangles: Some(0),
            vertices: Some(0),
            notes: vec!["mesh contains no triangles".into()],
            ..Default::default()
        };
    }
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for triangle in triangles {
        for vertex in triangle.vertices {
            for axis in 0..3 {
                let v = f64::from(vertex[axis]);
                min[axis] = min[axis].min(v);
                max[axis] = max[axis].max(v);
            }
        }
    }
    MeshStats {
        format: Some("stl (binary)".into()),
        triangles: Some(triangles.len() as u64),
        vertices: Some(triangles.len() as u64 * 3),
        bounds_min: Some(min),
        bounds_max: Some(max),
        dimensions: Some([max[0] - min[0], max[1] - min[1], max[2] - min[2]]),
        notes: vec!["Vertex count is per-triangle (not deduplicated).".into()],
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MeshStats {
    pub format: Option<String>,
    pub triangles: Option<u64>,
    pub vertices: Option<u64>,
    pub bounds_min: Option<[f64; 3]>,
    pub bounds_max: Option<[f64; 3]>,
    pub dimensions: Option<[f64; 3]>,
    pub notes: Vec<String>,
}

/// Best-effort mesh inspection from bytes (currently STL-focused).
#[must_use]
pub fn inspect_mesh_bytes(bytes: &[u8], format_hint: &str) -> MeshStats {
    let fmt = format_hint.trim().to_ascii_lowercase();
    if fmt == "stl" || looks_like_stl(bytes) {
        if let Some(stats) = inspect_binary_stl(bytes) {
            return stats;
        }
        if let Some(stats) = inspect_ascii_stl(bytes) {
            return stats;
        }
        return MeshStats {
            format: Some("stl".into()),
            notes: vec!["STL detected but could not be parsed reliably".into()],
            ..Default::default()
        };
    }
    if fmt == "obj" || looks_like_obj(bytes) {
        if let Some(triangles) = parse_obj(bytes) {
            let mut stats = stats_for_triangles(&triangles);
            stats.format = Some("obj (ascii)".into());
            return stats;
        }
        return MeshStats {
            format: Some("obj".into()),
            notes: vec!["OBJ detected but could not be parsed reliably".into()],
            ..Default::default()
        };
    }
    MeshStats {
        format: if fmt.is_empty() { None } else { Some(fmt) },
        notes: vec!["Mesh inspection supports STL and OBJ; other formats are not parsed.".into()],
        ..Default::default()
    }
}

fn looks_like_obj(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    let has_vertex = head.lines().any(|l| l.trim_start().starts_with("v "));
    let has_face = head.lines().any(|l| l.trim_start().starts_with("f "));
    has_vertex && has_face
}

fn looks_like_stl(bytes: &[u8]) -> bool {
    if bytes.len() >= 84 {
        // heuristic: binary STL has 80-byte header + u32 tri count and expected size
        let tri = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
        let expected = 84usize.saturating_add(tri.saturating_mul(50));
        if expected == bytes.len() {
            return true;
        }
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(256)]).to_ascii_lowercase();
    head.trim_start().starts_with("solid") && head.contains("facet")
}

fn inspect_binary_stl(bytes: &[u8]) -> Option<MeshStats> {
    if bytes.len() < 84 {
        return None;
    }
    let tri_count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
    let expected = 84usize.checked_add(tri_count.checked_mul(50)?)?;
    if expected != bytes.len() {
        return None;
    }
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    let mut cursor = Cursor::new(&bytes[84..]);
    for _ in 0..tri_count {
        let mut rec = [0u8; 50];
        if cursor.read_exact(&mut rec).is_err() {
            return None;
        }
        // skip normal (12 bytes), read 3 vertices * 3 f32
        let mut off = 12usize;
        for _v in 0..3 {
            for axis in 0..3 {
                let f = f64::from(f32::from_le_bytes([
                    rec[off],
                    rec[off + 1],
                    rec[off + 2],
                    rec[off + 3],
                ]));
                if f < min[axis] {
                    min[axis] = f;
                }
                if f > max[axis] {
                    max[axis] = f;
                }
                off += 4;
            }
        }
    }
    let dims = [max[0] - min[0], max[1] - min[1], max[2] - min[2]];
    Some(MeshStats {
        format: Some("stl (binary)".into()),
        triangles: Some(tri_count as u64),
        vertices: Some(tri_count as u64 * 3),
        bounds_min: Some(min),
        bounds_max: Some(max),
        dimensions: Some(dims),
        notes: vec!["Vertex count is per-triangle (not deduplicated).".into()],
    })
}

fn inspect_ascii_stl(bytes: &[u8]) -> Option<MeshStats> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    let mut tri_count: u64 = 0;
    let mut vertex_count: u64 = 0;
    for line in text.lines() {
        let l = line.trim();
        if l.to_ascii_lowercase().starts_with("facet ") {
            tri_count += 1;
        } else if l.to_ascii_lowercase().starts_with("vertex ") {
            let parts: Vec<&str> = l.split_whitespace().collect();
            if parts.len() >= 4 {
                for axis in 0..3 {
                    if let Ok(v) = parts[axis + 1].parse::<f64>() {
                        if v < min[axis] {
                            min[axis] = v;
                        }
                        if v > max[axis] {
                            max[axis] = v;
                        }
                    }
                }
                vertex_count += 1;
            }
        }
    }
    if tri_count == 0 && vertex_count == 0 {
        return None;
    }
    let has_bounds = min.iter().all(|v| v.is_finite()) && max.iter().all(|v| v.is_finite());
    let (bounds_min, bounds_max, dimensions) = if has_bounds {
        (
            Some(min),
            Some(max),
            Some([max[0] - min[0], max[1] - min[1], max[2] - min[2]]),
        )
    } else {
        (None, None, None)
    };
    Some(MeshStats {
        format: Some("stl (ascii)".into()),
        triangles: Some(tri_count),
        vertices: Some(vertex_count),
        bounds_min,
        bounds_max,
        dimensions,
        notes: vec!["Vertex count is per-triangle (not deduplicated).".into()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspect_ascii_stl_basic() {
        let stl = b"solid t
  facet normal 0 0 1
    outer loop
      vertex 0 0 0
      vertex 1 0 0
      vertex 0 1 0
    endloop
  endfacet
endsolid t
";
        let s = inspect_mesh_bytes(stl, "stl");
        assert_eq!(s.triangles, Some(1));
        assert_eq!(s.vertices, Some(3));
        assert!(s.bounds_max.is_some());
    }

    fn unit_triangle() -> Vec<Triangle> {
        vec![Triangle {
            vertices: [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        }]
    }

    #[test]
    fn parses_obj_quad_and_triangulates() {
        let obj = b"# square\nv 0 0 0\nv 2 0 0\nv 2 2 0\nv 0 2 0\nf 1 2 3 4\n";
        let triangles = parse_obj(obj).expect("obj should parse");
        assert_eq!(triangles.len(), 2);
        let stats = inspect_mesh_bytes(obj, "obj");
        assert_eq!(stats.format.as_deref(), Some("obj (ascii)"));
        assert_eq!(stats.triangles, Some(2));
        assert_eq!(stats.dimensions, Some([2.0, 2.0, 0.0]));
    }

    #[test]
    fn obj_negative_indices_resolve() {
        let obj = b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf -3 -2 -1\n";
        let triangles = parse_obj(obj).expect("obj should parse");
        assert_eq!(triangles.len(), 1);
        let vertex = triangles[0].vertices[1];
        assert!((vertex[0] - 1.0).abs() < f32::EPSILON);
        assert!(vertex[1].abs() < f32::EPSILON);
        assert!(vertex[2].abs() < f32::EPSILON);
    }

    #[test]
    fn binary_stl_roundtrip() {
        let bytes = write_binary_stl(&unit_triangle());
        let parsed = parse_stl(&bytes).expect("binary stl should parse");
        assert_eq!(parsed.len(), 1);
        let stats = stats_for_triangles(&parsed);
        assert_eq!(stats.triangles, Some(1));
        assert_eq!(stats.dimensions, Some([1.0, 1.0, 0.0]));
    }

    #[test]
    fn transform_translate_and_scale() {
        let spec = TransformSpec {
            translate: [10.0, -2.0, 5.0],
            scale: [2.0, 2.0, 2.0],
            ..Default::default()
        };
        let out = transform_triangles(&unit_triangle(), &spec);
        let stats = stats_for_triangles(&out);
        assert_eq!(stats.bounds_min, Some([10.0, -2.0, 5.0]));
        assert_eq!(stats.bounds_max, Some([12.0, 0.0, 5.0]));
    }

    #[test]
    fn transform_rotate_z_90_degrees() {
        let spec = TransformSpec {
            rotate_deg: [0.0, 0.0, 90.0],
            ..Default::default()
        };
        let out = transform_triangles(&unit_triangle(), &spec);
        assert!((out[0].vertices[1][0] - 0.0).abs() < 1e-5);
        assert!((out[0].vertices[1][1] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn transform_mirror_x_flips_bounds() {
        let spec = TransformSpec {
            mirror: [true, false, false],
            ..Default::default()
        };
        let out = transform_triangles(&unit_triangle(), &spec);
        let stats = stats_for_triangles(&out);
        assert_eq!(stats.bounds_min, Some([-1.0, 0.0, 0.0]));
        assert_eq!(stats.bounds_max, Some([0.0, 1.0, 0.0]));
    }

    #[test]
    fn merge_concatenates_triangles() {
        let a = unit_triangle();
        let b = unit_triangle();
        let merged = merge_triangles(&[a, b]);
        assert_eq!(merged.len(), 2);
        assert_eq!(stats_for_triangles(&merged).triangles, Some(2));
    }
}
