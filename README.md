# mcp-orca

A safe, structured [Model Context Protocol](https://modelcontextprotocol.io) server for **OrcaSlicer** and FDM 3D printing G-code generation workflows.

## Architecture (inspired by `mcp-cura`)

`mcp-orca` is a new standalone Rust project inspired by the structure and ergonomics of [`mcp-cura`](https://github.com/martinsgmx/mcp-cura) (used as a reference only, not a fork/copy): similar layering around tools, engine runners, profiles, slicing/jobs, workspace, and safety.

- **Runners:** host CLI and Docker. Use the built-in `engine.preset = "orca"` for the documented OrcaSlicer headless CLI, or provide `engine.slice_command` for a custom/target-specific invocation (`slice_command` always overrides the preset).
- **Essentials:** slicing requires resolved **printer + filament + process** presets (no invented defaults). The `orca` preset requires those selections to resolve to native OrcaSlicer `.json` preset files (paths, indexed presets, or a workspace profile's `base_profile`).
- **Profile safety:** no whole-system scans; bounded/explicit paths; backups before any in-place profile modification; custom profiles stored in the workspace.

## Build and run

Requires Rust 1.89 (see `rust-toolchain.toml`).

```sh
cargo build --release
cargo run -- init --output mcp-orca.toml   # starter config
cargo run -- --config mcp-orca.toml        # MCP server over stdio (default)
cargo run -- --config mcp-orca.toml tools  # list tools
cargo run -- --config mcp-orca.toml tools --json  # machine-readable catalog (risk/gate/args)
cargo run -- --config mcp-orca.toml doctor # environment/diagnostics
cargo run -- --config mcp-orca.toml print-config
cargo run -- --config mcp-orca.toml approvals list
```

Configuration precedence: built-in defaults < TOML file < `MCP_ORCA_*` environment variables < CLI flags.

## Engine configuration (slicing)

Slicing stays disabled until an invocation is configured: set `engine.preset = "orca"` **or** provide `engine.slice_command`. The adapter stages inputs into the job directory, runs the slicer with timeout + cancellation, captures logs, and imports the produced G-code as an artifact.

### Automatic binary detection

When `engine.binary_detection = "auto"` (default), `mcp-orca` will automatically try to discover available OrcaSlicer binaries on your system if the configured `engine.binary` is not found. It tries the following names in order:

1. `OrcaSlicer` (Electron app name)
2. `OrcaSlicer-CLI` (CLI binary)
3. `OrcaSlicer.AppImage` (AppImage)
4. `orcaslicer` (lowercase)
5. `orcaslicer-cli`
6. `OrcaSlicerCLI`

This allows the server to work regardless of how OrcaSlicer was installed (system package, manual download, AppImage, etc.) without requiring explicit configuration of the exact binary name.

To disable this behavior and use only the configured `engine.binary`, set `binary_detection = "manual"`.

### Built-in OrcaSlicer preset (`engine.preset = "orca"`)

Uses the documented OrcaSlicer headless CLI contract (researched from the upstream wiki):

- Invocation shape: staged model(s) + `--load-settings "process.json;printer.json"` + `--load-filaments filament.json` + merged setting overrides + `--arrange 1 --slice 0 --outputdir {jobdir} --export-settings {jobdir}/orca_effective_settings.json`.
- **Native presets required:** `printer`/`filament`/`process` selections must resolve to real `.json` preset files (direct path, indexed/discovered preset, or a workspace custom-profile artifact whose `base_profile` points at one). Workspace profile artifacts still contribute merged setting overrides.
- **Setting overrides:** merged workspace/override settings are passed as `--key=value`. OrcaSlicer CLI flags use hyphens, so setting keys are converted underscore→hyphen (`layer_height` → `--layer-height`). Booleans become `1`/`0`, arrays are comma-joined. Preset-identity, print-host, and internal extruder-bookkeeping keys that Orca excludes from CLI overrides are omitted.
- **Output:** Orca writes `plate_<N>.gcode` into `--outputdir`; the preset collects results into the job's `output.gcode` artifact (single plate: renamed/copied; multi-plate: concatenated in sorted order with plate markers, while individual plate files remain in the job directory). Use a custom `slice_command` for per-plate artifact handling.
- **Docker:** preset JSONs are staged into the mounted job directory so the container can read them.

```toml
[engine]
runner = "binary"
preset = "orca"
binary = "orca-slicer"
# Binary detection mode: "manual" (use only configured binary) or "auto" (try multiple common OrcaSlicer names)
# binary_detection = "auto"
# docker_image = "your-orcaslicer-image:tag"
timeout_secs = 900
```

### Custom argv template (`engine.slice_command`)

`slice_command` (and `docker_command`) are argv arrays expanded per job. An explicit `slice_command` overrides the preset.

Placeholders: `{binary}`, `{image}`, `{jobdir}`, `{gcode}`, `{models}` (one argv entry per model), `{model}` (first model), `{printer}`, `{filament}`, `{process}`, `{project}`, `{overrides}`, `{settings}`, `{plan_json}`.

Extra slicer environment variables can be set with `engine.env`; values are placeholder-expanded too:

```toml
[engine]
env = { ORCA_PROFILE_DIR = "{jobdir}/profiles", MCP_ORCA_JOB = "{jobdir}" }
```

Example custom host template:

```toml
[engine]
runner = "binary"
binary = "orca-slicer"
slice_command = [
  "{binary}", "{models}",
  "--load-settings", "{process};{printer}",
  "--load-filaments", "{filament}",
  "--arrange", "1", "--slice", "0",
  "--outputdir", "{jobdir}",
]
```

Example Docker configuration:

```toml
[engine]
runner = "docker"
preset = "orca" # or provide slice_command
docker_image = "your-orcaslicer-image:tag"
# docker_command = ["docker", "run", "--rm", "-v", "{jobdir}:{jobdir}", "-w", "{jobdir}"]
```

For Docker, the job directory is mounted by default; bind-mount any additional directories needed by a custom `slice_command`. The built-in preset stages required preset files into the job directory.

## Safety, approvals, and audit

Every tool call goes through one registry choke point: policy evaluation, optional approval, execution, and an audit record.

- Policy (`[policy]`) allows a read-only mode and per-capability flags for workspace writes, slicing, export, and delete.
- `orca_artifact_export`, `orca_artifact_delete`, and other gated actions do not execute on the first attempt. They create a pending approval request and return its id.
- List requests with `orca_pending_approvals`, then approve or deny with `orca_approval_decide` (`approval.mode = "in_band"` or `"both"`). Retry the **identical** call within `approval.ttl_secs`; the approval is consumed exactly once.
- With `approval.mode = "out_of_band"`, MCP-side decisions are refused. Decide from the host instead: `mcp-orca approvals list`, `mcp-orca approvals approve <apr_id>`, `mcp-orca approvals deny <apr_id>`.
- Audit records are appended as JSONL to `<workspace>/audit.jsonl` (tool, timestamp, outcome, short sanitized detail). Secrets are never written.

## Tools

Admin/approvals: `orca_server_info`, `orca_engine_info`, `orca_policy_info`, `orca_pending_approvals`, `orca_approval_decide`, `orca_doctor`.

Local OrcaSlicer config: `orca_config_scan` (bounded read-only discovery of user JSON presets and system `*.opc` caches), `orca_config_profiles` (list printer/filament/process presets with filter), `orca_config_resolve` (resolve `inherits`/`includes` and extract printing essentials for one preset or a printer+filament+process selection), `orca_config_setup` (first-time setup: detect the active OrcaSlicer selection or accept explicit names, save defaults in the workspace), `orca_config_defaults` (read the saved defaults and essentials snapshot). `orca_slice` reuses these saved defaults when printer/filament/process are omitted; explicit arguments win.

Artifacts: `orca_artifact_import` (reports requested/stored kind; preserves explicit mesh/gcode intent), `orca_artifact_list`, `orca_artifact_get`, `orca_artifact_read_text`, `orca_artifact_update_metadata` (repair name/kind/metadata without changing bytes), `orca_artifact_export`, `orca_artifact_delete`.

Mesh: `orca_mesh_inspect` (STL + OBJ; can inspect misclassified mesh artifacts by format/content signature with a warning), `orca_mesh_transform` (translate/rotate/scale/mirror; STL or OBJ in, binary STL out), `orca_mesh_merge` (STL/OBJ in, binary STL out).

Profiles: `orca_profile_list`, `orca_profile_get` (discovered entries + workspace artifacts), `orca_profile_refresh`, `orca_profile_resolve` (merged effective settings, completeness reason/next checks). Discovery is bounded but path-aware: Orca user preset dirs like `user/default/{machine,filament,process}` are recognized, and list/refresh report `discovery_coverage`/`scanned_user_dirs`/`excluded_patterns`.

Slicing/jobs: `orca_slice`, `orca_slice_status`, `orca_slice_wait`, `orca_slice_cancel`, `orca_slice_list`.

G-code: `orca_gcode_inspect` (reports computed metrics, slicer summary comments when present, discrepancy warnings, and safety-relevant command scan).

## Tests

```sh
cargo test
cargo clippy --all-targets
./scripts/smoke.sh      # fmt-adjacent checks: tests, clippy, tools, doctor, stdio handshake
./scripts/http_smoke.sh # token-gated HTTP: 401 without token, 200 with token
```

The integration suite uses fake host/Docker slicer scripts to exercise the full import → mesh → slice → G-code artifact path without OrcaSlicer installed, plus approval, profile-resolution, and mesh (STL/OBJ) flows.

## Documentation

- [Design spec (v1)](docs/DESIGN.md)
- [Glossary](GLOSSARY.md)
- [Scripts](scripts/README.md)

## License

Apache-2.0 — © 2026 Martin Sg. See [LICENSE](LICENSE).

> OrcaSlicer is a separate project with its own license; `mcp-orca` invokes it as an external tool.
