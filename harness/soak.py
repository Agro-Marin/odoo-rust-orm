#!/usr/bin/env python3
import argparse
import json
import os
import random
import statistics
import sys
import threading
import time
import urllib.error
import urllib.request

STOP = threading.Event()


def call(port, payload, token=None):
    body = json.dumps(payload).encode()
    req = urllib.request.Request(
        "http://127.0.0.1:%d/call" % port,
        data=body,
        headers={"content-type": "application/json"},
    )
    if token:
        req.add_header("X-Rustorm-Token", token)
    with urllib.request.urlopen(req, timeout=30) as r:
        return r.read().decode()


def health(port):
    with urllib.request.urlopen("http://127.0.0.1:%d/health" % port, timeout=5) as r:
        return json.loads(r.read().decode())


def rss_of(port):
    try:
        import subprocess

        pid = subprocess.check_output(
            ["bash", "-c", "ss -ltnp 2>/dev/null | grep ':%d ' | grep -o 'pid=[0-9]*'" % port]
        ).decode()
        pid = pid.strip().split("=")[1].split(",")[0]
        with open("/proc/%s/status" % pid) as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except Exception:
        return None


def build_cases(models, uids):
    out = []
    for m in models:
        for uid in uids:
            out.append({"model": m, "method": "search_count", "uid": uid})
            out.append(
                {
                    "model": m,
                    "method": "search_read",
                    "fields": ["id"],
                    "limit": 5,
                    "uid": uid,
                }
            )
    return out


def worker(port, cases, expected, token, latencies, errors, mismatches):
    rng = random.Random(threading.get_ident())
    while not STOP.is_set():
        i = rng.randrange(len(cases))
        t0 = time.monotonic()
        try:
            got = call(port, cases[i], token)
        except Exception as e:  # noqa: BLE001
            errors.append("%s: %s" % (cases[i]["model"], e))
            continue
        latencies.append(time.monotonic() - t0)
        if got != expected[i]:
            mismatches.append(
                "%s uid=%s\n  once: %.200s\n  now : %.200s"
                % (cases[i]["model"], cases[i].get("uid"), expected[i], got)
            )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8072)
    ap.add_argument("--threads", type=int, default=8)
    ap.add_argument("--seconds", type=float, default=30.0)
    ap.add_argument("--models", default="res.partner,res.country,res.users,res.company")
    ap.add_argument("--uids", default="")
    args = ap.parse_args()
    token = os.environ.get("RUSTORM_SERVE_TOKEN") or None

    h = health(args.port)
    print("health: %s" % h)
    if h.get("status") != "ok":
        print("server is not healthy; refusing to soak it")
        return 2

    uids = [int(u) for u in args.uids.split(",") if u.strip()] or [None]
    cases = build_cases([m for m in args.models.split(",") if m], uids)

    expected, keep = [], []
    for c in cases:
        try:
            expected.append(call(args.port, c, token))
            keep.append(c)
        except Exception as e:  # noqa: BLE001
            print("  skipping %s uid=%s: %s" % (c["model"], c.get("uid"), e))
    cases = keep
    if not cases:
        print("no case the server can answer; nothing to soak")
        return 2
    print("baseline: %d cases over %d models" % (len(cases), len(set(c["model"] for c in cases))))

    rss_before = rss_of(args.port)
    latencies, errors, mismatches = [], [], []
    threads = [
        threading.Thread(
            target=worker,
            args=(args.port, cases, expected, token, latencies, errors, mismatches),
            daemon=True,
        )
        for _ in range(args.threads)
    ]
    t0 = time.monotonic()
    for t in threads:
        t.start()
    try:
        time.sleep(args.seconds)
    except KeyboardInterrupt:
        pass
    STOP.set()
    for t in threads:
        t.join(timeout=35)
    elapsed = time.monotonic() - t0
    rss_after = rss_of(args.port)

    n = len(latencies)
    print(
        "\n%d requests in %.1fs on %d threads = %.0f req/s"
        % (n, elapsed, args.threads, n / elapsed if elapsed else 0)
    )
    if n:
        lat = sorted(latencies)
        print(
            "latency ms: p50=%.1f p95=%.1f p99=%.1f max=%.1f"
            % (
                lat[n // 2] * 1000,
                lat[int(n * 0.95)] * 1000,
                lat[int(n * 0.99)] * 1000,
                lat[-1] * 1000,
            )
        )
        print("            mean=%.1f" % (statistics.mean(lat) * 1000))
    if rss_before and rss_after:
        print(
            "rss: %d KB -> %d KB (%+d KB over %d requests)"
            % (rss_before, rss_after, rss_after - rss_before, n)
        )
    print("errors: %d   mismatches: %d" % (len(errors), len(mismatches)))
    for e in errors[:5]:
        print("  error: %s" % e)
    for m in mismatches[:5]:
        print("  MISMATCH %s" % m)

    try:
        after = health(args.port)
        print("health after: %s" % after)
        healthy = after.get("status") == "ok"
    except Exception as e:  # noqa: BLE001
        print("health after: UNREACHABLE (%s)" % e)
        healthy = False

    ok = not errors and not mismatches and healthy
    print("\nSOAK %s" % ("OK" if ok else "FAILED"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
