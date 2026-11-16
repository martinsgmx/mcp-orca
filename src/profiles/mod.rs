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
            catalog.scanned_dirs.push(dir.to_string_lossy().to_string());
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
                if !seen.insert(path.clone()) {
                    continue;
                }
                if let Some(profile_entry) = classify_entry(&path, source) {
                    catalog.entries.push(profile_entry);
                }
            }
        }

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

    let (kind, is_candidate) = match ext.as_str() {
        "3mf" => (ProfileKind::Project, true),
        "orca_printer" | "orca_filament" => (classify_by_name(&lower, &ext), true),
        "json" => (
            classify_by_name(&lower, &ext),
            likely_preset_json(path, &lower),
        ),
        "ini" | "cfg" => (classify_by_name(&lower, &ext), likely_preset_ini(&lower)),
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
