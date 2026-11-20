#!/usr/bin/env python3
"""Rebuild the models.dev catalog snapshot bundled with CodeSmith.

    scripts/models-snapshot.py catalog.json    # a saved catalog
    scripts/models-snapshot.py                 # fetch https://models.dev/api.json

Reads a models.dev catalog: a saved v2 copy (the committed snapshot comes
from OpenCode's `packages/core/src/models-dev/snapshot.txt`, which carries
more than the public v1 API: per-API packages, MIME modalities, modes), or
the v1 `/api.json` of models.dev or of CODESMITH_MODELS_URL. A v2 catalog is
trimmed to the fields `crates/codesmith-core/src/catalog` reads; a v1 one is
kept whole. Writes it gzipped to
`crates/codesmith-core/src/catalog/snapshot.json.gz`.
"""

import gzip
import json
import os
import sys
import urllib.request

OUT = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    "..",
    "crates",
    "codesmith-core",
    "src",
    "catalog",
    "snapshot.json.gz",
)
# A floor, not equality: guards against an error page or a truncated body.
MINIMUM_PROVIDERS = 100


def pick(d, keys):
    return {k: d[k] for k in keys if k in d}


def api(a):
    if not isinstance(a, dict):
        return a
    out = pick(a, ("base_url", "sdk", "compatibility"))
    caps = a.get("capabilities")
    if isinstance(caps, dict) and isinstance(caps.get("tools"), dict):
        if "freeform" in caps["tools"]:
            out["capabilities"] = {"tools": {"freeform": caps["tools"]["freeform"]}}
    return out


def cost(c):
    return pick(c, ("text", "multipliers")) if isinstance(c, dict) else c


def mode(m):
    if not isinstance(m, dict):
        return m
    out = pick(m, ("service_tier", "body", "headers"))
    if "cost" in m:
        out["cost"] = cost(m["cost"])
    return out


def model(m):
    if not isinstance(m, dict):
        return m
    out = pick(
        m, ("id", "type", "name", "family", "release_date", "status", "modalities", "limit")
    )
    caps = m.get("capabilities")
    if isinstance(caps, dict):
        out["capabilities"] = pick(caps, ("tools", "reasoning", "temperature"))
        tools = caps.get("tools")
        if isinstance(tools, dict):
            out["capabilities"]["tools"] = pick(tools, ("supported",))
    elif caps is not None:
        out["capabilities"] = caps
    if "cost" in m:
        out["cost"] = cost(m["cost"])
    if isinstance(m.get("api"), dict):
        out["api"] = {k: api(v) for k, v in m["api"].items()}
    elif "api" in m:
        out["api"] = m["api"]
    ex = m.get("experimental")
    if isinstance(ex, dict) and isinstance(ex.get("modes"), dict):
        out["experimental"] = {"modes": {k: mode(v) for k, v in ex["modes"].items()}}
    return out


def main():
    if len(sys.argv) > 1:
        with open(sys.argv[1], encoding="utf-8") as f:
            text = f.read()
    else:
        source = os.environ.get("CODESMITH_MODELS_URL") or "https://models.dev"
        req = urllib.request.Request(source + "/api.json", headers={"User-Agent": "codesmith"})
        with urllib.request.urlopen(req, timeout=30) as r:
            text = r.read().decode("utf-8")
    catalog = json.loads(text)
    v1 = any(isinstance(p, dict) and "npm" in p for p in catalog.values())
    trimmed = {}
    for key, p in catalog.items():
        if v1:
            trimmed[key] = p
            continue
        q = pick(p, ("id", "name", "env"))
        q["models"] = {k: model(v) for k, v in p.get("models", {}).items()}
        trimmed[key] = q
    if len(trimmed) < MINIMUM_PROVIDERS:
        sys.exit(f"catalog has {len(trimmed)} providers, expected at least {MINIMUM_PROVIDERS}")
    body = json.dumps(trimmed, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    with open(OUT, "wb") as f:
        # mtime 0 keeps the file byte-identical for an unchanged catalog.
        f.write(gzip.compress(body, compresslevel=9, mtime=0))
    print(f"{len(trimmed)} providers, {len(body)} bytes ({os.path.getsize(OUT)} gzipped)")


if __name__ == "__main__":
    main()
