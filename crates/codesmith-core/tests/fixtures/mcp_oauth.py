"""An MCP server that is also its OAuth authorization server, for tests."""
import base64, hashlib, json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse, parse_qs, urlencode

codes, valid = {}, set()
class H(BaseHTTPRequestHandler):
    def base(self): return f"http://127.0.0.1:{self.server.server_address[1]}"
    def json(self, code, body, headers=()):
        b = json.dumps(body).encode(); self.send_response(code)
        self.send_header("Content-Type", "application/json")
        for k, v in headers: self.send_header(k, v)
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        u = urlparse(self.path)
        if u.path == "/.well-known/oauth-protected-resource/mcp":
            return self.json(200, {"resource": self.base() + "/mcp", "authorization_servers": [self.base() + "/auth"], "scopes_supported": ["tools"]})
        if u.path == "/.well-known/oauth-authorization-server/auth":
            b = self.base() + "/auth"
            return self.json(200, {"issuer": b, "authorization_endpoint": b + "/authorize", "token_endpoint": b + "/token", "registration_endpoint": b + "/register", "code_challenge_methods_supported": ["S256"]})
        if u.path == "/auth/authorize":
            q = {k: v[0] for k, v in parse_qs(u.query).items()}
            assert q["client_id"] == "cid-1" and q["code_challenge_method"] == "S256" and q["resource"] == self.base() + "/mcp" and q["scope"] == "tools", q
            codes["code-1"] = q["code_challenge"]
            self.send_response(302); self.send_header("Location", q["redirect_uri"] + "?" + urlencode({"code": "code-1", "state": q["state"]})); self.end_headers(); return
        self.json(404, {})
    def do_POST(self):
        u = urlparse(self.path); raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        if u.path == "/auth/register":
            assert json.loads(raw)["token_endpoint_auth_method"] == "none"
            return self.json(201, {"client_id": "cid-1"})
        if u.path == "/auth/token":
            f = {k: v[0] for k, v in parse_qs(raw.decode()).items()}
            assert f["resource"] == self.base() + "/mcp", f
            if f["grant_type"] == "authorization_code":
                want = base64.urlsafe_b64encode(hashlib.sha256(f["code_verifier"].encode()).digest()).rstrip(b"=").decode()
                if codes.get(f["code"]) != want: return self.json(400, {"error": "invalid_grant"})
                valid.add("at-1"); return self.json(200, {"access_token": "at-1", "refresh_token": "rt-1", "expires_in": 1, "token_type": "Bearer"})
            if f["grant_type"] == "refresh_token" and f["refresh_token"] == "rt-1":
                valid.add("at-2"); return self.json(200, {"access_token": "at-2", "expires_in": 3600, "token_type": "Bearer"})
            return self.json(400, {"error": "invalid_grant"})
        if u.path == "/mcp":
            tok = (self.headers.get("Authorization") or "").removeprefix("Bearer ")
            if tok not in valid:
                return self.json(401, {"error": "unauthorized"}, [("WWW-Authenticate", f'Bearer resource_metadata="{self.base()}/.well-known/oauth-protected-resource/mcp"')])
            m = json.loads(raw); mid = m.get("id")
            if mid is None: self.send_response(202); self.end_headers(); return
            res = {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "o", "version": "1"}} if m["method"] == "initialize" else \
                  {"tools": [{"name": "whoami", "inputSchema": {"type": "object"}}]} if m["method"] == "tools/list" else \
                  {"content": [{"type": "text", "text": "token " + tok}]}
            return self.json(200, {"jsonrpc": "2.0", "id": mid, "result": res})
        self.json(404, {})
    def log_message(self, *a): pass

srv = HTTPServer(("127.0.0.1", 0), H); print(srv.server_address[1], flush=True)
# Serve until stdin closes: the test holds the other end of the pipe, so
# the server goes when the test does, however it ends (even killed).
import os, threading
threading.Thread(target=srv.serve_forever, daemon=True).start()
sys.stdin.read()
os._exit(0)
