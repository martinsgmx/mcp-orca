# OrcaSlicer CLI Docker image (`Dockerfile.orca-cli`)

This document describes how to build and use the **OrcaSlicer CLI-only** Docker image provided by `Dockerfile.orca-cli`.

## What this image contains

- **OrcaSlicer v2.5.1** built from source in a multi-stage Docker build (`linux/amd64`).
- A balanced slim **Ubuntu 24.04** runtime image with the packaged OrcaSlicer runtime (binary, resources/profiles, and required runtime libraries) needed for CLI slicing.
- The CLI executable available on `PATH` as **`orca-slicer`**.
- **CLI-focused runtime only:** no `xvfb`/virtual-display wrapper and no desktop/GUI integration. The container runs `orca-slicer` directly.
- **Scope:** OrcaSlicer CLI only. This image does **not** include the `mcp-orca` server binary or any `mcp-orca` configuration.

## Build

From the repository root:

```sh
docker build -f Dockerfile.orca-cli -t orca-slicer-cli:v2.5.1 .
```

### Optional build arguments

- `ORCASLICER_VERSION` (default: `v2.5.1`) — pinned OrcaSlicer git tag/branch to build.
- `ORCASLICER_REPO` (default: `https://github.com/OrcaSlicer/OrcaSlicer.git`) — source repository.
- `UBUNTU_VERSION` (default: `24.04`) — Ubuntu base version for builder/runtime stages.
- `BUILD_JOBS` (default: unset) — if set, passed to `build_linux.sh -j` to control parallel build jobs.

Example with explicit parallelism:

```sh
docker build \
  -f Dockerfile.orca-cli \
  --build-arg BUILD_JOBS=8 \
  -t orca-slicer-cli:v2.5.1 .
```

> Note: building OrcaSlicer (and its dependencies) from source is resource- and time-intensive. Ensure sufficient CPU, RAM, and disk space.

## Basic usage

The image entrypoint is `orca-slicer`, and the default command shows CLI help:

```sh
docker run --rm orca-slicer-cli:v2.5.1
# equivalent to: orca-slicer --help
```

Mount your input/output files into the container and use `/work` as the working directory (default `WORKDIR`):

```sh
mkdir -p ./work
# place model and settings files in ./work, e.g. model.3mf, process.json, printer.json, filament.json

docker run --rm \
  -v "$PWD/work:/work" \
  orca-slicer-cli:v2.5.1 \
  model.3mf \
  --load-settings "process.json;printer.json" \
  --load-filaments "filament.json" \
  --slice 0 \
  --outputdir /work/out
```

## Common CLI workflows

`orca-slicer` supports headless/CLI operation for loading, transforming, and slicing models. Run `--help` for the full option list supported by this build.

### 1) Slice a model/project with explicit settings

```sh
docker run --rm -v "$PWD/work:/work" orca-slicer-cli:v2.5.1 \
  input.3mf \
  --load-settings "process.json;printer.json" \
  --load-filaments "filament.json" \
  --slice 0 \
  --outputdir /work/out
```

- `--load-settings "process.json;printer.json"` loads process/machine settings (semicolon-separated).
- `--load-filaments "filament.json"` loads filament settings.
- `--slice 0` slices all plates (see OrcaSlicer CLI docs for plate selection behavior).
- `--outputdir` writes exported outputs to the given directory.

### 2) Slice an STL/STEP/3MF and export G-code

```sh
docker run --rm -v "$PWD/work:/work" orca-slicer-cli:v2.5.1 \
  model.stl \
  --load-settings "process.json;printer.json" \
  --load-filaments "filament.json" \
  --export-gcode \
  --outputdir /work/out
```

(Exact export flags and combinations depend on OrcaSlicer CLI options; use `--help` and the upstream CLI documentation for details.)

### 3) Override settings from the command line

Most regular print/filament/printer settings can be overridden via CLI flags by converting setting keys to hyphenated flag names (for example, `layer_height` -> `--layer-height`):

```sh
docker run --rm -v "$PWD/work:/work" orca-slicer-cli:v2.5.1 \
  model.3mf \
  --load-settings "process.json;printer.json" \
  --load-filaments "filament.json" \
  --layer-height 0.2 \
  --slice 0 \
  --outputdir /work/out
```

### 4) Use a separate data directory (optional)

```sh
docker run --rm -v "$PWD/work:/work" orca-slicer-cli:v2.5.1 \
  --datadir /work/orcaslicer-data \
  model.3mf \
  --load-settings "process.json;printer.json" \
  --load-filaments "filament.json" \
  --slice 0 \
  --outputdir /work/out
```

## File mounts and paths

- Use `-v /host/path:/work` to provide inputs and collect outputs.
- Relative paths in arguments are resolved from `/work` (the container working directory).
- Ensure referenced settings/profile files (e.g. `process.json`, `printer.json`, `filament.json`) are accessible inside the container at the paths you provide.

## Notes and limitations

- **CLI-only/direct execution:** the image does not include `xvfb` or desktop integration. `orca-slicer` is invoked directly.
- **Source-built, pinned version:** OrcaSlicer is compiled from the pinned `v2.5.1` tag for reproducibility.
- **Resources/profiles:** the packaged OrcaSlicer resources are included in the image; provide or load the printer/process/filament settings appropriate for your CLI workflow as needed.
- **Performance/build time:** source builds are heavy; runtime slicing performance depends on model/settings and allocated container resources.

## Upstream references

- OrcaSlicer repository: https://github.com/OrcaSlicer/OrcaSlicer
- OrcaSlicer CLI mode wiki: https://github.com/OrcaSlicer/OrcaSlicer/wiki/cli_mode
- CLI actions: https://github.com/OrcaSlicer/OrcaSlicer/wiki/cli_actions
- CLI misc/options: https://github.com/OrcaSlicer/OrcaSlicer/wiki/cli_misc
- CLI transform: https://github.com/OrcaSlicer/OrcaSlicer/wiki/cli_transform
