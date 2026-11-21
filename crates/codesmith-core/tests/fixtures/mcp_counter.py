"""A stdio MCP server for tests that must count tool calls: `post` appends
its `text` to the file named by the first argument, one line a call, and
answers with how many lines there are; `wait` answers once the file named
by its `until` argument exists. It exits when stdin closes."""
import json, os, sys, time

LOG = sys.argv[1]


def send(m):
    sys.stdout.write(json.dumps(m) + "\n")
    sys.stdout.flush()


TOOLS = [
    {"name": "post", "description": "Record a post", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
    {"name": "wait", "description": "Wait for a file", "inputSchema": {"type": "object", "properties": {"until": {"type": "string"}}, "required": ["until"]}},
]

while True:
    line = sys.stdin.readline()
    if not line:
        break
    m = json.loads(line)
    method, mid = m.get("method"), m.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": m["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "counter", "version": "1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
    elif method == "tools/call":
        name = m["params"]["name"]
        args = m["params"].get("arguments") or {}
        if name == "post":
            with open(LOG, "a") as f:
                f.write(args.get("text", "") + "\n")
            with open(LOG) as f:
                n = len(f.readlines())
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "posted %d" % n}]}})
        elif name == "wait":
            # Bounded, so a forgotten waiter cannot outlive its test by much.
            deadline = time.time() + 60
            while not os.path.exists(args["until"]) and time.time() < deadline:
                if sys.stdin.closed:
                    break
                time.sleep(0.02)
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "waited"}]}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown tool"}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown"}})
