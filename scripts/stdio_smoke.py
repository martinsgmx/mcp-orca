"""Minimal MCP stdio handshake smoke test.

Runs the built server, performs initialize + tools/list, and prints the results.
"""

import json
import os
import subprocess
import sys

binary = os.environ.get("MCP_ORCA_BIN", "target/debug/mcp-orca")
p = subprocess.Popen(
    [binary],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
)
msgs = [
 {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"smoke","version":"0.1"}}},
 {"jsonrpc":"2.0","method":"notifications/initialized"},
 {"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}},
]
for m in msgs:
    p.stdin.write(json.dumps(m)+"\n"); p.stdin.flush()
out = []
for _ in range(2):
    line = p.stdout.readline()
    if not line:
        break
    out.append(line.strip())

if len(out) != 2:
    print(f"FAILED: expected 2 responses, got {len(out)}")
    p.terminate()
    print(p.stderr.read()[:2000])
    sys.exit(1)

server = None
tool_count = None
for line in out:
    d = json.loads(line)
    if d.get("id") == 1:
        server = d.get("result", {}).get("serverInfo", {}).get("name")
    if d.get("id") == 2:
        tool_count = len(d.get("result", {}).get("tools", []))

print(f"initialize: server={server}")
print(f"tools/list: {tool_count} tools")
p.terminate()
if server != "mcp-orca" or not tool_count:
    sys.exit(1)
