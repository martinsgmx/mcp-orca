# Scripts

- `smoke.sh` — local smoke checks: `cargo test`, `cargo clippy --all-targets`, tool catalog, `doctor`, and an MCP stdio handshake.
- `stdio_smoke.py` — minimal initialize + tools/list handshake against `target/debug/mcp-orca` (override with `MCP_ORCA_BIN`).

Engine wiring is configuration-driven (`engine.slice_command` / `engine.docker_command`); see the main README for host and Docker examples.
