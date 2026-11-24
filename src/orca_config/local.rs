//! Bounded discovery and resolution of a local `OrcaSlicer` configuration.
//!
//! The scanner only looks at `OrcaSlicer`'s own configuration layout (and known
//! bundled resource directories when explicitly enabled). It never crawls the
//! whole filesystem, never writes to the `OrcaSlicer` installation, and never
//! executes the slicer.
//!
//! Sources, in precedence order when the same preset name exists in more than
//! one place:
//! 1. user presets (`<config>/user/*/{machine,filament,process}/*.json`)
//! 2. installed system presets (`<config>/system/*.opc` and system JSON presets)
//! 3. bundled presets shipped with `OrcaSlicer` (`resources/profiles`), opt-in.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use walkdir::WalkDir;

use crate::{
    error::{Error, Result},
    orca_config::opc::{opc_files_in, vendor_from_path, OpcFile},
    profiles::ProfileKind,
};

const MAX_PRESET_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_JSON_FILES: usize = 50_000;

/// Where one local preset came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalSource {
    UserJson,
    SystemOpc,
    SystemJson,
    BundledOpc,
    BundledJson,
}

impl LocalSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserJson => "user_json",
            Self::SystemOpc => "system_opc",
            Self::SystemJson => "system_json",
            Self::BundledOpc => "bundled_opc",
            Self::BundledJson => "bundled_json",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::UserJson => 0,
            Self::SystemOpc => 1,
            Self::SystemJson => 2,
            Self::BundledOpc => 3,
            Self::BundledJson => 4,
        }
    }
}

/// One locally discovered preset with its own (inheritance-free) settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalPreset {
    pub name: String,
    pub kind: ProfileKind,
    pub source: LocalSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inherits: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub includes: Vec<String>,
    #[serde(default)]
    pub settings: BTreeMap<String, Value>,
}

/// A printer + filament + process selection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Selection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

impl Selection {
    #[must_use]
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.printer.as_deref().unwrap_or_default().is_empty() {
            out.push("printer");
        }
        if self.filament.as_deref().unwrap_or_default().is_empty() {
            out.push("filament");
        }
        if self.process.as_deref().unwrap_or_default().is_empty() {
            out.push("process");
        }
        out
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.missing().is_empty()
    }
}

/// Summary counts for a scan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalCounts {
    pub total: usize,
    pub printers: usize,
    pub filaments: usize,
    pub processes: usize,
    pub user_json: usize,
    pub system_opc: usize,
    pub system_json: usize,
    pub bundled_opc: usize,
    pub bundled_json: usize,
}

/// Result of scanning a local `OrcaSlicer` configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_dirs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_dirs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bundle_dirs: Vec<String>,
    pub counts: LocalCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_selection: Option<Selection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<LocalPreset>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Options controlling discovery.
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Explicit `OrcaSlicer` configuration directory. When empty, OS defaults are probed.
    pub config_dir: Option<PathBuf>,
    /// Include bundled `resources/profiles` caches/JSONs shipped with `OrcaSlicer`.
    pub include_bundled: bool,
    /// Stop after this many presets (0 means the built-in default).
    pub max_presets: usize,
}

impl LocalConfig {
    /// Scan the local `OrcaSlicer` configuration and return an in-memory catalog.
    pub fn scan(options: &ScanOptions) -> Result<Self> {
        let config_dir = match &options.config_dir {
            Some(path) => {
                let path = crate::workspace::expand_tilde(&path.to_string_lossy());
                if !path.is_dir() {
                    return Err(Error::Profile(format!(
                        "OrcaSlicer config directory {} does not exist",
                        path.display()
                    )));
                }
                Some(path)
            }
            None => detect_config_dir(),
        };

        let mut presets = Vec::new();
        let mut warnings = Vec::new();
        let mut user_dirs = Vec::new();
        let mut system_dirs = Vec::new();
        let mut bundle_dirs = Vec::new();
        let max_presets = if options.max_presets == 0 {
            20_000
        } else {
            options.max_presets
        };

        if let Some(dir) = &config_dir {
            // User presets. Only one level under `user/` is needed for the
            // standard `user/default/{machine,filament,process}` layout.
            if let Ok(entries) = fs::read_dir(dir.join("user")) {
                for entry in entries.filter_map(std::result::Result::ok) {
                    let user_root = entry.path();
                    if !user_root.is_dir() {
                        continue;
                    }
                    for (kind, sub) in [
                        (ProfileKind::Printer, "machine"),
                        (ProfileKind::Filament, "filament"),
                        (ProfileKind::Process, "process"),
                    ] {
                        let preset_dir = user_root.join(sub);
                        if !preset_dir.is_dir() {
                            continue;
                        }
                        user_dirs.push(preset_dir.to_string_lossy().to_string());
                        scan_json_dir(
                            &preset_dir,
                            kind,
                            LocalSource::UserJson,
                            &mut presets,
                            &mut warnings,
                            max_presets,
                        );
                    }
                }
            }

            // Installed system presets: `.opc` caches plus any JSON layouts.
            let system = dir.join("system");
            if system.is_dir() {
                system_dirs.push(system.to_string_lossy().to_string());
                scan_opc_dir(
                    &system,
                    LocalSource::SystemOpc,
                    &mut presets,
                    &mut warnings,
                    max_presets,
                );
                scan_system_json_tree(
                    &system,
                    LocalSource::SystemJson,
                    &mut presets,
                    &mut warnings,
                    max_presets,
                );
            }
        } else {
            warnings.push(
                "No OrcaSlicer configuration directory found; pass config_dir explicitly.".into(),
            );
        }

        if options.include_bundled {
            for dir in bundled_profile_dirs() {
                if !dir.is_dir() || bundle_dirs.iter().any(|d| d == &dir.to_string_lossy()) {
                    continue;
                }
                bundle_dirs.push(dir.to_string_lossy().to_string());
                scan_opc_dir(
                    &dir,
                    LocalSource::BundledOpc,
                    &mut presets,
                    &mut warnings,
                    max_presets,
                );
                scan_system_json_tree(
                    &dir,
                    LocalSource::BundledJson,
                    &mut presets,
                    &mut warnings,
                    max_presets,
                );
            }
        }

        presets.sort_by(|a, b| {
            a.kind
                .as_str()
                .cmp(b.kind.as_str())
                .then_with(|| {
                    a.name
                        .to_ascii_lowercase()
                        .cmp(&b.name.to_ascii_lowercase())
                })
                .then_with(|| a.source.rank().cmp(&b.source.rank()))
        });
        presets.dedup_by(|a, b| a.name == b.name && a.kind == b.kind && a.source == b.source);
        if presets.len() > max_presets {
            presets.truncate(max_presets);
            warnings.push(format!(
                "preset limit reached ({max_presets}); increase max_presets to see more"
            ));
        }

        let current_selection = config_dir
            .as_deref()
            .and_then(|dir| read_current_selection(&dir.join("OrcaSlicer.conf"), &mut warnings));

        let counts = count_presets(&presets);
        Ok(Self {
            config_dir: config_dir.map(|p| p.to_string_lossy().to_string()),
            user_dirs,
            system_dirs,
            bundle_dirs,
            counts,
            current_selection,
            presets,
            warnings,
        })
    }

    /// Find the best match for `name` and `kind` (user presets first, then
    /// installed system presets, then bundled resources).
    #[must_use]
    pub fn find(&self, kind: ProfileKind, name: &str) -> Option<&LocalPreset> {
        self.presets
            .iter()
            .filter(|p| p.kind == kind && p.name == name)
            .min_by_key(|p| p.source.rank())
    }

    /// Resolve one preset, applying its `inherits` chain and `includes`.
    pub fn resolve(&self, kind: ProfileKind, name: &str) -> ResolvedPreset {
        let mut chain = Vec::new();
        let mut include_chain = Vec::new();
        let mut warnings = Vec::new();
        let mut stack = Vec::new();
        let settings = self.resolve_inner(
            kind,
            name,
            &mut stack,
            &mut chain,
            &mut include_chain,
            &mut warnings,
        );
        let root = self.find(kind, name);
        ResolvedPreset {
            name: name.to_string(),
            kind: kind.as_str().to_string(),
            source: root.map(|p| p.source.as_str().to_string()),
            path: root.and_then(|p| p.path.clone()),
            vendor: root.and_then(|p| p.vendor.clone()),
            inherits: root.and_then(|p| p.inherits.clone()),
            includes: root.map(|p| p.includes.clone()).unwrap_or_default(),
            chain,
            include_chain,
            found: root.is_some(),
            settings,
            warnings,
        }
    }

    /// Resolve a full selection and merge the three role configs in order:
    /// printer, filament, process, then caller overrides.
    pub fn resolve_selection(
        &self,
        selection: &Selection,
        overrides: &Map<String, Value>,
    ) -> ResolvedSelection {
        let mut warnings = Vec::new();
        let printer = selection.printer.as_deref().map(|name| {
            let resolved = self.resolve(ProfileKind::Printer, name);
            warnings.extend(resolved.warnings.clone());
            resolved
        });
        let filament = selection.filament.as_deref().map(|name| {
            let resolved = self.resolve(ProfileKind::Filament, name);
            warnings.extend(resolved.warnings.clone());
            resolved
        });
        let process = selection.process.as_deref().map(|name| {
            let resolved = self.resolve(ProfileKind::Process, name);
            warnings.extend(resolved.warnings.clone());
            resolved
        });

        let mut merged = Map::new();
        for resolved in [&printer, &filament, &process].into_iter().flatten() {
            for (key, value) in &resolved.settings {
                merged.insert(key.clone(), value.clone());
            }
        }
        for (key, value) in overrides {
            merged.insert(key.clone(), value.clone());
        }

        let missing = selection.missing();
        ResolvedSelection {
            selection: selection.clone(),
            printer,
            filament,
            process,
            merged: Value::Object(merged),
            missing: missing.into_iter().map(ToString::to_string).collect(),
            warnings,
        }
    }

    fn resolve_inner(
        &self,
        kind: ProfileKind,
        name: &str,
        stack: &mut Vec<String>,
        chain: &mut Vec<ChainStep>,
        include_chain: &mut Vec<ChainStep>,
        warnings: &mut Vec<String>,
    ) -> BTreeMap<String, Value> {
        let key = format!("{}:{name}", kind.as_str());
        if stack.contains(&key) {
            warnings.push(format!(
                "inheritance cycle detected at {key}; stopping that branch"
            ));
            return BTreeMap::new();
        }
        if stack.len() >= 64 {
            warnings.push(format!("inheritance depth limit reached at {key}"));
            return BTreeMap::new();
        }
        let Some(preset) = self.find(kind, name) else {
            warnings.push(format!(
                "missing {} preset '{name}' (referenced by inherits/includes)",
                kind.as_str()
            ));
            chain.push(ChainStep {
                name: name.to_string(),
                kind: kind.as_str().to_string(),
                source: None,
                path: None,
                vendor: None,
                found: false,
            });
            return BTreeMap::new();
        };
        stack.push(key);
        chain.push(ChainStep {
            name: preset.name.clone(),
            kind: kind.as_str().to_string(),
            source: Some(preset.source.as_str().to_string()),
            path: preset.path.clone(),
            vendor: preset.vendor.clone(),
            found: true,
        });

        let mut settings = BTreeMap::new();
        if let Some(parent) = &preset.inherits {
            let parent_settings =
                self.resolve_inner(kind, parent, stack, chain, include_chain, warnings);
            settings.extend(parent_settings);
        }
        for include in &preset.includes {
            // Includes are layered under the preset's own diff, in listed order.
            let before = chain.len();
            let included = self.resolve_inner(kind, include, stack, chain, include_chain, warnings);
            include_chain.extend(chain.drain(before..));
            settings.extend(included);
        }
        for (key, value) in &preset.settings {
            settings.insert(key.clone(), value.clone());
        }
        stack.pop();
        settings
    }
}

/// One step in a resolved inheritance/include chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainStep {
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    pub found: bool,
}

/// One preset resolved with its inheritance/includes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedPreset {
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inherits: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub includes: Vec<String>,
    pub found: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<ChainStep>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_chain: Vec<ChainStep>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Three resolved roles plus their merged settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedSelection {
    pub selection: Selection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer: Option<ResolvedPreset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament: Option<ResolvedPreset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<ResolvedPreset>,
    #[serde(default)]
    pub merged: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Probe OS-specific `OrcaSlicer` configuration locations.
#[must_use]
pub fn detect_config_dir() -> Option<PathBuf> {
    if let Some(value) = env::var_os("ORCA_CONFIG_DIR") {
        let path = PathBuf::from(value);
        if path.is_dir() {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();
    if let Some(home) = dirs::home_dir() {
        #[cfg(target_os = "macos")]
        candidates.push(home.join("Library/Application Support/OrcaSlicer"));
        #[cfg(not(target_os = "macos"))]
        {
            candidates.push(home.join(".config/OrcaSlicer"));
            candidates.push(home.join(".OrcaSlicer"));
            candidates.push(home.join(".local/share/OrcaSlicer"));
        }
    }
    if let Some(config) = env::var_os("XDG_CONFIG_HOME") {
        candidates.push(PathBuf::from(config).join("OrcaSlicer"));
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = env::var_os("APPDATA") {
            candidates.push(PathBuf::from(appdata).join("OrcaSlicer"));
        }
        if let Some(local) = env::var_os("LOCALAPPDATA") {
            candidates.push(PathBuf::from(local).join("OrcaSlicer"));
        }
    }

    candidates.into_iter().find(|path| {
        path.is_dir()
            && (path.join("user").is_dir()
                || path.join("system").is_dir()
                || path.join("OrcaSlicer.conf").is_file())
    })
}

/// Known and env-provided bundled profile directories.
#[must_use]
pub fn bundled_profile_dirs() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(value) = env::var_os("ORCA_PROFILES_DIR") {
        candidates.push(PathBuf::from(value));
    }
    if let Some(value) = env::var_os("ORCA_RESOURCES_DIR") {
        candidates.push(PathBuf::from(value).join("profiles"));
    }
    for path in [
        "/usr/share/OrcaSlicer/resources/profiles",
        "/usr/local/share/OrcaSlicer/resources/profiles",
        "/opt/OrcaSlicer/resources/profiles",
        "/opt/orcaslicer/resources/profiles",
    ] {
        candidates.push(PathBuf::from(path));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/share/OrcaSlicer/resources/profiles"));
        candidates.push(home.join("Applications/OrcaSlicer.app/Contents/Resources/profiles"));
    }

    // If the configured engine binary can be found, look for `resources/profiles`
    // beside it. This covers unpacked installs and manually downloaded builds.
    if let Some(bin) = env::var_os("MCP_ORCA_ENGINE_BINARY")
        .or_else(|| env::var_os("ORCA_BINARY"))
        .and_then(|value| which::which(value).ok())
    {
        let mut current = bin.parent();
        for _ in 0..6 {
            let Some(dir) = current else { break };
            let candidate = dir.join("resources/profiles");
            if candidate.is_dir() {
                candidates.push(candidate);
                break;
            }
            current = dir.parent();
        }
    }

    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|path| path.is_dir())
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn scan_opc_dir(
    dir: &Path,
    source: LocalSource,
    presets: &mut Vec<LocalPreset>,
    warnings: &mut Vec<String>,
    max_presets: usize,
) {
    for path in opc_files_in(dir) {
        if presets.len() >= max_presets {
            return;
        }
        let expected = vendor_from_path(&path);
        match OpcFile::parse(&path) {
            Ok(file) => {
                if let Some(expected) = expected.as_deref() {
                    if !file.vendor.eq_ignore_ascii_case(expected) {
                        warnings.push(format!(
                            "{}: vendor stamp '{}' does not match file name '{expected}'",
                            path.display(),
                            file.vendor
                        ));
                    }
                }
                warnings.extend(file.warnings.iter().cloned());
                let vendor = Some(file.vendor.clone());
                for (kind, entries) in [
                    (ProfileKind::Process, &file.process),
                    (ProfileKind::Filament, &file.filament),
                    (ProfileKind::Printer, &file.machine),
                ] {
                    for entry in entries {
                        if presets.len() >= max_presets {
                            return;
                        }
                        presets.push(LocalPreset {
                            name: entry.name.clone(),
                            kind,
                            source,
                            vendor: vendor.clone(),
                            path: Some(path.to_string_lossy().to_string()),
                            inherits: entry.inherits.clone(),
                            includes: entry.includes.clone(),
                            settings: entry.settings.clone(),
                        });
                    }
                }
            }
            Err(err) => warnings.push(err.public_message()),
        }
    }
}

/// Scan JSON presets under `dir` (non-recursive) as one role.
fn scan_json_dir(
    dir: &Path,
    kind: ProfileKind,
    source: LocalSource,
    presets: &mut Vec<LocalPreset>,
    warnings: &mut Vec<String>,
    max_presets: usize,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(std::result::Result::ok) {
        if presets.len() >= max_presets {
            return;
        }
        let path = entry.path();
        if !path.is_file()
            || !path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            continue;
        }
        match read_json_preset(&path, kind, source) {
            Ok(preset) => presets.push(preset),
            Err(err) => warnings.push(err.public_message()),
        }
    }
}

/// Older/alternate `OrcaSlicer` installations keep JSON presets inside
/// `system/<vendor>/{machine,filament,process}`. Walk only those role names,
/// with a shallow depth bound.
fn scan_system_json_tree(
    root: &Path,
    source: LocalSource,
    presets: &mut Vec<LocalPreset>,
    warnings: &mut Vec<String>,
    max_presets: usize,
) {
    let mut seen_files = 0usize;
    for entry in WalkDir::new(root)
        .max_depth(5)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
        })
    {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        let kind = match name.as_str() {
            "machine" | "machines" => ProfileKind::Printer,
            "filament" | "filaments" => ProfileKind::Filament,
            "process" | "processes" | "print" => ProfileKind::Process,
            _ => continue,
        };
        // Do not descend into a role directory looking for more role directories.
        for file in fs::read_dir(entry.path())
            .into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
        {
            if presets.len() >= max_presets || seen_files >= MAX_JSON_FILES {
                return;
            }
            let path = file.path();
            if !path.is_file()
                || !path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            {
                continue;
            }
            seen_files += 1;
            match read_json_preset(&path, kind, source) {
                Ok(preset) => presets.push(preset),
                Err(err) => warnings.push(err.public_message()),
            }
        }
    }
}

fn read_json_preset(path: &Path, kind: ProfileKind, source: LocalSource) -> Result<LocalPreset> {
    let meta = fs::metadata(path)
        .map_err(|err| Error::Profile(format!("could not stat {}: {err}", path.display())))?;
    if meta.len() > MAX_PRESET_FILE_BYTES {
        return Err(Error::Profile(format!(
            "{}: preset JSON is {} bytes (limit {MAX_PRESET_FILE_BYTES})",
            path.display(),
            meta.len()
        )));
    }
    let bytes = fs::read(path)
        .map_err(|err| Error::Profile(format!("could not read {}: {err}", path.display())))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|err| Error::Profile(format!("{}: invalid preset JSON: {err}", path.display())))?;
    let Value::Object(mut object) = value else {
        return Err(Error::Profile(format!(
            "{}: preset JSON must be an object",
            path.display()
        )));
    };

    let name = object
        .get("name")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            object
                .get("filament_settings_id")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .or_else(|| {
            object
                .get("printer_settings_id")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("preset")
                .to_string()
        });
    let inherits = object
        .get("inherits")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    let includes = parse_includes(object.get("includes"));

    for key in METADATA_KEYS {
        object.remove(*key);
    }
    let settings = object.into_iter().collect::<BTreeMap<_, _>>();

    Ok(LocalPreset {
        name,
        kind,
        source,
        vendor: None,
        path: Some(path.to_string_lossy().to_string()),
        inherits,
        includes,
        settings,
    })
}

const METADATA_KEYS: &[&str] = &[
    "name",
    "inherits",
    "includes",
    "from",
    "version",
    "setting_id",
    "base_id",
    "filament_settings_id",
    "printer_settings_id",
    "print_settings_id",
    "renamed_from",
    "alias",
    "is_custom_defined",
    "sync_info",
    "user_id",
    "updated_time",
];

fn parse_includes(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(name)) if !name.trim().is_empty() => vec![name.trim().to_string()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToString::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn read_current_selection(path: &Path, warnings: &mut Vec<String>) -> Option<Selection> {
    let Ok(bytes) = fs::read(path) else {
        return None;
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(err) => {
            warnings.push(format!(
                "{}: could not parse OrcaSlicer.conf: {err}",
                path.display()
            ));
            return None;
        }
    };
    let selected_machine = value
        .get("presets")
        .and_then(|p| p.get("machine"))
        .and_then(Value::as_str)
        .map(ToString::to_string);

    let entries = value.get("orca_presets").and_then(Value::as_array);
    let entry = entries.and_then(|entries| {
        entries
            .iter()
            .find(|entry| {
                selected_machine.as_deref().is_some_and(|machine| {
                    entry.get("machine").and_then(Value::as_str) == Some(machine)
                })
            })
            .or_else(|| entries.first())
    });

    let printer = entry
        .and_then(|e| e.get("machine"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or(selected_machine);
    let filament = entry
        .and_then(|e| e.get("filament"))
        .and_then(first_string)
        .or_else(|| {
            value
                .get("filaments")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .map(ToString::to_string)
        });
    let process = entry.and_then(|e| e.get("process")).and_then(first_string);

    if printer.is_none() && filament.is_none() && process.is_none() {
        None
    } else {
        Some(Selection {
            printer,
            filament,
            process,
        })
    }
}

fn first_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(ToString::to_string),
        _ => None,
    }
}

fn count_presets(presets: &[LocalPreset]) -> LocalCounts {
    let mut counts = LocalCounts {
        total: presets.len(),
        ..LocalCounts::default()
    };
    for preset in presets {
        match preset.kind {
            ProfileKind::Printer => counts.printers += 1,
            ProfileKind::Filament => counts.filaments += 1,
            ProfileKind::Process => counts.processes += 1,
            _ => {}
        }
        match preset.source {
            LocalSource::UserJson => counts.user_json += 1,
            LocalSource::SystemOpc => counts.system_opc += 1,
            LocalSource::SystemJson => counts.system_json += 1,
            LocalSource::BundledOpc => counts.bundled_opc += 1,
            LocalSource::BundledJson => counts.bundled_json += 1,
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_user_preset_and_current_selection() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let machine_dir = root.join("user/default/machine");
        fs::create_dir_all(&machine_dir).unwrap();
        fs::write(
            machine_dir.join("local.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "Local Printer",
                "inherits": "Base Printer",
                "from": "User",
                "version": "2.4.0.9",
                "nozzle_diameter": ["0.4"],
                "machine_start_gcode": "G28\n"
            }))
            .unwrap(),
        )
        .unwrap();
        fs::create_dir_all(root.join("system")).unwrap();
        fs::write(
            root.join("OrcaSlicer.conf"),
            serde_json::to_vec(&serde_json::json!({
                "presets": {"machine": "Local Printer"},
                "orca_presets": [{
                    "machine": "Local Printer",
                    "filament": "Local Filament",
                    "process": "Local Process"
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let scan = LocalConfig::scan(&ScanOptions {
            config_dir: Some(root.to_path_buf()),
            include_bundled: false,
            max_presets: 100,
        })
        .unwrap();
        assert_eq!(scan.counts.printers, 1);
        assert_eq!(
            scan.current_selection.as_ref().unwrap().process.as_deref(),
            Some("Local Process")
        );
        let preset = scan.find(ProfileKind::Printer, "Local Printer").unwrap();
        assert_eq!(preset.inherits.as_deref(), Some("Base Printer"));
        assert!(preset.settings.contains_key("nozzle_diameter"));
        assert!(!preset.settings.contains_key("from"));
    }

    #[test]
    fn resolves_inheritance_across_sources() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let machine_dir = root.join("user/default/machine");
        fs::create_dir_all(&machine_dir).unwrap();
        fs::write(
            machine_dir.join("child.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "Child",
                "inherits": "Base",
                "nozzle_diameter": ["0.6"]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::create_dir_all(root.join("system")).unwrap();

        let mut scan = LocalConfig::scan(&ScanOptions {
            config_dir: Some(root.to_path_buf()),
            include_bundled: false,
            max_presets: 100,
        })
        .unwrap();
        // Simulate a system preset already indexed.
        scan.presets.push(LocalPreset {
            name: "Base".into(),
            kind: ProfileKind::Printer,
            source: LocalSource::SystemJson,
            vendor: Some("Custom".into()),
            path: None,
            inherits: None,
            includes: vec![],
            settings: BTreeMap::from([("nozzle_diameter".to_string(), serde_json::json!(["0.4"]))]),
        });

        let resolved = scan.resolve(ProfileKind::Printer, "Child");
        assert!(resolved.found);
        assert_eq!(resolved.chain.len(), 2);
        assert_eq!(
            resolved.settings.get("nozzle_diameter"),
            Some(&serde_json::json!(["0.6"]))
        );
    }
}
