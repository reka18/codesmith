"""A stdio MCP server for the elicitation tests. Each tool sends the client
an `elicitation/create` request while its call runs and returns the
client's answer as JSON text:

- `empty`: a form with no fields
- `form`: a form asking for a name (required) and a confirmation
- `url`: a URL elicitation with an elicitationId; with `{"complete": true}`
  the server also announces it complete
- `modern`: a URL elicitation without an elicitationId
- `giveup`: a URL elicitation the server cancels after 0.2 s, failing the call
"""
import json, sys, time

def send(m):
    sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()

def wait_for(rid):
    for line in sys.stdin:
        m = json.loads(line)
        if m.get("id") == rid and "method" not in m:
            return m.get("result")
    sys.exit(0)

TOOLS = [{"name": n, "inputSchema": {"type": "object"}} for n in ["empty", "form", "url", "modern", "giveup"]]
URL = "https://example.com/authorize"

for line in sys.stdin:
    m = json.loads(line)
    method, mid = m.get("method"), m.get("id")
    if method == "initialize":
        caps = m["params"]["capabilities"]
        assert "elicitation" in caps, caps
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": m["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "elicit", "version": "1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
    elif method == "tools/call":
        name = m["params"]["name"]
        args = m["params"].get("arguments") or {}
        rid = "elicit-" + str(mid)
        if name == "empty":
            params = {"mode": "form", "message": "Confirm", "requestedSchema": {"type": "object", "properties": {}}}
        elif name == "form":
            params = {"mode": "form", "message": "Who are you?", "requestedSchema": {"type": "object", "properties": {
                "name": {"type": "string", "title": "Name"},
                "confirm": {"type": "boolean", "title": "boolean", "description": "Go ahead"}}, "required": ["name"]}}
        elif name in ("url", "giveup"):
            params = {"mode": "url", "message": "Authorize access", "url": URL, "elicitationId": "elicitation-test"}
        else:
            params = {"mode": "url", "message": "Authorize", "url": URL}
        send({"jsonrpc": "2.0", "id": rid, "method": "elicitation/create", "params": params})
        if name == "giveup":
            time.sleep(0.2)
            send({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": rid, "reason": "timed out"}})
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "gave up"}], "isError": True}})
            continue
        if args.get("complete"):
            time.sleep(0.1)
            send({"jsonrpc": "2.0", "method": "notifications/elicitation/complete", "params": {"elicitationId": "elicitation-test"}})
        result = wait_for(rid)
        send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": json.dumps(result, sort_keys=True)}], "structuredContent": result}})
    elif mid is not None and "method" in m:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown"}})
