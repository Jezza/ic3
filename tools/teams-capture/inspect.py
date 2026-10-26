#!/usr/bin/env python3
"""Summarise a `record.mjs` capture without printing its contents.

    python3 -I tools/teams-capture/inspect.py mark LABEL [FILE]
    python3 -I tools/teams-capture/inspect.py endpoints [FILE] [--since-mark]
    python3 -I tools/teams-capture/inspect.py shape URL_REGEX [FILE] [--req] [--depth N]

FILE defaults to the newest `captures/live-*/events.jsonl`.

- `mark` appends a timestamped marker, so later output can be limited to what
  happened after an action in Teams.
- `endpoints` lists the requests as method, status counts, response bytes and a
  URL template (conversation, user and numeric ids replaced, query reduced to
  its parameter names).
- `shape` prints the structure of matching response bodies (or, with `--req`,
  request bodies): field names and value types, with strings reduced to their
  length and ids to their prefix. Enum-like fields (`messagetype`, `type`, …)
  keep their values.

The output is meant to be safe to paste into docs: no message text, names or
ids. Still read it before sharing.
"""

import glob
import json
import os
import re
import sys
import time
from collections import Counter, OrderedDict
from urllib.parse import parse_qsl, unquote, urlsplit

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ENUM_KEYS = {
    "messagetype", "messageType", "contenttype", "type", "resourceType", "folderType",
    "sortType", "chatType", "channelType", "activityType", "activitySubtype",
    "sourceThreadType", "threadType", "productThreadType", "action",
}
ID = re.compile(r"^(19|8|28|48):")


def newest_capture():
    files = sorted(glob.glob(os.path.join(REPO, "captures", "live-*", "events.jsonl")))
    if not files:
        sys.exit("no captures/live-*/events.jsonl found")
    return files[-1]


def events(path):
    with open(path) as f:
        for line in f:
            yield json.loads(line)


def last_mark(path):
    marks = [e["t"] for e in events(path) if e["kind"] == "mark"]
    return marks[-1] if marks else 0


def template(url):
    s = urlsplit(url)
    p = unquote(s.path)
    p = re.sub(r"(19|48|8|28):[^/?;]+", lambda m: "{%s}" % m.group(1), p)
    p = re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", "{guid}", p)
    p = re.sub(r"/\d{6,}", "/{n}", p)
    p = re.sub(r"/personal/[^/]+", "/personal/{user}", p)
    p = re.sub(r"[^/@']+@[^/']+", "{email}", p)
    q = "&".join(sorted({k for k, _ in parse_qsl(s.query, keep_blank_values=True)}))
    return s.netloc + p + (f"  ?{q}" if q else "")


def cmd_endpoints(path, since):
    reqs, agg = {}, OrderedDict()
    for e in events(path):
        if e["t"] < since:
            continue
        if e["kind"] == "req" and e["id"] not in reqs:
            reqs[e["id"]] = key = (e["method"], template(e["url"]))
            agg.setdefault(key, {"n": 0, "status": Counter(), "bytes": 0})["n"] += 1
        elif e["kind"] == "res" and e["id"] in reqs:
            agg[reqs[e["id"]]]["status"][e["status"]] += 1
        elif e["kind"] == "body" and e["id"] in reqs:
            agg[reqs[e["id"]]]["bytes"] += len(e["body"])
    for (method, url), a in agg.items():
        print(f'{a["n"]:4} {method:7} {str(dict(a["status"])):14} {a["bytes"]:>9}  {url}')


def shape(v, key="", depth=0, max_depth=6):
    if isinstance(v, str) and v[:1] in "{[":
        try:
            return {"<json-string>": shape(json.loads(v), key, depth, max_depth)}
        except ValueError:
            pass
    if depth >= max_depth:
        return type(v).__name__
    if isinstance(v, dict):
        if v and all(ID.match(k) or k.isdigit() or re.fullmatch(r"[0-9a-f-]{36}", k) for k in v):
            first = next(iter(v.values()))
            return {f"<{len(v)} id-keyed>": shape(first, key, depth + 1, max_depth)}
        return {k: shape(x, k, depth + 1, max_depth) for k, x in v.items()}
    if isinstance(v, list):
        if not v:
            return []
        merged = {}
        for x in v[:50]:
            if isinstance(x, dict):
                for k, y in x.items():
                    merged.setdefault(k, y)
        return [f"len={len(v)}", shape(merged or v[0], key, depth + 1, max_depth)]
    if isinstance(v, bool):
        return "bool"
    if isinstance(v, (int, float)):
        return "num"
    if v is None:
        return "null"
    if key in ENUM_KEYS and len(v) < 40 and not ID.match(v) and not re.search(r"[0-9a-f]{8}-", v):
        return v
    if ID.match(v):
        return "id:" + v[:3]
    if re.match(r"^\d{4}-\d\d-\d\dT", v):
        return "iso-time"
    return f"str({len(v)})"


def cmd_shape(path, pattern, use_req, max_depth):
    rx, seen = re.compile(pattern), set()
    kind = "req" if use_req else "body"
    for e in events(path):
        if e["kind"] != kind or not rx.search(e["url"]) or e["id"] in seen:
            continue
        seen.add(e["id"])
        if use_req:
            print(e["method"], template(e["url"]))
            body = e.get("body")
            if not body:
                print("----")
                continue
        else:
            body = e["body"]
        try:
            print(json.dumps(shape(json.loads(body), max_depth=max_depth), indent=1))
        except ValueError:
            print(f"(not JSON, {len(body)} chars)")
        print("----")


def main(argv):
    if not argv:
        sys.exit(__doc__)
    flags = {a for a in argv if a.startswith("--")}
    depth = 6
    if "--depth" in argv:
        depth = int(argv[argv.index("--depth") + 1])
        argv = argv[: argv.index("--depth")] + argv[argv.index("--depth") + 2 :]
    pos = [a for a in argv if not a.startswith("--")]
    cmd = pos[0]
    if cmd == "mark":
        path = pos[2] if len(pos) > 2 else newest_capture()
        with open(path, "a") as f:
            f.write(json.dumps({"t": int(time.time() * 1000), "kind": "mark", "label": pos[1]}) + "\n")
        print("marked", path)
    elif cmd == "endpoints":
        path = pos[1] if len(pos) > 1 else newest_capture()
        cmd_endpoints(path, last_mark(path) if "--since-mark" in flags else 0)
    elif cmd == "shape":
        path = pos[2] if len(pos) > 2 else newest_capture()
        cmd_shape(path, pos[1], "--req" in flags, depth)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])
