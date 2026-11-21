"""A stdio MCP server for framing tests. Its initialize reply comes after
a blank line, a whitespace line and a line that is not JSON, ends in CRLF,
and shares its write with a notification. Its one tool, `frame`, answers
with a reply line of `size` bytes (without the newline) written in
`chunk`-byte pieces, or, given `text`, with that text written one byte at
a time (splitting UTF-8). It stays up, reading stdin, after any reply, and
exits when stdin closes."""
import json, sys

out = sys.stdout.buffer


def write(data, chunk=None):
    if chunk:
        for i in range(0, len(data), chunk):
            out.write(data[i:i + chunk])
            out.flush()
    else:
        out.write(data)
        out.flush()


for line in sys.stdin:
    m = json.loads(line)
    method, mid, params = m.get("method"), m.get("id"), m.get("params") or {}
    if method == "initialize":
        reply = json.dumps({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": params["protocolVersion"], "serverInfo": {"name": "frames", "version": "1"}, "capabilities": {"tools": {}}}})
        note = json.dumps({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "hi"}})
        write(("\n  \r\nnot json\n" + note + "\n" + reply + "\r\n").encode())
    elif method == "tools/list":
        write((json.dumps({"jsonrpc": "2.0", "id": mid, "result": {"tools": [{"name": "frame", "inputSchema": {"type": "object"}}]}}) + "\n").encode())
    elif method == "tools/call":
        a = params.get("arguments") or {}
        if "text" in a:
            body = json.dumps({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": a["text"]}]}}, ensure_ascii=False).encode() + b"\n"
            write(body, 1)
            continue
        prefix = json.dumps({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": ""}]}}).encode()
        # Pad the text so the line is `size` bytes.
        cut = prefix.index(b'"text": "') + len(b'"text": "')
        pad = a["size"] - len(prefix)
        body = prefix[:cut] + b"x" * pad + prefix[cut:] + b"\n"
        write(body, a.get("chunk", 65536))
    elif mid is not None:
        write((json.dumps({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown"}}) + "\n").encode())
