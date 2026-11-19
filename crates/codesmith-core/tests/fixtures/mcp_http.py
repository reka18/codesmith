"""A minimal streamable-HTTP MCP server for tests: answers with SSE and
requires the session id it hands out."""
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        method, mid = body.get("method"), body.get("id")
        if method != "initialize" and self.headers.get("Mcp-Session-Id") != "sess-42":
            self.send_response(400); self.end_headers(); return
        if mid is None:
            self.send_response(202); self.end_headers(); return
        if method == "initialize":
            result = {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "h", "version": "1"}}
        elif method == "tools/list":
            result = {"tools": [{"name": "add", "description": "Add numbers", "inputSchema": {"type": "object"}}]}
        else:
            a = body["params"]["arguments"]
            result = {"content": [{"type": "text", "text": str(a["a"] + a["b"])}]}
        data = "event: message\ndata: " + json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Mcp-Session-Id", "sess-42")
        self.end_headers()
        self.wfile.write(data.encode())
    def log_message(self, *a):
        pass

srv = HTTPServer(("127.0.0.1", 0), H)
print(srv.server_address[1], flush=True)
srv.serve_forever()
