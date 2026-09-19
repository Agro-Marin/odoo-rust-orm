#!/usr/bin/env python3

import argparse
import http.cookiejar
import json
import pathlib
import sys
import urllib.error
import urllib.request


def make_opener(port, db, login, password):
    jar = http.cookiejar.CookieJar()
    opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "method": "call",
            "params": {"db": db, "login": login, "password": password},
        }
    ).encode()
    req = urllib.request.Request(
        "http://127.0.0.1:%d/web/session/authenticate" % port,
        data=body,
        headers={"content-type": "application/json"},
    )
    with opener.open(req, timeout=60) as r:
        payload = json.loads(r.read().decode())
    if payload.get("error") or not payload.get("result", {}).get("uid"):
        raise SystemExit("authentication failed: %.200s" % payload)
    return opener


def call(opener, port, case, timeout):
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "method": "call",
            "params": {
                "model": case["model"],
                "method": case["method"],
                "args": case.get("args") or [],
                "kwargs": case.get("kwargs") or {},
            },
        }
    ).encode()
    req = urllib.request.Request(
        "http://127.0.0.1:%d/web/dataset/call_kw" % port,
        data=body,
        headers={"content-type": "application/json"},
    )
    try:
        with opener.open(req, timeout=timeout) as r:
            return r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        return "HTTP %d %s" % (exc.code, exc.read().decode("utf-8", "replace"))
    except Exception as exc:
        return "TRANSPORT %s: %s" % (type(exc).__name__, exc)


def _scrub(body):
    try:
        payload = json.loads(body)
    except ValueError:
        return body
    if isinstance(payload, dict):
        payload.pop("id", None)
    return json.dumps(payload, sort_keys=False, separators=(",", ":"))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--db", required=True)
    ap.add_argument("--login", default="admin")
    ap.add_argument("--password", required=True)
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--timeout", type=float, default=120.0)
    args = ap.parse_args()

    with pathlib.Path(args.corpus).open(encoding="utf-8") as fh:
        cases = json.load(fh)
    opener = make_opener(args.port, args.db, args.login, args.password)
    out = {}
    for n, case in enumerate(cases, 1):
        out[case["id"]] = _scrub(call(opener, args.port, case, args.timeout))
        if n % 500 == 0:
            print("  %d/%d" % (n, len(cases)), file=sys.stderr)
    with pathlib.Path(args.out).open("w", encoding="utf-8") as fh:
        json.dump(out, fh)
    print("PARITY recorded %d responses to %s" % (len(out), args.out))


if __name__ == "__main__":
    main()
