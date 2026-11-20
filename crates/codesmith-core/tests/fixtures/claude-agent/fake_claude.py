#!/usr/bin/env python3
"""A fake `claude` for tests: it relays its stdin and stdout over the Unix
socket in $FAKE_CLAUDE_SOCKET to the test, which plays Claude Code's side
of the stream-json protocol from recordings. It first sends its arguments,
environment, folder and pid; a line "\\0EXIT <code>" from the test ends it
with that exit code."""
import json, os, socket, sys, threading

sock = socket.socket(socket.AF_UNIX)
sock.connect(os.environ["FAKE_CLAUDE_SOCKET"])
hello = {"args": sys.argv[1:], "env": dict(os.environ), "cwd": os.getcwd(), "pid": os.getpid()}
sock.sendall((json.dumps(hello) + "\n").encode())


def relay_input():
    while True:
        try:
            data = os.read(0, 65536)
        except OSError:
            data = b""
        if not data:
            break
        sock.sendall(data)
    try:
        sock.shutdown(socket.SHUT_WR)
    except OSError:
        pass


threading.Thread(target=relay_input, daemon=True).start()
code = 0
out = sys.stdout.buffer
for line in sock.makefile("rb"):
    if line.startswith(b"\0EXIT "):
        code = int(line[6:].strip() or b"0")
        break
    try:
        out.write(line)
        out.flush()
    except BrokenPipeError:
        break
os._exit(code)
