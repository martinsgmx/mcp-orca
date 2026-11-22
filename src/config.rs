//! Configuration loading, normalization, and validation.
//!
//! Precedence, lowest to highest:
//! 1. built-in safe defaults
//! 2. optional TOML file (`--config` / `MCP_ORCA_CONFIG`)
//! 3. environment variables
//! 4. command-line flags (applied by `main.rs`)

use std::{env, net::IpAddr, path::Path, str::FromStr};

use crate::workspace::expand_tilde;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// MCP transport to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    #[default]
    Stdio,
    Http,
}

impl FromStr for TransportKind {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "stdio" => Ok(Self::Stdio),
            "http" | "streamable-http" | "streamable_http" => Ok(Self::Http),
            other => Err(format!(
                "unknown transport '{other}' (expected 'stdio' or 'http')"
            )),
        }
    }
}

/// How destructive actions are approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    #[default]
    InBand,
    OutOfBand,
    Both,
}

impl FromStr for ApprovalMode {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "in_band" | "in-band" | "inband" => Ok(Self::InBand),
            "out_of_band" | "out-of-band" | "outofband" => Ok(Self::OutOfBand),
            "both" => Ok(Self::Both),
            other => Err(format!(
                "unknown approval mode '{other}' (expected 'in_band', 'out_of_band' or 'both')"
            )),
        }
    }
}

/// Engine runner type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EngineRunner {
    #[default]
    Binary,
    Docker,
}

impl FromStr for EngineRunner {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "binary" | "host" => Ok(Self::Binary),
            "docker" => Ok(Self::Docker),
            other => Err(format!(
                "unknown engine runner '{other}' (expected 'binary' or 'docker')"
            )),
        }
    }
}

/// Binary detection mode for automatic OrcaSlicer binary discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BinaryDetection {
    /// Use only the configured binary name, no fallback detection.
    #[default]
    Manual,
    /// Try multiple common OrcaSlicer binary names and paths.
    Auto,
}

impl FromStr for BinaryDetection {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "manual" => Ok(Self::Manual),
            "auto" => Ok(Self::Auto),
            other => Err(format!(
                "unknown binary detection mode '{other}' (expected 'manual' or 'auto')"
            )),
        }
    }
}

/// HTTP transport settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
    #[serde(skip_serializing)]
    pub token: Option<SecretString>,
    pub allowed_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub allow_unauthenticated: bool,
    pub path: String,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8765,
            token: None,
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            allow_unauthenticated: false,
            path: "/mcp".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub transport: TransportKind,
    pub http: HttpConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub root: String,
    pub import_roots: Vec<String>,
    pub export_roots: Vec<String>,
    pub max_import_bytes: usize,
    pub max_artifact_bytes: usize,
    pub max_artifacts: usize,
    pub keep_jobs: usize,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            root: "~/.local/share/mcp-orca".to_string(),
            import_roots: Vec::new(),
            export_roots: Vec::new(),
            max_import_bytes: 512 * 1024 * 1024,
            max_artifact_bytes: 1024 * 1024 * 1024,
            max_artifacts: 10_000,
            keep_jobs: 100,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    pub runner: EngineRunner,
    pub binary: String,
    /// Binary detection mode: manual (use only configured binary) or auto (try multiple common OrcaSlicer names).
    #[serde(default)]
    pub binary_detection: BinaryDetection,
    pub docker_image: Option<String>,
    pub docker_command: Vec<String>,
    /// Built-in invocation preset. Currently only `"orca"` (aliases
    /// `"orcaslicer"` / `"orca-slicer"`) is recognized. When set and
    /// `slice_command` is empty, the adapter uses the documented `OrcaSlicer` CLI
    /// contract (`--load-settings`, `--load-filaments`, `--slice 0`,
    /// `--outputdir`, `--key=value` overrides) and discovers `plate_N.gcode`.
    /// An explicit `slice_command` always wins over the preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Argv template for the slicer invocation. Supports placeholders documented in
    /// `engine` module docs (e.g. `{binary}`, `{gcode}`, `{models}`, `{printer}`).
    pub slice_command: Vec<String>,
    /// Extra environment variables for the slicer process (host or Docker).
    /// Values support the same placeholders as `slice_command`.
    pub env: std::collections::BTreeMap<String, String>,
    pub timeout_secs: u64,
    pub max_concurrent_jobs: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            runner: EngineRunner::Binary,
            binary: "orca-slicer".to_string(),
            binary_detection: BinaryDetection::Auto,
            docker_image: None,
            docker_command: Vec::new(),
            preset: None,
            slice_command: Vec::new(),
            env: std::collections::BTreeMap::new(),
            timeout_secs: 900,
            max_concurrent_jobs: 2,
        }
    }
}

impl EngineConfig {
    /// Whether the configured preset is the built-in `OrcaSlicer` contract.
    #[must_use]
    pub fn orca_preset(&self) -> bool {
        self.preset.as_deref().is_some_and(|preset| {
            matches!(
                preset.trim().to_ascii_lowercase().as_str(),
                "orca" | "orcaslicer" | "orca-slicer"
            )
        })
    }

    /// Whether a slice invocation can be built: an explicit argv template or a
    /// recognized preset.
    #[must_use]
    pub fn slice_invocation_configured(&self) -> bool {
        !self.slice_command.is_empty() || self.orca_preset()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfilesConfig {
    pub search_paths: Vec<String>,
    pub use_os_defaults: bool,
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        Self {
            search_paths: Vec::new(),
            use_os_defaults: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    pub read_only: bool,
    pub allow_workspace_writes: bool,
    pub allow_slicing: bool,
    pub allow_export: bool,
    pub allow_delete: bool,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            read_only: false,
            allow_workspace_writes: true,
            allow_slicing: true,
            allow_export: true,
            allow_delete: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApprovalConfig {
    pub mode: ApprovalMode,
    pub ttl_secs: u64,
}

impl Default for ApprovalConfig {
    fn default() -> Self {
        Self {
            mode: ApprovalMode::InBand,
            ttl_secs: 120,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub workspace: WorkspaceConfig,
    pub engine: EngineConfig,
    pub profiles: ProfilesConfig,
    pub policy: PolicyConfig,
    pub approval: ApprovalConfig,
    pub logging: LoggingConfig,
}

impl Config {
    /// Load configuration from defaults, optional file, and environment.
    pub fn load(config_path: Option<&Path>) -> Result<Self> {
        let mut config = if let Some(path) = config_path {
            let text = std::fs::read_to_string(path).map_err(|err| {
                Error::Config(format!("could not read config {}: {err}", path.display()))
            })?;
            toml::from_str(&text)?
        } else {
            Self::default()
        };

        config.apply_env();
        config.validate()?;
        Ok(config)
    }

    fn apply_env(&mut self) {
        if let Ok(value) = env::var("MCP_ORCA_TRANSPORT") {
            if let Ok(kind) = value.parse() {
                self.server.transport = kind;
            }
        }
        if let Ok(value) = env::var("MCP_ORCA_HTTP_HOST") {
            self.server.http.host = value;
        }
        if let Ok(value) = env::var("MCP_ORCA_HTTP_PORT") {
            if let Ok(port) = value.parse() {
                self.server.http.port = port;
            }
        }
        if let Ok(value) = env::var("MCP_ORCA_HTTP_TOKEN") {
            if !value.is_empty() {
                self.server.http.token = Some(SecretString::from(value));
            }
        }
        if let Ok(value) = env::var("MCP_ORCA_WORKSPACE") {
            self.workspace.root = value;
        }
        if let Ok(value) = env::var("MCP_ORCA_ENGINE_RUNNER") {
            if let Ok(runner) = value.parse() {
                self.engine.runner = runner;
            }
        }
        if let Ok(value) = env::var("MCP_ORCA_ENGINE_BINARY") {
            self.engine.binary = value;
        }
        if let Ok(value) = env::var("MCP_ORCA_ENGINE_BINARY_DETECTION") {
            if let Ok(detection) = value.parse() {
                self.engine.binary_detection = detection;
            }
        }
        if let Ok(value) = env::var("MCP_ORCA_DOCKER_IMAGE") {
            if !value.is_empty() {
                self.engine.docker_image = Some(value);
            }
        }
        if let Ok(value) = env::var("MCP_ORCA_LOG_LEVEL") {
            self.logging.level = value;
        }
    }

    /// Validate configuration.
    pub fn validate(&self) -> Result<()> {
        if self.workspace.max_artifacts == 0 {
            return Err(Error::Config("workspace.max_artifacts must be > 0".into()));
        }
        if self.engine.timeout_secs == 0 {
            return Err(Error::Config("engine.timeout_secs must be > 0".into()));
        }
        if self.engine.max_concurrent_jobs == 0 {
            return Err(Error::Config(
                "engine.max_concurrent_jobs must be > 0".into(),
            ));
        }
        if let Some(preset) = &self.engine.preset {
            if !self.engine.orca_preset() {
                return Err(Error::Config(format!(
                    "unknown engine.preset '{preset}' (expected 'orca')"
                )));
            }
        }

        if self.server.transport == TransportKind::Http {
            let host: IpAddr =
                self.server.http.host.parse().map_err(|_| {
                    Error::Config("server.http.host must be a valid IP address".into())
                })?;
            let loopback = host.is_loopback();
            if !loopback
                && self.server.http.token.is_none()
                && !self.server.http.allow_unauthenticated
            {
                return Err(Error::Config(
                    "non-loopback HTTP requires MCP_ORCA_HTTP_TOKEN or server.http.allow_unauthenticated=true".into(),
                ));
            }
        }
        Ok(())
    }

    /// Public, secret-free view for printing/diagnostics.
    #[must_use]
    pub fn public_view(&self) -> serde_json::Value {
        serde_json::json!({
            "server": {
                "transport": self.server.transport,
                "http": {
                    "host": self.server.http.host,
                    "port": self.server.http.port,
                    "path": self.server.http.path,
                    "has_token": self.server.http.token.is_some(),
                    "allow_unauthenticated": self.server.http.allow_unauthenticated,
                }
            },
            "workspace": {
                "root": expand_tilde(&self.workspace.root),
                "import_roots": self.workspace.import_roots.iter().map(|p| expand_tilde(p)).collect::<Vec<_>>(),
                "export_roots": self.workspace.export_roots.iter().map(|p| expand_tilde(p)).collect::<Vec<_>>(),
                "max_import_bytes": self.workspace.max_import_bytes,
                "max_artifact_bytes": self.workspace.max_artifact_bytes,
                "max_artifacts": self.workspace.max_artifacts,
                "keep_jobs": self.workspace.keep_jobs,
            },
            "engine": self.engine,
            "profiles": self.profiles,
            "policy": self.policy,
            "approval": self.approval,
            "logging": self.logging,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        let cfg = Config::default();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.engine.binary, "orca-slicer");
        assert!(cfg.profiles.use_os_defaults);
    }

    #[test]
    fn preset_validation_and_slice_invocation() {
        let mut cfg = Config::default();
        assert!(!cfg.engine.slice_invocation_configured());
        cfg.engine.preset = Some("orca".into());
        assert!(cfg.validate().is_ok());
        assert!(cfg.engine.orca_preset());
        assert!(cfg.engine.slice_invocation_configured());

        cfg.engine.preset = Some("bogus".into());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn public_view_hides_secrets_shape() {
        let mut cfg = Config::default();
        cfg.server.http.token = Some(secrecy::SecretString::from("secret".to_string()));
        let v = cfg.public_view();
        assert_eq!(v["server"]["http"]["has_token"], serde_json::json!(true));
        assert!(v.get("token").is_none());
    }

    #[test]
    fn binary_detection_mode_defaults_to_auto() {
        let cfg = Config::default();
        assert_eq!(cfg.engine.binary_detection, BinaryDetection::Auto);
    }

    #[test]
    fn binary_detection_mode_can_be_set_to_manual() {
        let mut cfg = Config::default();
        cfg.engine.binary_detection = BinaryDetection::Manual;
        assert_eq!(cfg.engine.binary_detection, BinaryDetection::Manual);
    }

    #[test]
    fn binary_detection_environment_variable() {
        // Test that MCP_ORCA_ENGINE_BINARY_DETECTION environment variable is respected
        std::env::set_var("MCP_ORCA_ENGINE_BINARY_DETECTION", "manual");
        let cfg = Config::load(None).unwrap();
        assert_eq!(cfg.engine.binary_detection, BinaryDetection::Manual);
        std::env::set_var("MCP_ORCA_ENGINE_BINARY_DETECTION", "auto");
        let cfg = Config::load(None).unwrap();
        assert_eq!(cfg.engine.binary_detection, BinaryDetection::Auto);
        std::env::remove_var("MCP_ORCA_ENGINE_BINARY_DETECTION");
    }
}
