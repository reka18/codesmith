"""A minimal language server for tests. Each line of an opened or changed
document containing ERROR becomes an error diagnostic (WARN a warning). It
first publishes an empty list, then the real one a moment later, so the
client's debounce is exercised; every push also reports an error in a
second file, `other.fake`. It asks the client for its configuration after
`initialized` and answers definition requests with the first ERROR line.
With `"watch": "<glob>"` in its options it registers a file watcher for
that glob, and `fake/watched` returns the didChangeWatchedFiles changes it
got."""
import json, os, sys, threading, time

out = sys.stdout.buffer
lock = threading.Lock()
docs = {}
config = {}
watched = []


def send(m):
    body = json.dumps(m).encode()
    with lock:
        out.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        out.flush()


def read():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            break
        name, _, value = line.partition(b":")
        if name.lower() == b"content-length":
            length = int(value)
    return json.loads(sys.stdin.buffer.read(length))


def diagnostics(text):
    out = []
    for i, line in enumerate(text.split("\n")):
        for word, sev in (("ERROR", 1), ("WARN", 2)):
            col = line.find(word)
            if col >= 0:
                out.append({
                    "range": {"start": {"line": i, "character": col},
                              "end": {"line": i, "character": col + len(word)}},
                    "severity": sev,
                    "source": "fake",
                    "message": "%s found (%s)" % (word.lower(), config.get("flavor", "plain")),
                })
    return out


def publish(uri, text):
    send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
          "params": {"uri": uri, "diagnostics": []}})
    time.sleep(0.05)
    send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
          "params": {"uri": uri, "diagnostics": diagnostics(text)}})
    other = uri.rsplit("/", 1)[0] + "/other.fake"
    if other != uri:
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
              "params": {"uri": other, "diagnostics": diagnostics("ERROR elsewhere")}})


while True:
    m = read()
    if m is None:
        break
    method, mid = m.get("method"), m.get("id")
    if method == "initialize":
        config = m["params"].get("initializationOptions") or {}
        assert m["params"]["rootUri"].startswith("file://"), m
        send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": {"textDocumentSync": 1, "definitionProvider": True}}})
    elif method == "initialized":
        send({"jsonrpc": "2.0", "id": "cfg-1", "method": "workspace/configuration",
              "params": {"items": [{"section": "fake"}]}})
        if config.get("watch"):
            send({"jsonrpc": "2.0", "id": "reg-1", "method": "client/registerCapability",
                  "params": {"registrations": [{"id": "w1", "method": "workspace/didChangeWatchedFiles",
                             "registerOptions": {"watchers": [{"globPattern": config["watch"]}]}}]}})
    elif method == "workspace/didChangeWatchedFiles":
        watched.extend(m["params"]["changes"])
    elif method == "fake/watched":
        send({"jsonrpc": "2.0", "id": mid, "result": watched})
    elif method is None and mid == "reg-1":
        pass
    elif method is None and mid == "cfg-1":
        config = (m.get("result") or [{}])[0] or config
    elif method == "textDocument/didOpen":
        d = m["params"]["textDocument"]
        docs[d["uri"]] = d["text"]
        publish(d["uri"], d["text"])
    elif method == "textDocument/didChange":
        uri = m["params"]["textDocument"]["uri"]
        docs[uri] = m["params"]["contentChanges"][-1]["text"]
        publish(uri, docs[uri])
    elif method == "textDocument/definition":
        uri = m["params"]["textDocument"]["uri"]
        lines = docs.get(uri, "").split("\n")
        hits = [i for i, l in enumerate(lines) if "ERROR" in l]
        result = [{"uri": uri, "range": {"start": {"line": hits[0], "character": 0},
                                         "end": {"line": hits[0], "character": 1}}}] if hits else []
        send({"jsonrpc": "2.0", "id": mid, "result": result})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": mid, "result": None})
    elif method == "exit":
        break
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "not found"}})
