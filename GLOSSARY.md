# Glossary

Domain language for **mcp-orca** (v1 design). This glossary defines terms only; it does not prescribe implementation details.

## Core

- **mcp-orca**: A Model Context Protocol (MCP) server that enables agents to prepare models and generate G-code using OrcaSlicer in a controlled, inspectable way. It does not communicate with printers and does not execute G-code.
- **OrcaSlicer**: The external slicer application/CLI used by `mcp-orca` to produce G-code from 3D models and print settings.
- **Engine runner**: The configured mechanism used to invoke OrcaSlicer. v1 supports **host** (local CLI command/binary) and **Docker** (containerized CLI) runners.
- **Slice**: The operation of producing G-code from one or more input models using resolved presets/profiles and setting overrides.
- **Slice job**: A tracked unit of slicing work (queued/running/completed/failed/cancelled) with associated inputs, configuration, logs, timing, and outputs.

## Inputs, outputs, and workspace

- **Model**: A 3D input asset to be sliced (e.g. mesh file). In v1, mesh preparation focuses on inspection, transformation, and merging prior to slicing.
- **G-code**: The toolpath/output artifact produced by a slice.
- **Artifact**: A content-addressed workspace object (e.g. imported model, derived mesh, generated G-code, logs/metadata) that can be referenced by a stable identifier.
- **Workspace**: The server-managed storage area for artifacts, job records, custom profiles, and temporary files, with configurable retention/cleanup.

## Profiles and settings

- **Preset / Profile**: A named bundle of slicer settings. OrcaSlicer commonly organizes presets as **printer**, **filament**, and **process** presets (terminology to be aligned with the target OrcaSlicer version/CLI during integration).
- **Printer preset**: Settings describing the target machine/configuration context for slicing (as defined by OrcaSlicer).
- **Filament preset**: Settings describing material-related behavior (as defined by OrcaSlicer).
- **Process preset**: Settings describing print/process behavior such as layers, infill, speeds, supports, etc. (as defined by OrcaSlicer).
- **Essential presets**: The minimum resolved preset set required to slice in v1: **printer + filament + process**. If not fully resolved, slicing fails with a clear error (no invented defaults).
- **Profile source**: A location or input providing presets (e.g. configured search paths, explicit file paths, or a project file).
- **Profile discovery/index**: Scanning bounded, configured/common OS-specific locations (only if present) and explicit paths to enumerate available presets for listing/selection; not a whole-system crawl.
- **Effective settings / resolved settings**: The merged view of settings resulting from selected presets/profiles and applied overrides for a given slice or inspection request.
- **Setting override**: A caller-specified value that changes a particular slicer setting for a slice or when creating/updating a custom profile (including pass-through/advanced keys where supported).
- **Custom profile**: A server-managed profile/override artifact stored in the workspace, created or updated by `mcp-orca` without mutating user/system OrcaSlicer installations in place.
- **Backup (profile safety)**: A timestamped copy created before any in-place modification of an existing profile file, used as a precaution.

## Mesh preparation (v1)

- **Mesh inspect**: Reading/reporting properties of a mesh/model relevant to preparation and slicing (e.g. bounds/size, integrity signals as supported).
- **Mesh transform**: Applying geometric transformations to a mesh such as scale (uniform/non-uniform), rotate, translate, and mirror.
- **Mesh merge**: Combining multiple meshes/models into a single mesh artifact for slicing/preparation.

## Controls (v1 focus)

- **Infill**: Internal structure settings for printed parts (e.g. pattern/type and density) as represented by OrcaSlicer settings.
- **Temperature settings**: Thermal settings relevant to printing (e.g. nozzle, bed, chamber as applicable) as represented by OrcaSlicer settings.
- **Key process controls**: High-value slicing parameters such as layer height, speeds, supports, walls/perimeters, top/bottom, seams, retraction, cooling/fan, flow/extrusion, brim/skirt/raft, and acceleration/jerk (subject to mapping to the target OrcaSlicer setting keys).

## Safety and operations

- **Policy**: Local rules that classify and constrain tool actions (capabilities, risk, limits) before execution.
- **Approval**: A gated confirmation step required for certain actions per policy (e.g. exports/deletions or other configured operations), recorded and auditable.
- **Audit log**: A record of tool actions, decisions, and outcomes for traceability.
- **Doctor**: A diagnostic command/report that checks configuration, runner availability, profile sources, workspace health, and tool readiness.
