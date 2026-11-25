//! `OrcaSlicer` profile/preset discovery and indexing.
//!
//! Bounded discovery only: explicit `search_paths` + optional OS-detected common defaults
//! (scanned only if they exist). No whole-system crawl.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::UNIX_EPOCH,
};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::{config::Config, error::Result, workspace::expand_tilde};

const MAX_SCAN_DEPTH: usize = 10;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    Printer,
    Filament,
    Process,
    Project,
    Bundle,
    Unknown,
}

impl ProfileKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Printer => "printer",
            Self::Filament => "filament",
            Self::Process => "process",
            Self::Project => "project",
            Self::Bundle => "bundle",
            Self::Unknown => "unknown",
        }
    }
}

impl std::str::FromStr for ProfileKind {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "printer" | "machine" => Ok(Self::Printer),
            "filament" | "material" => Ok(Self::Filament),
            "process" | "print" | "preset" => Ok(Self::Process),
            "project" | "3mf" => Ok(Self::Project),
            "bundle" => Ok(Self::Bundle),
            "unknown" => Ok(Self::Unknown),
            other => Err(format!("unknown profile kind '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileEntry {
    pub id: String,
    pub name: String,
    pub kind: ProfileKind,
    pub path: String,
    pub format: String,
    pub bytes: u64,
    pub modified_at: Option<u64>,
    pub source: String, // "explicit", "os_default", "workspace"
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileCatalog {
    pub scanned_dirs: Vec<String>,
    #[serde(default)]
    pub scanned_user_dirs: Vec<String>,
    pub entries: Vec<ProfileEntry>,
    pub warnings: Vec<String>,
}

impl ProfileCatalog {
    #[must_use]
    pub fn get(&self, id_or_name: &str) -> Option<&ProfileEntry> {
        self.entries
            .iter()
            .find(|e| e.id == id_or_name)
            .or_else(|| self.entries.iter().find(|e| e.path == id_or_name))
            .or_else(|| {
                self.entries
                    .iter()
                    .find(|e| e.name.eq_ignore_ascii_case(id_or_name))
            })
    }
}

/// Describes how profile discovery was performed (for tool output transparency).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryCoverage {
    pub use_os_defaults: bool,
    pub search_paths: Vec<String>,
    pub scanned_dirs: Vec<String>,
    pub scanned_user_dirs: Vec<String>,
    pub excluded_patterns: Vec<String>,
    pub max_scan_depth: usize,
    pub notes: Vec<String>,
}

/// Profile index with lazy scan and cache.
#[derive(Debug)]
pub struct ProfileIndex {
    config: Arc<Config>,
    catalog: RwLock<Option<Arc<ProfileCatalog>>>,
}

impl ProfileIndex {
    pub fn open(config: Arc<Config>) -> Result<Self> {
        Ok(Self {
            config,
            catalog: RwLock::new(None),
        })
    }

    #[must_use]
    pub fn discovery_coverage(&self, catalog: &ProfileCatalog) -> DiscoveryCoverage {
        DiscoveryCoverage {
            use_os_defaults: self.config.profiles.use_os_defaults,
            search_paths: self.config.profiles.search_paths.clone(),
            scanned_dirs: catalog.scanned_dirs.clone(),
            scanned_user_dirs: catalog.scanned_user_dirs.clone(),
            excluded_patterns: vec![
                "Hidden dotfiles/directories (except scan roots)".into(),
                "JSON/INI/CFG files outside preset-relevant names or preset directories (machine/filament/process/print/preset)".into(),
                "Non-preset file types (except .3mf, .orca_printer, .orca_filament, selected .zip bundles)".into(),
                "Symlinks are not followed; scan depth limited to 10".into(),
            ],
            max_scan_depth: MAX_SCAN_DEPTH,
            notes: vec![
                "Discovery is bounded: explicit search_paths + OS-detected common defaults (when enabled) + workspace/profiles.".into(),
                "If a preset exists on disk but is not indexed, pass its explicit path to orca_profile_resolve or add its directory to profiles.search_paths.".into(),
            ],
        }
    }

    pub fn catalog(&self) -> Result<Arc<ProfileCatalog>> {
        if let Some(catalog) = self
            .catalog
            .read()
            .map_err(|_| crate::error::Error::Profile("profile index lock poisoned".into()))?
            .as_ref()
        {
            return Ok(Arc::clone(catalog));
        }
        self.refresh()
    }

    pub fn refresh(&self) -> Result<Arc<ProfileCatalog>> {
        let mut catalog = ProfileCatalog::default();
        let mut seen: HashSet<PathBuf> = HashSet::new();

        for (dir, source) in self.discovery_dirs() {
            if !dir.is_dir() {
                continue;
            }
            let dir_str = dir.to_string_lossy().to_string();
            catalog.scanned_dirs.push(dir_str.clone());
            if is_user_preset_dir(&dir) {
                catalog.scanned_user_dirs.push(dir_str);
            }
            for entry in WalkDir::new(&dir)
                .max_depth(MAX_SCAN_DEPTH)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| {
                    e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.')
                })
            {
                let Ok(entry) = entry else { continue };
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path().to_path_buf();
                // Track user preset subdirs encountered during scan for coverage reporting.
                if let Some(parent) = path.parent() {
                    if is_user_preset_dir(parent) {
                        let parent_str = parent.to_string_lossy().to_string();
                        if !catalog.scanned_user_dirs.iter().any(|d| d == &parent_str) {
                            catalog.scanned_user_dirs.push(parent_str);
                        }
                    }
                }
                if !seen.insert(path.clone()) {
                    continue;
                }
                if let Some(profile_entry) = classify_entry(&path, source) {
                    catalog.entries.push(profile_entry);
                }
            }
        }
        catalog.scanned_user_dirs.sort();
        catalog.scanned_user_dirs.dedup();

        catalog.entries.sort_by(|a, b| {
            a.kind.as_str().cmp(b.kind.as_str()).then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            })
        });

        let catalog = Arc::new(catalog);
        *self
            .catalog
            .write()
            .map_err(|_| crate::error::Error::Profile("profile index lock poisoned".into()))? =
            Some(Arc::clone(&catalog));
        Ok(catalog)
    }

    #[must_use]
    pub fn discovery_dirs(&self) -> Vec<(PathBuf, &'static str)> {
        let mut dirs = Vec::new();
        for p in &self.config.profiles.search_paths {
            dirs.push((expand_tilde(p), "explicit"));
        }
        if self.config.profiles.use_os_defaults {
            for p in os_default_profile_dirs() {
                dirs.push((p, "os_default"));
            }
        }
        // workspace custom profiles
        let ws = expand_tilde(&self.config.workspace.root).join("profiles");
        dirs.push((ws, "workspace"));
        // de-dup while preserving order
        let mut seen = HashSet::new();
        dirs.retain(|(p, _)| seen.insert(p.clone()));
        dirs
    }

    #[must_use]
    pub fn find_entry<'a>(
        catalog: &'a ProfileCatalog,
        id_or_name: &str,
    ) -> Option<&'a ProfileEntry> {
        catalog.get(id_or_name)
    }
}

/// Best-effort OS-detected common profile/config locations (only if present).
#[must_use]
pub fn os_default_profile_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = dirs::home_dir();

    #[cfg(target_os = "linux")]
    {
        if let Some(h) = &home {
            dirs.push(h.join(".config/OrcaSlicer"));
            dirs.push(h.join(".OrcaSlicer"));
            dirs.push(h.join(".local/share/OrcaSlicer"));
        }
        dirs.push(PathBuf::from("/usr/share/OrcaSlicer"));
        dirs.push(PathBuf::from("/usr/local/share/OrcaSlicer"));
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(h) = &home {
            dirs.push(h.join("Library/Application Support/OrcaSlicer"));
        }
        dirs.push(PathBuf::from(
            "/Applications/OrcaSlicer.app/Contents/Resources",
        ));
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            dirs.push(PathBuf::from(appdata).join("OrcaSlicer"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("OrcaSlicer"));
        }
    }

    // Fallback generic candidates for other unix-likes
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        if let Some(h) = &home {
            dirs.push(h.join(".config/OrcaSlicer"));
        }
    }

    dirs
}

fn classify_entry(path: &Path, source: &str) -> Option<ProfileEntry> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let name_os = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let lower = name_os.to_ascii_lowercase();
    let dir_hint = kind_hint_from_path(path);

    let (kind, is_candidate) = match ext.as_str() {
        "3mf" => (ProfileKind::Project, true),
        "orca_printer" | "orca_filament" => (classify_by_name(&lower, &ext).or_hint(dir_hint), true),
        "json" => {
            let kind = classify_by_name(&lower, &ext).or_hint(dir_hint);
            let candidate = likely_preset_json(path, &lower) || dir_hint.is_some();
            (kind, candidate)
        }
        "ini" | "cfg" => {
            let kind = classify_by_name(&lower, &ext).or_hint(dir_hint);
            (kind, likely_preset_ini(&lower) || dir_hint.is_some())
        }
        "zip" => (
            ProfileKind::Bundle,
            lower.contains("profile") || lower.contains("preset") || lower.contains("orca"),
        ),
        _ => (ProfileKind::Unknown, false),
    };
    if !is_candidate {
        return None;
    }

    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let modified_at = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let id = format!("prf_{:016x}", stable_hash(&path.to_string_lossy()));
    let display_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name_os)
        .to_string();

    Some(ProfileEntry {
        id,
        name: display_name,
        kind,
        path: path.to_string_lossy().to_string(),
        format: if ext.is_empty() { "file".into() } else { ext },
        bytes: meta.len(),
        modified_at,
        source: source.to_string(),
    })
}

trait KindHint {
    fn or_hint(self, hint: Option<ProfileKind>) -> Self;
}

impl KindHint for ProfileKind {
    fn or_hint(self, hint: Option<ProfileKind>) -> Self {
        if matches!(self, ProfileKind::Unknown) {
            hint.unwrap_or(self)
        } else {
            self
        }
    }
}

/// Infer profile kind from ancestor directory names (Orca user/system layout).
fn kind_hint_from_path(path: &Path) -> Option<ProfileKind> {
    for ancestor in path.ancestors().skip(1).take(6) {
        let Some(name) = ancestor.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "machine" | "machines" | "printer" | "printers" => return Some(ProfileKind::Printer),
            "filament" | "filaments" | "material" | "materials" => {
                return Some(ProfileKind::Filament)
            }
            "process" | "processes" | "print" | "prints" | "preset" | "presets" => {
                return Some(ProfileKind::Process)
            }
            _ => {}
        }
    }
    None
}

fn is_user_preset_dir(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    lower.contains("/user/")
        || lower.contains("\\user\\")
        || lower.contains("/user_presets")
        || lower.contains("\\user_presets")
}

fn classify_by_name(lower_name: &str, ext: &str) -> ProfileKind {
    if ext == "orca_printer" || lower_name.contains("printer") || lower_name.contains("machine") {
        ProfileKind::Printer
    } else if ext == "orca_filament" || lower_name.contains("filament") {
        ProfileKind::Filament
    } else if lower_name.contains("process") || lower_name.contains("print") {
        ProfileKind::Process
    } else if lower_name.contains("project") || lower_name.contains("3mf") {
        ProfileKind::Project
    } else {
        ProfileKind::Unknown
    }
}

fn likely_preset_json(_path: &Path, lower_name: &str) -> bool {
    // Heuristic: Orca presets/bundles commonly live in known dirs or have indicative names.
    lower_name.contains("printer")
        || lower_name.contains("filament")
        || lower_name.contains("process")
        || lower_name.contains("machine")
        || lower_name.contains("print")
        || lower_name.contains("preset")
        || lower_name.contains("orca")
        || lower_name.contains("profile")
}

fn likely_preset_ini(lower_name: &str) -> bool {
    lower_name.contains("printer")
        || lower_name.contains("filament")
        || lower_name.contains("process")
        || lower_name.contains("print")
        || lower_name.contains("preset")
        || lower_name.contains("orca")
        || lower_name.contains("profile")
}

fn stable_hash(s: &str) -> u64 {
    // FNV-1a 64-bit for stable ids without crypto dependency here
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[test]
    fn discovers_profiles_in_explicit_path() {
        let td = tempdir().unwrap();
        let pdir = td.path().join("profiles");
        fs::create_dir_all(&pdir).unwrap();
        fs::write(pdir.join("my_printer.json"), b"{\"name\":\"P\"}").unwrap();
        fs::write(pdir.join("fast_process.json"), b"{\"name\":\"Proc\"}").unwrap();
        fs::write(pdir.join("notes.txt"), b"ignore").unwrap();

        let mut cfg = Config::default();
        cfg.profiles.search_paths = vec![pdir.to_string_lossy().to_string()];
        cfg.profiles.use_os_defaults = false;
        cfg.workspace.root = td.path().join("ws").to_string_lossy().to_string();
        let idx = ProfileIndex::open(Arc::new(cfg)).unwrap();
        let catalog = idx.catalog().unwrap();
        assert!(catalog.entries.iter().any(|e| e.name == "my_printer"));
        assert!(catalog.entries.iter().any(|e| e.name == "fast_process"));
        assert!(!catalog.entries.iter().any(|e| e.name == "notes"));
    }
}
