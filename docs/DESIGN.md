# mcp-orca — v1 Implementation Design Spec

**Status:** Confirmed shared understanding (v1 design baseline)  
**Date:** 2026-11-15  
**Project:** `mcp-orca` (Rust MCP server for OrcaSlicer G-code generation)  
**License:** Apache-2.0 (© 2026 Martin Sg)  
**Reference:** Inspired by the architecture and scaffolding/style of `~/repos/mcp-cura` (CuraEngine MCP server); **not** a fork and not a code copy. `mcp-cura` is used only as a structural/stylistic reference.

> This document captures the agreed v1 design. Items marked **[Placeholder / to wire]** require environment-specific details (notably the exact OrcaSlicer CLI invocation) and must be finalized when a concrete OrcaSlicer target is integrated.

## 1. Summary

`mcp-orca` is a safe, structured [Model Context Protocol](https://modelcontextprotocol.io) server that wraps **OrcaSlicer** for **FDM G-code generation** workflows.

v1 focuses on:
- Generating G-code from 3D model(s) using resolved **printer + filament + process** presets with agent-controllable settings (infill, temperatures, and other key process controls).
- **Profile discovery/indexing** from bounded, OS-detected common locations and/or explicit paths, plus **profile inspection/resolution** and **workspace-managed custom profile/override** creation and updates.
- Basic **mesh preparation** for slicing: inspect, transform (scale/rotate/translate/mirror), and merge.
- A `mcp-cura`-like operational model: content-addressed **workspace/artifacts**, background **slice jobs**, structured results, **policy/approvals/audit** gates, and **stdio** default transport (optional HTTP).

**Non-goals / hard boundaries (v1):**
- No printer communication or control.
- No execution of generated G-code.
- No mutation of user/system OrcaSlicer installations in place (custom/derived profiles are written to the workspace; any exceptional in-place edit requires a prior timestamped backup by design).
- No whole-system filesystem crawling for profiles.
- No invented/hardcoded fallback defaults when essential presets (printer/filament/process) cannot be resolved.

## 2. Confirmed decisions (design baseline)

| Area | Decision |
|---|---|
| Primary job | v1 G-code generation via OrcaSlicer with controllable infill/profile/temps and key process settings (A). |
| Mesh prep | v1 includes **inspect + full transform + merge** (B path, scoped subset of mcp-cura breadth). |
| Relationship to mcp-cura | New standalone Rust project inspired by `mcp-cura` architecture/patterns/style (A). |
| Engine runners | Support **both host and Docker** runners (configurable); design is generic/adapter-based in v1. |
| Profiles (sources) | Discovery/index from **explicit paths + documented common OS-detected defaults** (only if present), no broad scan; Docker requires mounts (A). |
| Profile management | Read/discover/get/refresh/resolve + **create/update custom profiles/overrides in workspace**; backup before any in-place modification (B + safety). |
| Settings surface | **Slice-time overrides and profile tools** (C) with broad v1 controls (infill, temps, layer height, speeds, supports, walls/top-bottom, seams, retraction, cooling, flow, brim/skirt/raft, accel/jerk, etc.) and pass-through/advanced overrides by key. |
| Essential presets fallback | **Require resolved printer + filament + process**; if missing/incomplete, fail clearly with guidance. No invented defaults (A). |
| CLI contract (current) | **Generic/examples-only** runner/adapter contract in v1; concrete OrcaSlicer binary/image and invocation to be wired later (A). |
| Profile recency/defaults | **OS-detected common paths** as v1 guarantee; opportunistic recency ordering only if trivial/non-brittle; **no implicit auto-selection** that hides missing essentials (C). |
| Operational model | `mcp-cura`-like: content-addressed workspace/artifacts, background jobs, policy/approvals/audit, no printer/G-code exec, stdio default (A). |
| Naming & license | Crate/bin `mcp-orca`, tools `orca_*`, **Apache-2.0**, © 2026 Martin Sg (A, adapted). |

## 3. Architecture overview

The server follows a layered, single-choke-point design (mirroring `mcp-cura`’s feel):

1. **Transport** (`stdio` default, optional HTTP/SSE) receives MCP requests and emits responses/diagnostics appropriately.
2. **Server/registry** exposes tool metadata (names, schemas, annotations) and routes `call` through a **single execution path**.
3. **Policy & approvals** evaluate capability/risk/gates per tool; high-risk or gated actions produce approvals/confirmations rather than executing directly.
4. **Tool handlers** (thin) validate/normalize args and delegate to domain services.
5. **Domain services** implement workspace/artifacts, mesh ops, profile index/resolve/customization, slicing/job orchestration, and (optionally later) G-code inspection — all operating on workspace state and configured runners.
6. **Engine adapter** abstracts **host** vs **Docker** invocation behind a common slice-run contract.
7. **Audit** records decisions and outcomes; **doctor** reports readiness.

### 3.1 Proposed module/scaffolding layout

Modeled after `mcp-cura` (adapted to Orca):

```text
src/
  main.rs              # CLI entry (serve default, doctor/tools/print-config/init, approvals, etc.)
  lib.rs               # library surface
  config.rs            # config sources/merge/validation (defaults < file < env < CLI)
  server.rs            # MCP server handler/wiring
  transport.rs         # stdio / HTTP-SSE setup
  error.rs             # error types & user-facing diagnostics
  audit.rs             # audit logging
  workspace/           # content-addressed artifacts, paths, GC/retention
  engine/              # runner abstraction (host/docker), command planning, process control
  profiles/            # discovery/index, parsing, resolution/merge, custom profiles, backups
  slicing/             # job model, queue/concurrency, execution, status/results
  gcode/               # (v1 minimal/optional) G-code metadata/read helpers; room to grow
  geometry/            # mesh inspect/transform/merge
  safety/              # policy, approvals, redaction/guards
  tools/               # ToolSpec registry + arg schemas/handlers by family
    mod.rs
    admin.rs
    artifacts.rs
    geometry.rs
    profiles.rs
    slicing.rs
    gcode.rs           # G-code artifact inspection
docs/
  DESIGN.md
examples/
scripts/
```

(Exact file split may evolve during implementation, but the separation of concerns and feel should remain consistent with `mcp-cura`.)

## 4. Configuration model

- **Precedence:** built-in defaults → config file (e.g. TOML) → environment variables → CLI flags.
- **Secrets:** never persisted in config files; HTTP auth tokens (if enabled) supplied via env/secret mechanisms.
- **Key config areas (v1):**
  - `server`: transport (`stdio`/`http`), HTTP host/port/path/auth settings.
  - `workspace`: root path, retention/GC, size/temp limits.
  - `engine`: `runner` (`binary`|`docker`), host command/path/args template & env, docker image/command/mounts/workdir/pull policy, timeouts, concurrency limits.
  - `profiles`: `search_paths` (explicit), `use_os_defaults` (bool), optional include/exclude hints, workspace custom-profile dir.
  - `safety/policy`: capability/risk gates, approval mode, limits (e.g. mesh/job/resource/time/temperature sanity caps where applicable), export/delete controls.
  - `logging/audit`: levels, destinations.

Provide: `print-config` (resolved, secret-free), `init` (starter config), and `doctor` diagnostics.

## 5. Engine runners and slice contract

v1 provides two ways to invoke OrcaSlicer: a **built-in preset** for the documented headless CLI (`engine.preset = "orca"`) and a **generic argv-template adapter** (`engine.slice_command`). An explicit `slice_command` always overrides the preset. A real OrcaSlicer binary/image remains environment-specific and is not bundled.

### 5.1 Runners
- **Host (`binary`)**: execute the resolved host command. Resolve `{binary}` via absolute path or PATH. Capture stdout/stderr/logs to the job workspace.
- **Docker (`docker`)**: run `engine.docker_command` (default: `docker run --rm -v {jobdir}:{jobdir} -w {jobdir}`) with `engine.docker_image`. No implicit mounts beyond the job directory; additional mounts must be added explicitly to `docker_command` when a custom invocation references external paths. The built-in preset stages required preset files into the mounted job directory.

### 5.2 Built-in OrcaSlicer preset (`engine.preset = "orca"`)

Researched from the upstream OrcaSlicer wiki (`cli_mode`, `cli_actions`, `cli_misc`, `cli_transform`) and implemented as an opt-in preset:

- **Invocation:** staged model(s) as positional inputs, then `--load-settings "process.json;printer.json"` and `--load-filaments filament.json` (semicolon-separated; printer+process share one `--load-settings` value), merged setting overrides as `--key=value`, `--arrange 1`, `--slice 0` (all plates), `--outputdir {jobdir}`, and `--export-settings {jobdir}/orca_effective_settings.json`.
- **Preset files:** OrcaSlicer requires native machine/process/filament JSON presets. Selections must resolve to `.json` files (direct path, discovered/indexed preset entry, or a workspace custom-profile artifact's `base_profile`). Workspace profile artifacts still contribute merged setting overrides.
- **Setting overrides / key mapping:** CLI flags use hyphens; setting keys are converted underscore→hyphen (e.g. `layer_height` → `--layer-height`). Booleans are rendered as `1`/`0`; vector/scalar-array values are comma-joined. Preset-identity/inheritance, print-host credential, and internal extruder-bookkeeping keys excluded by Orca's CLI contract are omitted from `--key=value` overrides.
- **Output handling:** Orca writes `plate_<N>.gcode` into `--outputdir`. The preset collects a single-plate result into the job's `output.gcode` artifact and fails clearly on zero or multi-plate output (multi-plate/custom naming requires `engine.slice_command`).
- **Staging:** models and resolved preset JSONs are copied into the job directory so host and Docker runs use mounted/invocation-local paths.

### 5.3 Custom argv template (`engine.slice_command`)
`engine.slice_command` (and `engine.docker_command`) are argv arrays expanded per job with these placeholders:

| Placeholder | Value |
|---|---|
| `{binary}` | configured host binary |
| `{image}` | configured Docker image |
| `{jobdir}` | job working directory (also staged inputs/output) |
| `{gcode}` | expected output path: `<jobdir>/output.gcode` |
| `{models}` | one argv entry per staged model path |
| `{model}` | first staged model path |
| `{printer}`, `{filament}`, `{process}`, `{project}` | selected essentials |
| `{overrides}` | path to `<jobdir>/overrides.json` |
| `{settings}` | path to `<jobdir>/settings.json` (merged workspace-profile + override settings) |
| `{plan_json}` | path to `<jobdir>/plan.json` |

`engine.env` adds environment variables to the slicer process (host or Docker); values are expanded with the same placeholders and passed verbatim to the child.

The template engine stages model files, `plan.json`, `overrides.json`, and `settings.json`, clears any stale `output.gcode`, runs with timeout + cancellation, and fails clearly when the command exits non-zero, times out, or does not produce non-empty G-code. The G-code file is then imported as a content-addressed artifact linked to the job.

### 5.4 Common slice contract (conceptual)
A slice invocation must be expressible as:
- **Inputs:** one or more model artifact/file paths (ordered).
- **Output:** target G-code path within the job/workspace (imported as an artifact on success).
- **Essentials:** selected/resolved **printer**, **filament**, and **process** presets (by id/name/path or project context, per integration).
- **Overrides:** a map of setting overrides (typed/known keys + pass-through keys) applied according to the adapter’s mechanism.
- **Execution controls:** timeout, cancellation token, optional thread/resource hints, environment.

**Environment-specific:** real OrcaSlicer binary/image availability, exact preset naming/contents, multi-plate workflows, and version-specific CLI behavior remain deployment concerns; `slice_command` and Docker mounts provide the escape hatch.

### 5.3 Reliability & degradation
- `doctor` reports runner availability/config validity and profile source health without requiring a successful slice.
- Read-only and workspace/profile/mesh tools should function (where meaningful) even if the engine is unavailable; slicing tools return clear, actionable errors when not configured.

## 6. Profiles: discovery, resolution, custom profiles, safety

### 6.1 Discovery/index (bounded, OS-detected)
- Build an index from: (1) explicit configured `search_paths`, (2) optional **common OS-detected defaults** (enabled by config), and (3) explicitly provided paths in requests.
- **No broad/system-wide scan.** Only known, documented directories are considered, and only if they exist/readable.
- **OS-detected common defaults (best-effort, to validate/refine):**
  - **Linux:** `~/.config/OrcaSlicer/`, `~/.OrcaSlicer/`, `~/.local/share/OrcaSlicer/` (user); `/usr/share/OrcaSlicer/` and similar known resource locations (system) — only if present.
  - **macOS:** `~/Library/Application Support/OrcaSlicer/` (user); app-bundle/resource locations as applicable — only if present.
  - **Windows:** `%APPDATA%\OrcaSlicer\` (user); installation/resource locations as applicable — only if present.
- **Docker:** no implicit host-path discovery inside container; docs must specify bind mounts/volumes and corresponding in-container `search_paths` (or explicit mounts of user/system profile dirs).
- Index entries should capture: stable id/handle, kind (printer/filament/process/project/unknown as applicable), display name, source path, and lightweight metadata for listing/filtering. Ordering may be deterministic (e.g. name/path) with opportunistic recency hints only if cheaply available.

### 6.2 Resolution and essentials
- Provide a **resolve** capability that produces an **effective settings view** from selected printer/filament/process presets plus layered overrides/custom profiles, with warnings where keys are unknown/unmapped.
- **Invariant:** slicing requires resolved **printer + filament + process**. If any essential is missing/ambiguous/unresolvable, fail with a clear error indicating what is missing and acceptable ways to provide it (ids/names/paths/project). **No invented defaults.**

### 6.3 Custom profiles & write safety
- **Create/update custom profiles/overrides in the workspace** (e.g. named custom preset/override artifacts) so agents can reuse configurations safely.
- **Never mutate user/system OrcaSlicer installs by default.** If any future/optional flow edits an existing on-disk profile in place, it **must create a timestamped backup first** and record/report the backup location.
- Refresh/reindex support after workspace/custom profile changes.

**[Placeholder]:** exact Orca preset file formats (json/ini/bundle/3mf-embedded, inheritance semantics) and mapping to printer/filament/process kinds must be confirmed against the target OrcaSlicer version.

## 7. Settings model (v1 controls)

v1 exposes first-class, agent-friendly controls (as tool args and/or custom-profile fields) mapped to OrcaSlicer settings keys:

- **Preset selection:** printer, filament, process (and/or project/profile paths).
- **Infill:** pattern/type, density/sparse infill (%).
- **Temperatures:** nozzle, bed, chamber (where applicable).
- **Layers:** layer height (and related essentials as needed).
- **Speeds:** key print speed controls (general and/or common stage speeds).
- **Supports:** enable/disable and basic support type/options.
- **Quality/structure:** walls/perimeters, top/bottom layers/shells.
- **Seams & extrusion behavior:** seam settings, retraction.
- **Cooling & flow:** fan/cooling, flow/extrusion multipliers.
- **Adhesion:** brim/skirt/raft basics.
- **Motion limits:** acceleration/jerk (key values).

**Pass-through/advanced overrides:** allow setting additional OrcaSlicer keys by name (string/number/bool as appropriate) for controls not explicitly modeled, with validation/warnings for unknown keys where feasible.

**[Placeholder]:** authoritative key names, types, units, allowed values/enums, and precedence/merge rules per target OrcaSlicer version; implement a mapping layer so tool schemas stay stable where possible.

## 8. Mesh preparation (v1)

Implemented in `geometry/` with CPU-bound work bounded by configured concurrency (process slots), operating on workspace artifacts:

- **`orca_mesh_inspect`**: report dimensions/bounds, triangle/vertex counts, and format. Implemented for STL (ASCII + binary) and Wavefront OBJ (ASCII); OBJ polygons are fan-triangulated.
- **`orca_mesh_transform`**: produce a new mesh artifact applying translate/rotate/scale (uniform or per-axis) and/or mirror to an STL or OBJ source; preserves provenance metadata. Output is always binary STL.
- **`orca_mesh_merge`**: combine multiple STL/OBJ mesh artifacts into one derived binary STL artifact; triangles are concatenated in the given order with no automatic placement offsets.

Out of v1: boolean, offset, remesh, decimate (can be added later following the same artifact/registry patterns).

## 9. Slicing and job lifecycle

- **Job-based execution:** `orca_slice` validates inputs/essentials, resolves profiles/overrides, plans an engine invocation, and starts a **background job** (optionally wait with timeout, similar to `mcp-cura` ergonomics).
- **Job record:** persisted under the workspace (e.g. `jobs/<job_id>/job.json`) with state, timings, resolved config summary, engine command/log references (redacted as needed), errors, and output artifact ids on success.
- **Controls:** cancellation, status/get/list/wait tools; bounded concurrency; per-job and global timeouts.
- **Outputs:** generated G-code (and relevant logs/metadata) imported as artifacts.
- **G-code handling (v1 baseline):** store and make inspectable as text/artifact metadata where cheap; deeper verify/prepare/post-process tools can be staged later (consistent with safety posture) but are not required for initial slice delivery.

## 10. Workspace and artifacts

Content-addressed store (mcp-cura-like):
- **Blobs:** stored by content hash (e.g. `blobs/<sha256>.<ext>`).
- **Metadata:** artifact records (id, kind, name, hashes/size, provenance, timestamps).
- **Jobs:** per-job working dirs and records.
- **Profiles:** workspace-managed custom profiles/overrides.
- **Temp:** ephemeral working files.
- **GC/retention:** configurable pruning of unreferenced/old artifacts and job data per policy.

Artifacts provide stable ids for tool composition (import model → transform/merge → slice → retrieve G-code).

## 11. Safety, policy, approvals, and audit

- **Single registry choke point:** every tool call passes through capability/risk/gate evaluation, approval handling (where required), execution, and auditing — no bypass path.
- **Capabilities (illustrative):** `read`, `workspace_write`, `process`, `export`, `destructive` (exact mapping per tool).
- **Approvals:** gated actions (e.g. exports, deletes, and any configured high-risk operations) require explicit confirmation/approval consistent with `mcp-cura`’s model; approvals are scoped/single-use/expiring as designed.
- **Boundaries enforced:** no printer comms, no G-code execution; refuse/flag dangerous or policy-violating requests per configured limits (e.g. resource/time/temperature sanity checks where applicable to inputs/overrides).
- **Redaction:** secrets/tokens and sensitive paths redacted from logs/audit/errors where appropriate.
- **Audit:** implemented as an append-only JSONL log at `<workspace>/audit.jsonl` (unix timestamp, tool name, outcome, capped sanitized detail). Every registry call records at least one entry; approval-gated and denied calls are recorded before the handler runs.
- **Implemented approval flow:** the first gated call persists a pending request (`approvals/<id>.json`) and returns an error carrying the request id; `orca_pending_approvals` lists unexpired requests; `orca_approval_decide` approves/denies; retrying the identical tool+args within `approval.ttl_secs` consumes the approval once.
- **Approval modes:** `in_band` (default) and `both` accept MCP-side decisions. `out_of_band` refuses `orca_approval_decide`; decisions are made on the host with `mcp-orca approvals approve|deny <id>` against the same approval store.

## 12. Proposed v1 tool catalog (`orca_*`)

Names/prefix and grouping follow the `mcp-cura` feel; final schemas/args are finalized during implementation.

### 12.1 Admin / introspection
- `orca_server_info`, `orca_engine_info`, `orca_policy_info`, `orca_doctor`, `orca_pending_approvals`, `orca_approval_decide`

### 12.2 Artifacts / workspace
- `orca_artifact_import`, `orca_artifact_list`, `orca_artifact_get`, `orca_artifact_read_text` (where applicable), `orca_artifact_export`, `orca_artifact_delete`

### 12.3 Mesh (v1)
- `orca_mesh_inspect`, `orca_mesh_transform`, `orca_mesh_merge`

### 12.4 Profiles
- `orca_profile_list`, `orca_profile_refresh`: bounded discovery (explicit paths + OS-detected defaults); the visited directories and warnings are reported.
- `orca_profile_get`: one discovered entry by id/path/name. Workspace custom-profile artifacts (`art_*`) are also resolved, returning their stored settings.
- `orca_profile_resolve`: merges settings from workspace custom-profile artifacts for printer/filament/process, then applies overrides; native presets are resolved by reference for the engine adapter. Essentials (printer+filament+process) are required unless a project is supplied.
- `orca_profile_create` / `orca_profile_update`: workspace-managed custom profiles as content-addressed JSON artifacts. External/user/system Orca installs are never mutated; updates always create a new version.

### 12.5 Slicing / jobs
- `orca_slice` (start background job; optional wait)
- `orca_slice_status`, `orca_slice_wait`, `orca_slice_cancel`, `orca_slice_list`

### 12.6 G-code (minimal v1 / extensible)
- `orca_gcode_inspect` reports line/command/comment counts, layer markers, nozzle/bed temperature maxima, filament usage, extrude/travel moves, move bounds, and header time estimates. It never executes G-code.

## 13. CLI, transport, and DX

- **Binary:** `mcp-orca`
- **Default behavior:** running with no subcommand starts the **stdio** MCP server (stdout reserved for protocol/messages; diagnostics to stderr).
- **Subcommands (implemented):** `serve` (stdio/http), `doctor`, `tools` (list), `print-config`, `init`, and `approvals list|approve|deny` for out-of-band decisions.
- **HTTP (optional):** token-gated when not loopback (unless explicitly overridden), with health/transport endpoints documented.
- **Docs/examples:** README, configuration reference, safety model, engine/profile setup (host + Docker with mount examples), and client config snippets (e.g. Claude Desktop/OpenCode style) mirroring `mcp-cura`’s documentation ergonomics.

## 14. Observability and diagnostics

- Structured logging/tracing to stderr (server-safe).
- Job-scoped engine stdout/stderr capture retained with job records (subject to redaction/size limits).
- `doctor` checks: config validity, workspace writability, engine runner reachability/version probe (best-effort, adapter-dependent), profile source resolution (counts/paths/warnings), and policy/approval store health.

## 15. Implementation milestones (suggested)

1. **Scaffold:** Cargo project, CLI skeleton, config/load/print/init, logging, error types, basic `doctor`/`server_info`.
2. **Workspace & artifacts:** content-addressed store, import/get/list/export/delete, GC basics.
3. **Safety & registry:** policy/approvals/audit and single tool execution choke point.
4. **Engine adapter:** host + Docker runner interfaces, generic command planning, timeouts/cancellation, job records.
5. **Profiles:** OS-detected + explicit-path discovery/index, get/list/refresh, resolve/merge model, workspace custom profile create/update with backup invariant.
6. **Mesh:** inspect/transform/merge tools.
7. **Slicing:** `orca_slice` + job lifecycle tools, essential-presets enforcement, artifact outputs.
8. **Docs & hardening:** examples (host/docker), config reference, safety docs, integration tests/smoke flows against a wired OrcaSlicer CLI.

## 16. Open items to finalize at integration time

- **OrcaSlicer CLI contract:** binary/command name, version targets, exact slice arguments, preset selection/loading mechanism, override injection method, output/log conventions (host and Docker images).
- **Profile formats & semantics:** definitive file types, schema/keys, inheritance/merge behavior for printer/filament/process in the target OrcaSlicer version.
- **Default path validation:** confirm/adjust OS-specific common profile/resource directories per real installations.
- **Setting key mapping:** authoritative key catalog, types/ranges/enums, and mapping table for v1 “broad controls” + pass-through behavior.
- **G-code tooling depth:** whether to include `inspect/verify/prepare` in initial implementation milestone or defer (baseline requires artifact storage/retrieval).

---
*This spec reflects the confirmed grilling/design decisions (Q1–Q3/A,A,A) and should be treated as the source of truth for v1 scaffolding and implementation planning, updated as placeholders are wired to a concrete OrcaSlicer target.* 
