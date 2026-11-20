"""A stdio MCP server for tests beyond tools: instructions, resources and
templates, prompts with arguments, list_changed notifications, logging, and
MCP Events (the draft io.modelcontextprotocol/events extension) with push
delivery, cursors and replay.

Event streams take `arguments.queue`: "reject" refuses the stream,
"truncate" reports that it skipped events, "plan" follows the plan file
`arguments.plan` (below), anything else streams two tickets (cursors c1,
c2) and then heartbeats. Resuming from cursor cN replays from event N on,
so the client must skip what already ran.

A plan is a JSON list, one entry per connection, in order: `{"events":
[[id, cursor, title], ...], "exit": true}` sends those events and then
exits (a broken stream); without `exit` the stream heartbeats. Each
stream's resume cursor is appended to `<plan>.log`, one JSON value a
line."""
import json, os, sys, threading, time

lock = threading.Lock()
grown = False
cancelled = set()
EVENTS = [("e1", "c1", {"title": "first"}), ("e2", "c2", {"title": "second"})]
SUB = "io.modelcontextprotocol/subscriptionId"


def send(m):
    with lock:
        sys.stdout.write(json.dumps(m) + "\n")
        sys.stdout.flush()


def reply(mid, result):
    send({"jsonrpc": "2.0", "id": mid, "result": result})


def error(mid, code, message):
    send({"jsonrpc": "2.0", "id": mid, "error": {"code": code, "message": message}})


def note(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})


def stream(mid, params):
    queue = (params.get("arguments") or {}).get("queue")
    if queue == "reject":
        return error(mid, -32024, "Forbidden: no access to that queue")
    meta = {SUB: mid}
    if queue == "truncate":
        note("notifications/events/active", {"cursor": "c9", "truncated": True, "_meta": meta})
    cursor = params.get("cursor")
    if queue == "plan":
        return planned(mid, params, meta, cursor)
    start = int(cursor[1:]) - 1 if cursor else 0
    last = cursor or "c0"
    note("notifications/events/active", {"cursor": last, "_meta": meta})
    for eid, cur, data in EVENTS[max(start, 0):]:
        note("notifications/events/event", {"eventId": eid, "name": "ticket.created", "timestamp": "2026-11-19T10:00:00Z", "data": data, "cursor": cur, "_meta": meta})
        last = cur
    note("notifications/events/error", {"error": {"code": -1, "message": "one upstream fetch failed"}, "_meta": meta})

    def beat():
        while mid not in cancelled:
            time.sleep(0.2)
            note("notifications/events/heartbeat", {"cursor": last, "_meta": meta})

    threading.Thread(target=beat, daemon=True).start()


def planned(mid, params, meta, cursor):
    plan = params["arguments"]["plan"]
    with open(plan + ".log", "a") as f:
        f.write(json.dumps(cursor) + "\n")
    with open(plan + ".log") as f:
        n = len(f.readlines()) - 1
    with open(plan) as f:
        steps = json.load(f)
    step = steps[n] if n < len(steps) else {"events": []}
    last = cursor or "c0"
    note("notifications/events/active", {"cursor": last, "_meta": meta})
    for eid, cur, title in step.get("events", []):
        note("notifications/events/event", {"eventId": eid, "name": "ticket.created", "timestamp": "2026-11-19T10:00:00Z", "data": {"title": title}, "cursor": cur, "_meta": meta})
        last = cur
    if step.get("exit"):
        time.sleep(0.2)
        os._exit(0)

    def beat():
        while mid not in cancelled:
            time.sleep(0.2)
            note("notifications/events/heartbeat", {"cursor": last, "_meta": meta})

    threading.Thread(target=beat, daemon=True).start()


TOOLS = [{"name": "grow", "description": "Add a tool", "inputSchema": {"type": "object"}}]
EXTRA = {"name": "extra", "description": "Appears after grow", "inputSchema": {"type": "object"}}

while True:
    line = sys.stdin.readline()
    if not line:
        break
    m = json.loads(line)
    method, mid, params = m.get("method"), m.get("id"), m.get("params") or {}
    if method == "initialize":
        reply(mid, {"protocolVersion": params["protocolVersion"], "serverInfo": {"name": "rich", "version": "1"},
                    "instructions": "Always cite ticket ids.\nPrefer memo resources.",
                    "capabilities": {"tools": {"listChanged": True}, "prompts": {"listChanged": True}, "resources": {}, "logging": {},
                                     "extensions": {"io.modelcontextprotocol/events": {}}}})
    elif method == "notifications/cancelled":
        cancelled.add(params.get("requestId"))
    elif mid is None:
        pass
    elif method == "tools/list":
        reply(mid, {"tools": TOOLS + ([EXTRA] if grown else [])})
    elif method == "tools/call":
        grown = True
        note("notifications/message", {"level": "info", "logger": "rich", "data": "growing"})
        note("notifications/tools/list_changed", {})
        note("notifications/prompts/list_changed", {})
        reply(mid, {"content": [{"type": "text", "text": "grown"}]})
    elif method == "resources/list":
        reply(mid, {"resources": [{"uri": "memo://one", "name": "one", "mimeType": "text/plain"},
                                  {"uri": "memo://logo", "name": "logo", "mimeType": "image/png"}]})
    elif method == "resources/templates/list":
        reply(mid, {"resourceTemplates": [{"uriTemplate": "memo://{id}", "name": "memo"}]})
    elif method == "resources/read":
        uri = params["uri"]
        if uri == "memo://one":
            reply(mid, {"contents": [{"uri": uri, "mimeType": "text/plain", "text": "the first memo"}]})
        elif uri == "memo://logo":
            reply(mid, {"contents": [{"uri": uri, "mimeType": "image/png", "blob": "aGVsbG8="}]})
        else:
            error(mid, -32002, "Resource not found")
    elif method == "prompts/list":
        prompts = [{"name": "review", "description": "Review a change", "arguments": [{"name": "target", "required": True}, {"name": "focus"}]}]
        if grown:
            prompts.append({"name": "later", "description": "Appears after grow"})
        reply(mid, {"prompts": prompts})
    elif method == "prompts/get":
        a = params.get("arguments") or {}
        reply(mid, {"messages": [
            {"role": "user", "content": {"type": "text", "text": "Review %s focusing on %s." % (a.get("target"), a.get("focus") or "everything")}},
            {"role": "user", "content": {"type": "image", "data": "x", "mimeType": "image/png"}},
        ]})
    elif method == "events/list":
        reply(mid, {"events": [
            {"name": "ticket.created", "description": "A new ticket", "delivery": ["push"],
             "inputSchema": {"type": "object", "properties": {"queue": {"type": "string"}, "plan": {"type": "string"}}, "required": ["queue"]},
             "payloadSchema": {"type": "object", "properties": {"title": {"type": "string"}}}},
            {"name": "polled", "delivery": ["poll"]},
        ]})
    elif method == "events/stream":
        stream(mid, params)
    else:
        error(mid, -32601, "unknown")
