//! First-time setup defaults recorded from a local `OrcaSlicer` configuration.
//!
//! The defaults file lives in the managed workspace (`orca_config/defaults.json`),
//! never inside the `OrcaSlicer` installation. Later tool calls can therefore reuse
//! the recorded printer/filament/process selection without rescanning or asking
//! the user again, while still allowing explicit arguments to win.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    error::{Error, Result},
    orca_config::Selection,
    workspace_expand::workspace_path,
};

/// Directory (under the workspace root) holding local-config setup state.
pub const DEFAULTS_DIR: &str = "orca_config";
/// File name of the recorded defaults.
pub const DEFAULTS_FILE: &str = "defaults.json";
/// Current defaults schema version.
pub const DEFAULTS_VERSION: u32 = 1;

/// Persisted first-time setup selection and its resolved essentials snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
    #[serde(default)]
    pub selection: Selection,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub essentials: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

const fn default_version() -> u32 {
    DEFAULTS_VERSION
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            version: DEFAULTS_VERSION,
            created_at: None,
            updated_at: None,
            config_dir: None,
            selection: Selection::default(),
            overrides: BTreeMap::new(),
            essentials: None,
            source: None,
            notes: Vec::new(),
        }
    }
}

impl Defaults {
    /// Absolute path of the defaults file for a configured workspace root.
    #[must_use]
    pub fn path(workspace_root: &str) -> PathBuf {
        workspace_path(workspace_root)
            .join(DEFAULTS_DIR)
            .join(DEFAULTS_FILE)
    }

    /// Load recorded defaults, returning `None` when setup has not run yet.
    pub fn load(workspace_root: &str) -> Result<Option<Self>> {
        let path = Self::path(workspace_root);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(Error::Config(format!(
                    "could not read defaults {}: {err}",
                    path.display()
                )))
            }
        };
        let defaults: Self = serde_json::from_slice(&bytes).map_err(|err| {
            Error::Config(format!("{}: invalid defaults JSON: {err}", path.display()))
        })?;
        Ok(Some(defaults))
    }

    /// Save these defaults atomically (temp file + rename) under the workspace.
    pub fn save(&mut self, workspace_root: &str) -> Result<PathBuf> {
        let path = Self::path(workspace_root);
        let dir = path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir).map_err(|err| {
            Error::Config(format!(
                "could not create defaults directory {}: {err}",
                dir.display()
            ))
        })?;

        let now = unix_now();
        if self.created_at.is_none() {
            self.created_at = Some(now);
        }
        self.updated_at = Some(now);
        self.version = DEFAULTS_VERSION;

        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|err| Error::Config(format!("could not encode defaults: {err}")))?;
        let temp = dir.join(format!(".{DEFAULTS_FILE}.tmp"));
        fs::write(&temp, bytes).map_err(|err| {
            Error::Config(format!(
                "could not write defaults staging file {}: {err}",
                temp.display()
            ))
        })?;
        fs::rename(&temp, &path).map_err(|err| {
            Error::Config(format!(
                "could not commit defaults {}: {err}",
                path.display()
            ))
        })?;
        Ok(path)
    }

    /// Delete recorded defaults. Returns whether a file was removed.
    pub fn clear(workspace_root: &str) -> Result<bool> {
        let path = Self::path(workspace_root);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(Error::Config(format!(
                "could not remove defaults {}: {err}",
                path.display()
            ))),
        }
    }

    /// Whether the recorded selection covers all three slicing roles.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.selection.is_complete()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_loads_and_clears_defaults() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_string_lossy().to_string();
        assert!(Defaults::load(&root).unwrap().is_none());

        let mut defaults = Defaults {
            selection: Selection {
                printer: Some("P".into()),
                filament: Some("F".into()),
                process: Some("S".into()),
            },
            ..Defaults::default()
        };
        let path = defaults.save(&root).unwrap();
        assert!(path.ends_with("orca_config/defaults.json"));
        assert_eq!(defaults.created_at, defaults.updated_at);

        let loaded = Defaults::load(&root).unwrap().unwrap();
        assert!(loaded.is_complete());
        assert_eq!(loaded.selection.printer.as_deref(), Some("P"));
        assert_eq!(loaded.version, DEFAULTS_VERSION);

        assert!(Defaults::clear(&root).unwrap());
        assert!(Defaults::load(&root).unwrap().is_none());
    }
}
