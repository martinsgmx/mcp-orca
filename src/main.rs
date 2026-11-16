//! Command-line interface for mcp-orca.

use std::{path::PathBuf, process::ExitCode, sync::Arc};

use clap::{Args, Parser, Subcommand};
use mcp_orca::{
    config::{Config, EngineRunner, TransportKind},
    error::Result,
    server::{server_details, OrcaServerHandler},
    tools::{ToolContext, ToolRegistry},
    transport,
};

/// Starter configuration written by `mcp-orca init`.
const STARTER_CONFIG: &str = r#"# mcp-orca configuration
# Precedence: built-in defaults < this file < environment variables < CLI flags.

[server]
transport = "stdio"

[server.http]
host = "127.0.0.1"
port = 8765
path = "/mcp"
allow_unauthenticated = false

[workspace]
root = "~/.local/share/mcp-orca"
max_import_bytes = 536870912
max_artifact_bytes = 1073741824
max_artifacts = 10000
keep_jobs = 100

[engine]
# runner: "binary" (host CLI) or "docker"
runner = "binary"
# Configure the OrcaSlicer CLI/image for your target. Slicing stays disabled
# while slice_command is empty.
binary = "orca-slicer"
# docker_image = "ghcr.io/example/orcaslicer:latest"
timeout_secs = 900
max_concurrent_jobs = 2
# Argv template. Placeholders: {binary} {image} {jobdir} {gcode} {models}
# {model} {printer} {filament} {process} {project} {overrides} {settings} {plan_json}
# slice_command = ["{binary}", "--slice", "{models}", "--output", "{gcode}"]
# docker_command = ["docker", "run", "--rm", "-v", "{jobdir}:{jobdir}", "-w", "{jobdir}"]
# Extra environment variables for the slicer process (placeholder-expanded).
# env = { ORCA_PROFILE_DIR = "{jobdir}/profiles" }

[profiles]
# Bounded discovery: explicit paths + OS-detected common defaults (no whole-system scan).
search_paths = []
use_os_defaults = true

[policy]
read_only = false
allow_workspace_writes = true
allow_slicing = true
allow_export = true
allow_delete = true

[approval]
# in_band (default) / out_of_band / both. out_of_band disables MCP-side
# decisions; use `mcp-orca approvals approve|deny <id>` instead.
mode = "in_band"
ttl_secs = 120

[logging]
level = "info"
"#;

#[derive(Debug, Parser)]
#[command(
    name = "mcp-orca",
    version,
    about = "Structured OrcaSlicer G-code generation MCP server",
    long_about = "A Model Context Protocol server for OrcaSlicer. It manages mesh preparation, profile discovery/resolution, background slice jobs, and artifact handling.\n\nNo printer communication. No G-code execution.\n\nRunning without a subcommand starts the stdio server.",
    propagate_version = true
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(long, global = true, env = "MCP_ORCA_CONFIG", value_name = "FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the MCP server (default when no subcommand is given).
    Serve(ServeArgs),
    /// Check configuration, workspace, engine, and profiles.
    Doctor(DoctorArgs),
    /// Print the fully resolved, secret-free configuration.
    PrintConfig,
    /// List MCP tools.
    Tools,
    /// Write a starter configuration file.
    Init(InitArgs),
    /// Inspect or decide approvals out of band (no MCP connection).
    Approvals(ApprovalsArgs),
}

#[derive(Debug, Args)]
struct ApprovalsArgs {
    #[command(subcommand)]
    action: ApprovalsAction,
}

#[derive(Debug, Subcommand)]
enum ApprovalsAction {
    /// List pending, unexpired approval requests.
    List,
    /// Approve a pending request.
    Approve {
        /// Approval id (apr_...).
        approval_id: String,
        /// Optional note stored with the decision.
        #[arg(long)]
        note: Option<String>,
    },
    /// Deny a pending request.
    Deny {
        /// Approval id (apr_...).
        approval_id: String,
        /// Optional note stored with the decision.
        #[arg(long)]
        note: Option<String>,
    },
}

#[derive(Debug, Clone, Args, Default)]
struct ServeArgs {
    /// Transport: stdio (default) or http.
    #[arg(long, value_name = "TRANSPORT")]
    transport: Option<String>,
    /// HTTP bind host.
    #[arg(long, value_name = "HOST")]
    http_host: Option<String>,
    /// HTTP bind port.
    #[arg(long, value_name = "PORT")]
    http_port: Option<u16>,
    /// Engine runner: binary or docker.
    #[arg(long, value_name = "RUNNER")]
    engine_runner: Option<String>,
    /// `OrcaSlicer` executable name or absolute path.
    #[arg(long, value_name = "BIN")]
    engine_bin: Option<String>,
    /// Docker image for the `OrcaSlicer` CLI.
    #[arg(long, value_name = "IMAGE")]
    docker_image: Option<String>,
    /// Workspace root.
    #[arg(long, value_name = "DIR")]
    workspace: Option<PathBuf>,
}

#[derive(Debug, Args, Default)]
struct DoctorArgs {
    /// `OrcaSlicer` executable name or absolute path.
    #[arg(long, value_name = "BIN")]
    engine_bin: Option<String>,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Output path for the starter config.
    #[arg(long, default_value = "mcp-orca.toml", value_name = "FILE")]
    output: PathBuf,
    /// Overwrite if the file exists.
    #[arg(long)]
    force: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Err(err) = run(cli).await {
        eprintln!("mcp-orca: {}", err.public_message());
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Command::Tools) => {
            init_tracing("info");
            let registry = ToolRegistry::new();
            for tool in registry.list_tools() {
                println!("{}\t{}", tool.name, tool.description.unwrap_or_default());
            }
            Ok(())
        }
        Some(Command::Init(args)) => {
            init_tracing("info");
            init_config(&args)
        }
        Some(Command::Approvals(args)) => {
            let config = Config::load(cli.config.as_deref())?;
            init_tracing(&config.logging.level);
            approvals_command(&config, args)
        }
        other => {
            let mut config = Config::load(cli.config.as_deref())?;
            match &other {
                Some(Command::Serve(args)) => apply_serve_args(&mut config, args.clone()),
                Some(Command::Doctor(args)) => {
                    if let Some(bin) = args.engine_bin.clone() {
                        config.engine.binary = bin;
                    }
                }
                _ => {}
            }
            init_tracing(&config.logging.level);
            match other {
                None | Some(Command::Serve(_)) => serve(config).await,
                Some(Command::Doctor(_)) => doctor(config).await,
                Some(Command::PrintConfig) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&config.public_view())
                            .unwrap_or_else(|_| "{}".into())
                    );
                    Ok(())
                }
                _ => unreachable!("handled above"),
            }
        }
    }
}

fn approvals_command(config: &Config, args: ApprovalsArgs) -> Result<()> {
    use mcp_orca::safety::approval::ApprovalStore;

    let dir = mcp_orca::workspace_expand::workspace_path(&config.workspace.root).join("approvals");
    let store = ApprovalStore::new(dir, config.approval.ttl_secs)?;
    match args.action {
        ApprovalsAction::List => {
            let pending = store.list_pending()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "count": pending.len(),
                    "pending": pending,
                }))
                .unwrap_or_else(|_| "{}".into())
            );
        }
        ApprovalsAction::Approve { approval_id, note } => {
            let request = store.decide(&approval_id, true, note)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&request).unwrap_or_else(|_| "{}".into())
            );
        }
        ApprovalsAction::Deny { approval_id, note } => {
            let request = store.decide(&approval_id, false, note)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&request).unwrap_or_else(|_| "{}".into())
            );
        }
    }
    Ok(())
}

fn apply_serve_args(config: &mut Config, args: ServeArgs) {
    if let Some(transport) = args.transport {
        if let Ok(kind) = transport.parse() {
            config.server.transport = kind;
        }
    }
    if let Some(host) = args.http_host {
        config.server.http.host = host;
    }
    if let Some(port) = args.http_port {
        config.server.http.port = port;
    }
    if let Some(runner) = args.engine_runner {
        if let Ok(r) = runner.parse::<EngineRunner>() {
            config.engine.runner = r;
        }
    }
    if let Some(bin) = args.engine_bin {
        config.engine.binary = bin;
    }
    if let Some(image) = args.docker_image {
        config.engine.docker_image = Some(image);
    }
    if let Some(workspace) = args.workspace {
        config.workspace.root = workspace.to_string_lossy().to_string();
    }
}

async fn serve(config: Config) -> Result<()> {
    let ctx = Arc::new(ToolContext::new(config.clone())?);
    let registry = Arc::new(ToolRegistry::new());
    let handler = OrcaServerHandler::new(registry, ctx);
    let details = server_details();
    match config.server.transport {
        TransportKind::Stdio => transport::run_stdio(&config, handler, details).await,
        TransportKind::Http => transport::run_http(&config, handler, details).await,
    }
}

async fn doctor(config: Config) -> Result<()> {
    let ctx = Arc::new(ToolContext::new(config)?);
    let registry = ToolRegistry::new();
    let result = registry
        .call(&ctx, "orca_doctor", None)
        .await
        .unwrap_or_else(|err| mcp_orca::tools::tool_error(&err));
    let text = result
        .content
        .iter()
        .find_map(|c| match c {
            rust_mcp_sdk::schema::ContentBlock::TextContent(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "{}".to_string());
    println!("{text}");
    Ok(())
}

fn init_config(args: &InitArgs) -> Result<()> {
    if args.output.exists() && !args.force {
        return Err(mcp_orca::error::Error::Config(format!(
            "refusing to overwrite existing {} (use --force)",
            args.output.display()
        )));
    }
    std::fs::write(&args.output, STARTER_CONFIG)?;
    println!("wrote {}", args.output.display());
    Ok(())
}

fn init_tracing(default_level: &str) {
    let filter = std::env::var("MCP_ORCA_LOG_LEVEL")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| default_level.to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
