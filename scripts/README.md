# Scripts

- `smoke.sh` — local smoke checks: `cargo test`, `cargo clippy --all-targets`, tool catalog, `doctor`, and an MCP stdio handshake.
- `stdio_smoke.py` — minimal initialize + tools/list handshake against `target/debug/mcp-orca` (override with `MCP_ORCA_BIN`).
- `http_smoke.sh` — starts the built server on a loopback port with a static bearer token and verifies 401 without the token and 200 with it (override port/token with `MCP_ORCA_HTTP_PORT` / `MCP_ORCA_HTTP_TOKEN`).

Engine wiring is configuration-driven (`engine.slice_command` / `engine.docker_command`); see the main README for host and Docker examples.
