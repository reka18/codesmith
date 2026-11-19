"""A minimal stdio MCP server for tests: two pages of tools, an echo tool,
a failing tool, and a ping to the client in the middle of a call."""
import json, sys

def send(m):
    sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()

def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

while True:
    m = read()
    if m is None:
        break
    method, mid = m.get("method"), m.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": m["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}})
    elif method == "tools/list":
        if not m.get("params", {}).get("cursor"):
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [{"name": "echo", "description": "Echo text back", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}], "nextCursor": "p2"}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [{"name": "fail.now", "description": "Always fails"}]}})
    elif method == "tools/call":
        send({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"})
        pong = read()
        assert pong and pong.get("id") == "srv-1" and "result" in pong, pong
        name = m["params"]["name"]
        if name == "echo":
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "echo: " + m["params"]["arguments"]["text"]}]}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "boom"}], "isError": True}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown"}})
