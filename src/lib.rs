//! `mcp-orca`: a structured Model Context Protocol server for `OrcaSlicer`.
//!
//! The crate is split into narrow layers:
//!
//! * [`config`] loads configuration from defaults, file, environment, and CLI.
//! * [`workspace`] owns the content-addressed artifact store and job dirs.
//! * [`geometry`] loads, inspects, and transforms meshes.
//! * [`profiles`] discovers, indexes, resolves, and manages `OrcaSlicer` presets/profiles.
//! * [`engine`] plans and invokes `OrcaSlicer` via host or Docker runners.
//! * [`gcode`] provides `G-code` artifact helpers and inspection hooks.
//! * [`slicing`] runs slice jobs in the background.
//! * [`safety`] is the policy and approval layer.
//! * [`tools`] exposes the MCP tool registry.
//! * [`server`] and [`transport`] speak MCP over stdio or HTTP.
//!
//! Nothing in this crate executes `G-code` or talks to a printer. The output is an
//! artifact that a human or a separate printer-control tool can inspect first.

pub mod audit;
pub mod config;
pub mod engine;
pub mod error;
pub mod gcode;
pub mod geometry;
pub mod orca_config;
pub mod profiles;
pub mod safety;
pub mod server;
pub mod slicing;
pub mod tools;
pub mod transport;
pub mod workspace;
pub mod workspace_expand;

/// Crate version, shown in server info.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
