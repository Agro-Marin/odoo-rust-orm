#!/usr/bin/env python3

import argparse
import http.cookiejar
import json
import pathlib
import statistics
import sys
import threading
import time
import urllib.request

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from _env import p50, p95

STOP = threading.Event()

CALLS = [
    (
        "res.country",
        "search_read",
        {"domain": [["code", "=", "BE"]], "fields": ["name", "code"]},
    ),
    (
        "res.currency",
        "search_read",
        {"domain": [], "fields": ["name", "symbol"], "limit": 30},
    ),
    ("res.partner", "search_count", {"domain": [["is_company", "=", True]]}),
    (
        "res.partner",
        "search_read",
        {
            "domain": [["is_company", "=", True]],
            "fields": ["name", "ref"],
            "limit": 200,
        },
    ),
    (
        "res.partner",
        "search_read",
        {
            "domain": [["name", "like", "Bench Partner 0001%"]],
            "fields": ["name", "ref", "email"],
        },
    ),
    (
        "res.partner",
        "search_read",
        {"domain": [], "fields": ["name", "country_id", "company_id"], "limit": 500},
    ),
    (
        "res.partner",
        "web_search_read",
        {
            "domain": [["is_company", "=", True]],
            "specification": {
                "name": {},
                "ref": {},
                "country_id": {"fields": {"display_name": {}}},
                "company_id": {},
                "child_ids": {},
            },
            "limit": 80,
            "count_limit": 10001,
        },
    ),
    (
        "res.country",
        "web_search_read",
        {
            "domain": [],
            "specification": {
                "name": {},
                "code": {},
                "currency_id": {"fields": {"display_name": {}}},
            },
            "offset": 40,
            "limit": 40,
            "order": "name asc",
        },
    ),
]

HEAVY_CALLS = [
    (
        "res.partner",
        "search_read",
        {
            "domain": [],
            "fields": [
                "name",
                "ref",
                "email",
                "street",
                "city",
                "zip",
                "lang",
                "country_id",
                "company_id",
                "is_company",
                "create_date",
                "write_date",
            ],
            "limit": 5000,
            "order": "id",
        },
    ),
    (
        "res.partner",
        "search_read",
        {
            "domain": [["name", "ilike", "Partner"]],
            "fields": ["name", "email", "country_id"],
            "limit": 2000,
            "order": "name",
        },
    ),
    ("res.partner", "search_count", {"domain": []}),
    (
        "res.partner",
        "read_group",
        {"domain": [], "fields": ["__count"], "groupby": ["country_id"]},
    ),
    (
        "res.partner",
        "read_group",
        {
            "domain": [["is_company", "=", False]],
            "fields": ["__count"],
            "groupby": ["country_id", "lang"],
            "lazy": False,
        },
    ),
    (
        "res.partner",
        "web_search_read",
        {
            "domain": [],
            "specification": {
                "name": {},
                "email": {},
                "country_id": {"fields": {"display_name": {}}},
                "company_id": {"fields": {"display_name": {}}},
                "is_company": {},
            },
            "limit": 1000,
            "count_limit": 200000,
            "order": "name asc",
        },
    ),
]


def _write_cycle(state, i):
    step = i % 5
    if step == 0:
        return ("crm.lead", "create", [{"name": "burn lead %d" % i}], {}), "lead"
    if step == 1:
        return (
            "crm.lead",
            "write",
            [
                [state.get("lead", 0)],
                {"name": "burn lead %d w" % i, "description": "burn %d" % i},
            ],
            {},
        ), None
    if step == 2:
        return (
            "crm.lead",
            "read",
            [[state.get("lead", 0)]],
            {"fields": ["name", "description"]},
        ), None
    if step == 3:
        return ("crm.lead", "unlink", [[state.get("lead", 0)]], {}), None
    return (
        "res.partner",
        "search_read",
        [],
        {"domain": [["is_company", "=", True]], "fields": ["name"], "limit": 20},
    ), None


WRITE_PROFILE = "writes"
PROFILES = {"default": CALLS, "heavy": HEAVY_CALLS, WRITE_PROFILE: _write_cycle}
ACTIVE = CALLS


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
    with opener.open(req, timeout=30) as r:
        payload = json.loads(r.read().decode())
    if payload.get("error") or not payload.get("result", {}).get("uid"):
        raise SystemExit("authentication failed: %.200s" % payload)
    return opener


def call(opener, port, model, method, kwargs, args=()):
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "method": "call",
            "params": {
                "model": model,
                "method": method,
                "args": list(args),
                "kwargs": kwargs,
            },
        }
    ).encode()
    req = urllib.request.Request(
        "http://127.0.0.1:%d/web/dataset/call_kw" % port,
        data=body,
        headers={"content-type": "application/json"},
    )
    with opener.open(req, timeout=60) as r:
        body = r.read().decode()
    if '"error"' in body and "error" in json.loads(body):
        raise RuntimeError(json.loads(body)["error"].get("message", "rpc error"))
    return body


def worker(port, db, login, password, latencies, errors, expected) -> None:
    try:
        opener = make_opener(port, db, login, password)
    except Exception as exc:
        errors.append("auth: %s" % exc)
        return
    i = 0
    state: dict = {}
    while not STOP.is_set():
        if callable(ACTIVE):
            (model, method, args, kwargs), remember = ACTIVE(state, i)
        else:
            model, method, kwargs = ACTIVE[i % len(ACTIVE)]
            args, remember = (), None
        i += 1
        t0 = time.monotonic()
        try:
            got = call(opener, port, model, method, kwargs, args)
        except Exception as exc:
            errors.append("%s.%s: %s" % (model, method, exc))
            continue
        latencies.append(time.monotonic() - t0)
        if remember is not None:
            result = json.loads(got).get("result")
            state[remember] = result[0] if isinstance(result, list) else result
        if (
            expected is not None
            and not callable(ACTIVE)
            and got != expected[(i - 1) % len(ACTIVE)]
        ):
            errors.append("ANSWER CHANGED for %s.%s" % (model, method))


def run(args, expected, seconds):
    STOP.clear()
    latencies, errors = [], []
    threads = [
        threading.Thread(
            target=worker,
            args=(
                args.port,
                args.db,
                args.login,
                args.password,
                latencies,
                errors,
                expected,
            ),
            daemon=True,
        )
        for _ in range(args.threads)
    ]
    t0 = time.monotonic()
    for t in threads:
        t.start()
    time.sleep(seconds)
    STOP.set()
    for t in threads:
        t.join(timeout=65)
    elapsed = time.monotonic() - t0
    return latencies, errors, elapsed


def baseline(args):
    opener = make_opener(args.port, args.db, args.login, args.password)
    if callable(ACTIVE):
        return []
    return [call(opener, args.port, m, meth, kw) for m, meth, kw in ACTIVE]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8073)
    ap.add_argument("--db", required=True)
    ap.add_argument("--login", default="admin")
    ap.add_argument("--password", required=True)
    ap.add_argument("--threads", type=int, default=8)
    ap.add_argument("--seconds", type=float, default=20.0)
    ap.add_argument("--warmup", type=float, default=5.0)
    ap.add_argument("--label", default="run")
    ap.add_argument("--profile", choices=sorted(PROFILES), default="default")
    args = ap.parse_args()
    global ACTIVE
    ACTIVE = PROFILES[args.profile]

    expected = baseline(args)
    print(
        "baseline: %d shapes, %d bytes" % (len(expected), sum(len(e) for e in expected))
    )

    run(args, None, args.warmup)
    latencies, errors, elapsed = run(args, expected, args.seconds)

    n = len(latencies)
    lat = sorted(latencies)
    print(
        json.dumps(
            {
                "label": args.label,
                "requests": n,
                "elapsed": round(elapsed, 2),
                "rps": round(n / elapsed, 1) if elapsed else 0,
                "p50_ms": round(p50(lat) * 1000, 2) if n else None,
                "p95_ms": round(p95(lat) * 1000, 2) if n else None,
                "p99_ms": round(lat[int(n * 0.99)] * 1000, 2) if n else None,
                "mean_ms": round(statistics.mean(lat) * 1000, 2) if n else None,
                "errors": len(errors),
            }
        )
    )
    for e in errors[:5]:
        print("  error: %s" % e, file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
