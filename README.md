# mcp-orca

A safe, structured [Model Context Protocol](https://modelcontextprotocol.io) server for **OrcaSlicer** and FDM 3D printing G-code generation workflows.

> **Status:** v1 implementation. Core services (workspace/artifacts, bounded profile discovery, mesh inspect/transform/merge, background slice jobs, policy/audit, CLI/doctor) are implemented and tested. Slicing execution is wired through a configurable engine argv template; the concrete OrcaSlicer CLI flags must be configured for your target version (`engine.slice_command`). See [`docs/DESIGN.md`](docs/DESIGN.md) and [`GLOSSARY.md`](GLOSSARY.md).

## Goals (v1)

- Generate G-code from 3D model(s) via OrcaSlicer with agent-controllable settings (profiles/presets, infill, temperatures, and key process controls).
- Discover/index Orca presets from bounded, OS-detected common locations and explicit paths; inspect/resolve settings; create/update **workspace-managed** custom profiles/overrides.
- Prepare meshes for slicing (**inspect, transform, merge**).
- Operate with a safe, inspectable model: content-addressed artifacts/workspace, background slice jobs, policy/approvals/audit gates.
- **No printer communication. No G-code execution.**

## Architecture (inspired by `mcp-cura`)

`mcp-orca` is a new standalone Rust project inspired by the structure and ergonomics of [`mcp-cura`](https://github.com/martinsgmx/mcp-cura) (used as a reference only, not a fork/copy): similar layering around tools, engine runners, profiles, slicing/jobs, workspace, and safety.

- **Runners:** host CLI and Docker (configurable adapter contract; concrete OrcaSlicer CLI/image wired per environment).
- **Essentials:** slicing requires resolved **printer + filament + process** presets (no invented defaults).
- **Profile safety:** no whole-system scans; bounded/explicit paths; backups before any in-place profile modification; custom profiles stored in the workspace.

## Build and run

Requires Rust 1.89 (see `rust-toolchain.toml`).

```sh
cargo build --release
cargo run -- init --output mcp-orca.toml   # starter config
cargo run -- --config mcp-orca.toml        # MCP server over stdio (default)
cargo run -- --config mcp-orca.toml tools  # list tools
cargo run -- --config mcp-orca.toml doctor # environment/diagnostics
cargo run -- --config mcp-orca.toml print-config
cargo run -- --config mcp-orca.toml approvals list
```

Configuration precedence: built-in defaults < TOML file < `MCP_ORCA_*` environment variables < CLI flags.

## Engine configuration (slicing)

Slicing stays disabled until `engine.slice_command` is set. The adapter stages models, `plan.json`, and `overrides.json` into the job directory, expands placeholders, runs the command, and imports the produced G-code as an artifact.

Placeholders: `{binary}`, `{image}`, `{jobdir}`, `{gcode}`, `{models}` (one argv entry per model), `{model}` (first model), `{printer}`, `{filament}`, `{process}`, `{project}`, `{overrides}`, `{settings}`, `{plan_json}`.

Extra slicer environment variables can be set with `engine.env`; values are placeholder-expanded too:

```toml
[engine]
env = { ORCA_PROFILE_DIR = "{jobdir}/profiles", MCP_ORCA_JOB = "{jobdir}" }
```

Example host configuration (adjust flags to your OrcaSlicer build):

```toml
[engine]
runner = "binary"
binary = "orca-slicer"
slice_command = [
  "{binary}", "--slice", "{models}",
  "--load", "{printer}", "--load", "{filament}", "--load", "{process}",
  "--output", "{gcode}",
]
timeout_secs = 900
```

Example Docker configuration:

```toml
[engine]
runner = "docker"
docker_image = "your-orcaslicer-image:tag"
docker_command = ["docker", "run", "--rm", "-v", "{jobdir}:{jobdir}", "-w", "{jobdir}"]
slice_command = ["{image}", "--slice", "{models}", "--output", "{gcode}"]
```

For Docker, also bind-mount any profile directories referenced by `{printer}`/`{filament}`/`{process}` so the container can read them.

## Safety, approvals, and audit

Every tool call goes through one registry choke point: policy evaluation, optional approval, execution, and an audit record.

- Policy (`[policy]`) allows a read-only mode and per-capability flags for workspace writes, slicing, export, and delete.
- `orca_artifact_export`, `orca_artifact_delete`, and other gated actions do not execute on the first attempt. They create a pending approval request and return its id.
- List requests with `orca_pending_approvals`, then approve or deny with `orca_approval_decide` (`approval.mode = "in_band"` or `"both"`). Retry the **identical** call within `approval.ttl_secs`; the approval is consumed exactly once.
- With `approval.mode = "out_of_band"`, MCP-side decisions are refused. Decide from the host instead: `mcp-orca approvals list`, `mcp-orca approvals approve <apr_id>`, `mcp-orca approvals deny <apr_id>`.
- Audit records are appended as JSONL to `<workspace>/audit.jsonl` (tool, timestamp, outcome, short sanitized detail). Secrets are never written.

## Tools

Admin/approvals: `orca_server_info`, `orca_engine_info`, `orca_policy_info`, `orca_pending_approvals`, `orca_approval_decide`, `orca_doctor`.

Artifacts: `orca_artifact_import`, `orca_artifact_list`, `orca_artifact_get`, `orca_artifact_read_text`, `orca_artifact_export`, `orca_artifact_delete`.

Mesh: `orca_mesh_inspect` (STL + OBJ), `orca_mesh_transform` (translate/rotate/scale/mirror; STL or OBJ in, binary STL out), `orca_mesh_merge` (STL/OBJ in, binary STL out).

Profiles: `orca_profile_list`, `orca_profile_get`, `orca_profile_refresh`, `orca_profile_resolve`, `orca_profile_create`, `orca_profile_update`.

Slicing/jobs: `orca_slice`, `orca_slice_status`, `orca_slice_wait`, `orca_slice_cancel`, `orca_slice_list`.

G-code: `orca_gcode_inspect`.

## Tests

```sh
cargo test
cargo clippy --all-targets
```

The integration suite uses a fake slicer script to exercise the full import → mesh → slice → G-code artifact path without OrcaSlicer installed.

## Documentation

- [Design spec (v1)](docs/DESIGN.md)
- [Glossary](GLOSSARY.md)
- [Scripts](scripts/README.md)

## License

Apache-2.0 — © 2026 Martin Sg. See [LICENSE](LICENSE).

> OrcaSlicer is a separate project with its own license; `mcp-orca` invokes it as an external tool.
