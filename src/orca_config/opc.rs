//! Parser for `OrcaSlicer` per-vendor preset cache files (`<vendor>.opc`).
//!
//! `OrcaSlicer` ships system presets as binary `<vendor>.opc` files: a 20-byte
//! header (`ZCRO`, format version, body size, CRC32) framing a cereal binary
//! body. The body carries the vendor profile map, one option dictionary shared
//! by all presets in the file, and the process/filament/machine preset diffs.
//!
//! This is a deliberately read-only, bounded parser: it validates the header
//! and CRC, caps allocation from file-controlled lengths, and turns each preset
//! diff into JSON values. It never writes `.opc` files.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use crate::error::{Error, Result};

/// `ZCRO` little-endian magic (`0x4F52435A`).
const MAGIC: &[u8; 4] = b"ZCRO";
/// Cache format version this parser understands.
const CACHE_VERSION: u32 = 2;
/// Refuse absurd files before reading them into memory.
const MAX_OPC_BYTES: u64 = 256 * 1024 * 1024;
/// Refuse absurd strings/vectors from a corrupt body.
const MAX_STRING_BYTES: u64 = 16 * 1024 * 1024;
const MAX_VECTOR_ENTRIES: u64 = 5_000_000;

// Mirrors Slic3r::ConfigOptionType from OrcaSlicer's Config.hpp.
const CO_VECTOR_TYPE: u16 = 0x4000;
const CO_FLOAT: u16 = 1;
const CO_FLOATS: u16 = CO_FLOAT + CO_VECTOR_TYPE;
const CO_INT: u16 = 2;
const CO_INTS: u16 = CO_INT + CO_VECTOR_TYPE;
const CO_STRING: u16 = 3;
const CO_STRINGS: u16 = CO_STRING + CO_VECTOR_TYPE;
const CO_PERCENT: u16 = 4;
const CO_PERCENTS: u16 = CO_PERCENT + CO_VECTOR_TYPE;
const CO_FLOAT_OR_PERCENT: u16 = 5;
const CO_FLOATS_OR_PERCENTS: u16 = CO_FLOAT_OR_PERCENT + CO_VECTOR_TYPE;
const CO_POINT: u16 = 6;
const CO_POINTS: u16 = CO_POINT + CO_VECTOR_TYPE;
const CO_POINT3: u16 = 7;
const CO_BOOL: u16 = 8;
const CO_BOOLS: u16 = CO_BOOL + CO_VECTOR_TYPE;
const CO_ENUM: u16 = 9;
const CO_ENUMS: u16 = CO_ENUM + CO_VECTOR_TYPE;
const CO_POINTS_GROUPS: u16 = 10 + CO_VECTOR_TYPE;
const CO_INTS_GROUPS: u16 = 11 + CO_VECTOR_TYPE;

/// One preset stored in an `.opc` cache, with its own (uninherited) diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpcPreset {
    pub name: String,
    pub sub_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inherits: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub includes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instantiation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setting_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renamed_from: Vec<String>,
    /// The preset's own settings, keyed by Orca option name.
    pub settings: BTreeMap<String, Value>,
}

/// Parsed contents of one `<vendor>.opc` file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpcFile {
    pub path: String,
    pub vendor: String,
    pub vendor_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_name: Option<String>,
    pub dictionary_keys: usize,
    pub enum_values: usize,
    pub process_count: usize,
    pub filament_count: usize,
    pub machine_count: usize,
    pub parse_errors: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub process: Vec<OpcPreset>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filament: Vec<OpcPreset>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub machine: Vec<OpcPreset>,
}

impl OpcFile {
    /// Parse one `.opc` file from disk.
    pub fn parse(path: &Path) -> Result<Self> {
        let bytes = read_bounded(path)?;
        Self::parse_bytes(path, &bytes)
    }

    /// Parse an `.opc` body already in memory. `path` is only used in errors.
    pub fn parse_bytes(path: &Path, bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 20 {
            return Err(Error::Profile(format!(
                "{}: file is shorter than the 20-byte .opc header",
                path.display()
            )));
        }
        let magic = &bytes[..4];
        if magic != MAGIC {
            return Err(Error::Profile(format!(
                "{}: not an OrcaSlicer .opc cache (magic {magic:02x?})",
                path.display()
            )));
        }
        let _version = u32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes"));
        let body_size = u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));
        let expected_crc = u32::from_le_bytes(bytes[16..20].try_into().expect("4 bytes"));
        let body = &bytes[20..];
        if body_size != body.len() as u64 {
            return Err(Error::Profile(format!(
                "{}: .opc body size mismatch (header {body_size}, file {})",
                path.display(),
                body.len()
            )));
        }
        let actual_crc = crc32(body);
        if actual_crc != expected_crc {
            return Err(Error::Profile(format!(
                "{}: .opc CRC mismatch (header {expected_crc:#010x}, body {actual_crc:#010x})",
                path.display()
            )));
        }

        let mut reader = Reader::new(body);
        let cache_version = reader.u32()?;
        if cache_version != CACHE_VERSION {
            return Err(Error::Profile(format!(
                "{}: unsupported .opc cache version {cache_version} (expected {CACHE_VERSION})",
                path.display()
            )));
        }
        let vendor = reader.string()?;
        let vendor_version = reader.string()?;
        let key_count = reader.u64()?;
        let keys = reader.string_vec_bounded(key_count)?;
        let type_count = reader.u64()?;
        let types = reader.u16_vec_bounded(type_count)?;
        if keys.len() != types.len() {
            return Err(Error::Profile(format!(
                "{}: .opc dictionary has {} keys but {} types",
                path.display(),
                keys.len(),
                types.len()
            )));
        }
        let enum_count = reader.u64()?;
        let enum_values = reader.string_vec_bounded(enum_count)?;

        let mut warnings = Vec::new();
        let mut vendor_name = None;
        let vendor_count = reader.u64()?;
        Reader::check_count(vendor_count, "vendor")?;
        for _ in 0..vendor_count {
            let _vendor_key = reader.string()?;
            let profile = reader.vendor_profile()?;
            if vendor_name.is_none() {
                vendor_name = Some(profile);
            }
        }

        let process = read_entries(&mut reader, &keys, &types, &enum_values)?;
        let filament = read_entries(&mut reader, &keys, &types, &enum_values)?;
        let machine = read_entries(&mut reader, &keys, &types, &enum_values)?;
        let parse_errors = reader.u64()?;
        if reader.remaining() != 0 {
            warnings.push(format!(
                "{}: {} trailing bytes after the .opc body",
                path.display(),
                reader.remaining()
            ));
        }

        Ok(Self {
            path: path.to_string_lossy().to_string(),
            vendor,
            vendor_version,
            vendor_name,
            dictionary_keys: keys.len(),
            enum_values: enum_values.len(),
            process_count: process.len(),
            filament_count: filament.len(),
            machine_count: machine.len(),
            parse_errors,
            warnings,
            process,
            filament,
            machine,
        })
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let meta = fs::metadata(path)
        .map_err(|err| Error::Profile(format!("could not stat {}: {err}", path.display())))?;
    if meta.len() > MAX_OPC_BYTES {
        return Err(Error::Profile(format!(
            "{}: .opc file is {} bytes (limit {MAX_OPC_BYTES})",
            path.display(),
            meta.len()
        )));
    }
    fs::read(path)
        .map_err(|err| Error::Profile(format!("could not read {}: {err}", path.display())))
}

fn read_entries(
    reader: &mut Reader<'_>,
    keys: &[String],
    types: &[u16],
    enum_values: &[String],
) -> Result<Vec<OpcPreset>> {
    let count = reader.u32()?;
    Reader::check_count(u64::from(count), "preset")?;
    let mut out = Vec::with_capacity((count as usize).min(4096));
    for _ in 0..count {
        out.push(reader.preset(keys, types, enum_values)?);
    }
    Ok(out)
}

/// A cursor over a cereal binary archive body.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| Error::Profile("corrupt .opc: length overflow".into()))?;
        if end > self.data.len() {
            return Err(Error::Profile(format!(
                "corrupt .opc: need {n} bytes at offset {}, only {} left",
                self.pos,
                self.remaining()
            )));
        }
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn check_count(count: u64, what: &str) -> Result<()> {
        if count > MAX_VECTOR_ENTRIES {
            return Err(Error::Profile(format!(
                "corrupt .opc: {what} count {count} exceeds limit {MAX_VECTOR_ENTRIES}"
            )));
        }
        Ok(())
    }

    /// Cereal binary archive string: `u64` byte length followed by UTF-8 bytes.
    fn string(&mut self) -> Result<String> {
        let len = self.u64()?;
        if len > MAX_STRING_BYTES {
            return Err(Error::Profile(format!(
                "corrupt .opc: string length {len} exceeds limit {MAX_STRING_BYTES}"
            )));
        }
        let bytes = self.take(len as usize)?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    fn string_vec_bounded(&mut self, count: u64) -> Result<Vec<String>> {
        Self::check_count(count, "string")?;
        let mut out = Vec::with_capacity((count as usize).min(4096));
        for _ in 0..count {
            out.push(self.string()?);
        }
        Ok(out)
    }

    fn u16_vec_bounded(&mut self, count: u64) -> Result<Vec<u16>> {
        Self::check_count(count, "type")?;
        let mut out = Vec::with_capacity((count as usize).min(4096));
        for _ in 0..count {
            out.push(self.u16()?);
        }
        Ok(out)
    }

    /// Consume a `std::map<std::string, VendorProfile>` enough to find the
    /// vendor's display name and step over every hand-written field.
    fn vendor_profile(&mut self) -> Result<String> {
        let name = self.string()?;
        let _id = self.string()?;
        // Semver uses cereal minimal serialization, i.e. a plain string.
        let _config_version = self.string()?;
        let _config_update_url = self.string()?;
        let _changelog_url = self.string()?;

        let models = self.u64()?;
        Self::check_count(models, "printer model")?;
        for _ in 0..models {
            let _id = self.string()?;
            let _name = self.string()?;
            let _model_id = self.string()?;
            let _technology = self.u8()?; // PrinterTechnology: unsigned char
            let _family = self.string()?;
            let _variants = self.string_vec_len()?;
            let _default_materials = self.string_vec_len()?;
            let _not_support_bed_types = self.string_vec_len()?;
            for _ in 0..9 {
                let _ = self.string()?;
            }
        }
        let _default_filaments = self.string_vec_len()?;
        let _default_sla_materials = self.string_vec_len()?;
        Ok(name)
    }

    fn string_vec_len(&mut self) -> Result<Vec<String>> {
        let count = self.u64()?;
        self.string_vec_bounded(count)
    }

    fn preset(
        &mut self,
        keys: &[String],
        types: &[u16],
        enum_values: &[String],
    ) -> Result<OpcPreset> {
        let name = self.string()?;
        let sub_path = self.string()?;
        let settings = self.config(keys, types, enum_values)?;
        let inherits = non_empty(self.string()?);
        let includes = self.string_vec_len()?;
        let description = non_empty(self.string()?);
        let instantiation = non_empty(self.string()?);
        let setting_id = non_empty(self.string()?);
        let filament_id = non_empty(self.string()?);
        let renamed_from = self.string_vec_len()?;
        Ok(OpcPreset {
            name,
            sub_path,
            inherits,
            includes,
            description,
            instantiation,
            setting_id,
            filament_id,
            renamed_from,
            settings,
        })
    }

    fn config(
        &mut self,
        keys: &[String],
        types: &[u16],
        enum_values: &[String],
    ) -> Result<BTreeMap<String, Value>> {
        let count = self.u32()?;
        Self::check_count(u64::from(count), "config entry")?;
        let mut out = BTreeMap::new();
        for _ in 0..count {
            let idx = usize::from(self.u16()?);
            let key = keys.get(idx).ok_or_else(|| {
                Error::Profile(format!(
                    "corrupt .opc: config key index {idx} past dictionary ({} keys)",
                    keys.len()
                ))
            })?;
            let typ = *types.get(idx).ok_or_else(|| {
                Error::Profile(format!(
                    "corrupt .opc: config type index {idx} past dictionary ({} types)",
                    types.len()
                ))
            })?;
            let value = self.value(typ, enum_values)?;
            out.insert(key.clone(), value);
        }
        Ok(out)
    }

    fn value(&mut self, typ: u16, enum_values: &[String]) -> Result<Value> {
        match typ {
            CO_FLOAT => Ok(float_value(self.f64()?)),
            CO_FLOATS | CO_PERCENTS => number_vec(self),
            CO_INT => Ok(Value::Number(Number::from(self.i32()?))),
            CO_INTS => int_vec(self),
            CO_STRING => Ok(Value::String(self.string()?)),
            CO_STRINGS => Ok(Value::Array(
                self.string_vec_len()?
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            )),
            CO_PERCENT => Ok(float_value(self.f64()?)),
            CO_FLOAT_OR_PERCENT => {
                let value = float_value(self.f64()?);
                let percent = self.u8()? != 0;
                Ok(float_or_percent(value, percent))
            }
            CO_FLOATS_OR_PERCENTS => {
                let count = self.u64()?;
                Self::check_count(count, "float-or-percent")?;
                let mut out = Vec::with_capacity((count as usize).min(4096));
                for _ in 0..count {
                    let value = float_value(self.f64()?);
                    let percent = self.u8()? != 0;
                    out.push(float_or_percent(value, percent));
                }
                Ok(Value::Array(out))
            }
            CO_POINT => Ok(Value::Array(vec![
                float_value(self.f64()?),
                float_value(self.f64()?),
            ])),
            CO_POINTS => {
                let count = self.u64()?;
                Self::check_count(count, "point")?;
                let mut out = Vec::with_capacity((count as usize).min(4096));
                for _ in 0..count {
                    out.push(Value::Array(vec![
                        float_value(self.f64()?),
                        float_value(self.f64()?),
                    ]));
                }
                Ok(Value::Array(out))
            }
            CO_POINT3 => Ok(Value::Array(vec![
                float_value(self.f64()?),
                float_value(self.f64()?),
                float_value(self.f64()?),
            ])),
            CO_BOOL => Ok(Value::Bool(self.u8()? != 0)),
            CO_BOOLS => {
                let count = self.u64()?;
                Self::check_count(count, "bool")?;
                let mut out = Vec::with_capacity((count as usize).min(4096));
                for _ in 0..count {
                    out.push(Value::Bool(self.u8()? != 0));
                }
                Ok(Value::Array(out))
            }
            CO_ENUM => {
                let values = self.enum_values(enum_values)?;
                if values.len() == 1 {
                    Ok(values.into_iter().next().expect("one value"))
                } else {
                    Ok(Value::Array(values))
                }
            }
            CO_ENUMS => Ok(Value::Array(self.enum_values(enum_values)?)),
            CO_POINTS_GROUPS => {
                let groups = self.u64()?;
                Self::check_count(groups, "point group")?;
                let mut out = Vec::with_capacity((groups as usize).min(4096));
                for _ in 0..groups {
                    let count = self.u64()?;
                    Self::check_count(count, "point in group")?;
                    let mut group = Vec::with_capacity((count as usize).min(4096));
                    for _ in 0..count {
                        group.push(Value::Array(vec![
                            float_value(self.f64()?),
                            float_value(self.f64()?),
                        ]));
                    }
                    out.push(Value::Array(group));
                }
                Ok(Value::Array(out))
            }
            CO_INTS_GROUPS => {
                let groups = self.u64()?;
                Self::check_count(groups, "int group")?;
                let mut out = Vec::with_capacity((groups as usize).min(4096));
                for _ in 0..groups {
                    let count = self.u64()?;
                    Self::check_count(count, "int in group")?;
                    let mut group = Vec::with_capacity((count as usize).min(4096));
                    for _ in 0..count {
                        group.push(Value::Number(Number::from(self.i32()?)));
                    }
                    out.push(Value::Array(group));
                }
                Ok(Value::Array(out))
            }
            other => Err(Error::Profile(format!(
                "corrupt .opc: unsupported config option type {other}"
            ))),
        }
    }

    fn enum_values(&mut self, enum_values: &[String]) -> Result<Vec<Value>> {
        let count = self.u32()?;
        Self::check_count(u64::from(count), "enum")?;
        let mut out = Vec::with_capacity((count as usize).min(4096));
        for _ in 0..count {
            let idx = usize::from(self.u16()?);
            // Index 0 is the unnamed slot; the writer then stores the raw int.
            if idx == 0 {
                out.push(Value::Number(Number::from(self.i32()?)));
                continue;
            }
            let name = enum_values.get(idx).ok_or_else(|| {
                Error::Profile(format!(
                    "corrupt .opc: enum index {idx} past enum table ({} values)",
                    enum_values.len()
                ))
            })?;
            out.push(Value::String(name.clone()));
        }
        Ok(out)
    }
}

fn number_vec(reader: &mut Reader<'_>) -> Result<Value> {
    let count = reader.u64()?;
    Reader::check_count(count, "number")?;
    let mut out = Vec::with_capacity((count as usize).min(4096));
    for _ in 0..count {
        out.push(float_value(reader.f64()?));
    }
    Ok(Value::Array(out))
}

fn int_vec(reader: &mut Reader<'_>) -> Result<Value> {
    let count = reader.u64()?;
    Reader::check_count(count, "int")?;
    let mut out = Vec::with_capacity((count as usize).min(4096));
    for _ in 0..count {
        out.push(Value::Number(Number::from(reader.i32()?)));
    }
    Ok(Value::Array(out))
}

fn float_value(value: f64) -> Value {
    if value.is_nan() {
        Value::String("nil".into())
    } else if value.is_infinite() {
        Value::String(if value.is_sign_negative() {
            "-inf".into()
        } else {
            "inf".into()
        })
    } else {
        Number::from_f64(value).map_or_else(|| Value::String(value.to_string()), Value::Number)
    }
}

fn float_or_percent(value: Value, percent: bool) -> Value {
    let mut map = Map::new();
    map.insert("value".into(), value);
    map.insert("percent".into(), Value::Bool(percent));
    Value::Object(map)
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Standard IEEE CRC-32 (the polynomial `OrcaSlicer`'s `boost::crc_32_type` uses).
fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = crc_table();
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        let idx = ((crc ^ u32::from(*byte)) & 0xFF) as usize;
        crc = (crc >> 8) ^ TABLE[idx];
    }
    !crc
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Return the vendor name encoded in a `<vendor>.opc` file name.
#[must_use]
pub fn vendor_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

/// List `*.opc` files directly under a directory (non-recursive, sorted).
#[must_use]
pub fn opc_files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("opc"))
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny but structurally valid `.opc` cache body.
    fn synthetic_opc() -> Vec<u8> {
        fn string(out: &mut Vec<u8>, s: &str) {
            out.extend_from_slice(&(s.len() as u64).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        fn vec_string(out: &mut Vec<u8>, values: &[&str]) {
            out.extend_from_slice(&(values.len() as u64).to_le_bytes());
            for v in values {
                string(out, v);
            }
        }
        let mut body = Vec::new();
        body.extend_from_slice(&2u32.to_le_bytes()); // cache version
        string(&mut body, "Custom");
        string(&mut body, "2.4.0.9");

        // Dictionary: two keys, one enum value.
        let keys = ["layer_height", "sparse_infill_pattern"];
        let types = [CO_FLOAT, CO_ENUM];
        body.extend_from_slice(&(keys.len() as u64).to_le_bytes());
        for k in keys {
            string(&mut body, k);
        }
        body.extend_from_slice(&(types.len() as u64).to_le_bytes());
        for t in types {
            body.extend_from_slice(&t.to_le_bytes());
        }
        vec_string(&mut body, &["", "crosshatch"]);

        // Vendor map: one vendor with no models/sets.
        body.extend_from_slice(&1u64.to_le_bytes());
        string(&mut body, "Custom");
        string(&mut body, "Custom Printer");
        string(&mut body, "Custom");
        string(&mut body, "2.4.9");
        string(&mut body, "");
        string(&mut body, "");
        body.extend_from_slice(&0u64.to_le_bytes()); // models
        body.extend_from_slice(&0u64.to_le_bytes()); // default filaments
        body.extend_from_slice(&0u64.to_le_bytes()); // default sla materials

        // Process entries: one entry inheriting nothing.
        body.extend_from_slice(&1u32.to_le_bytes());
        string(&mut body, "0.20mm Standard");
        string(&mut body, "process/0.20mm Standard.json");
        body.extend_from_slice(&2u32.to_le_bytes()); // two config entries
        body.extend_from_slice(&0u16.to_le_bytes()); // layer_height
        body.extend_from_slice(&0.2f64.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes()); // sparse_infill_pattern
        body.extend_from_slice(&1u32.to_le_bytes()); // enum count
        body.extend_from_slice(&1u16.to_le_bytes()); // enum index
        string(&mut body, ""); // inherits
        body.extend_from_slice(&0u64.to_le_bytes()); // includes
        string(&mut body, "");
        string(&mut body, "true");
        string(&mut body, "");
        string(&mut body, "");
        body.extend_from_slice(&0u64.to_le_bytes()); // renamed_from

        // No filament/machine entries.
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u64.to_le_bytes()); // parse errors

        let mut file = Vec::new();
        file.extend_from_slice(MAGIC);
        file.extend_from_slice(&CACHE_VERSION.to_le_bytes());
        file.extend_from_slice(&(body.len() as u64).to_le_bytes());
        file.extend_from_slice(&crc32(&body).to_le_bytes());
        file.extend_from_slice(&body);
        file
    }

    #[test]
    fn parses_synthetic_opc() {
        let bytes = synthetic_opc();
        let parsed = OpcFile::parse_bytes(Path::new("Custom.opc"), &bytes).unwrap();
        assert_eq!(parsed.vendor, "Custom");
        assert_eq!(parsed.vendor_name.as_deref(), Some("Custom Printer"));
        assert_eq!(parsed.process_count, 1);
        let preset = &parsed.process[0];
        assert_eq!(preset.name, "0.20mm Standard");
        assert_eq!(preset.settings["layer_height"], serde_json::json!(0.2));
        assert_eq!(
            preset.settings["sparse_infill_pattern"],
            serde_json::json!("crosshatch")
        );
    }

    #[test]
    fn rejects_bad_crc() {
        let mut bytes = synthetic_opc();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        assert!(OpcFile::parse_bytes(Path::new("Custom.opc"), &bytes).is_err());
    }
}
