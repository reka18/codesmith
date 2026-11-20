"""A stdio MCP server for the tool input repair tests: `count` takes a
closed object (an integer `count`, an optional boolean `detail`) and says
what arguments it got."""
import json, sys

TOOL = {"name": "count", "description": "Record a count", "inputSchema": {
    "type": "object",
    "properties": {"count": {"type": "integer"}, "detail": {"type": "boolean"}},
    "required": ["count"], "additionalProperties": False}}

def send(m):
    sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()

for line in sys.stdin:
    m = json.loads(line)
    method, mid = m.get("method"), m.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": m["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "fix", "version": "1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [TOOL]}})
    elif method == "tools/call":
        args = m["params"].get("arguments")
        send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "got: " + json.dumps(args, sort_keys=True)}]}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown"}})
