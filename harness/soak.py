#!/usr/bin/env python3
import argparse
import importlib.util
import json
import os
import pathlib
import random
import statistics
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import pathlib

from _env import p50, p95

STOP = threading.Event()
REQUEST_KEYS = frozenset(
    {
        "id",
        "model",
        "method",
        "domain",
        "fields",
        "limit",
        "offset",
        "order",
        "groupby",
        "aggregates",
        "uid",
        "su",
        "lang",
        "allowed_company_ids",
        "groupby_labels",
        "active_test",
        "x2many_active_test",
        "tz",
    }
)


def load_diff():
    path = os.path.join(
        pathlib.Path(pathlib.Path(__file__).resolve()).parent, "diff.py"
    )
    spec = importlib.util.spec_from_file_location("rustorm_diff", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def call(port, payload, token=None):
    body = json.dumps(payload).encode()
    req = urllib.request.Request(
        "http://127.0.0.1:%d/call" % port,
        data=body,
        headers={"content-type": "application/json"},
    )
    if token:
        req.add_header("X-Rustorm-Token", token)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def health(port):
    with urllib.request.urlopen("http://127.0.0.1:%d/health" % port, timeout=5) as r:
        return json.loads(r.read().decode())


def rss_of(port):
    try:
        pid = subprocess.check_output(
            [
                "bash",
                "-c",
                "ss -ltnp 2>/dev/null | grep ':%d ' | grep -o 'pid=[0-9]*'" % port,
            ]
        ).decode()
        pid = pid.strip().split("=")[1].split(",")[0]
        with pathlib.Path("/proc/%s/status" % pid).open(encoding="utf-8") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except Exception:
        return None


def _canonical(value):
    if isinstance(value, list):
        return sorted(json.dumps(v, sort_keys=True, default=str) for v in value)
    return value


class Case:
    def __init__(
        self, label, payload, want_status=None, want_result=None, want_text=None
    ) -> None:
        self.label = label
        self.payload = payload
        self.want_status = want_status
        self.want_result = want_result
        self.want_text = want_text

    def check(self, eq, status, text):
        if self.want_text is not None:
            return (status, text) == self.want_text, self.want_text[1]
        if self.want_status == 200:
            if status != 200:
                return False, json.dumps(self.want_result)[:200]
            got = json.loads(text).get("result")
            ok, _path = eq(self.want_result, got, "$")
            if not ok and self._order_is_not_total():
                # two rows equal under the requested order come back in
                # either order, from Python as much as from the kernel: the
                # baseline pinned one of them; compare as a set instead
                ok, _path = eq(_canonical(self.want_result), _canonical(got), "$")
            return ok, json.dumps(self.want_result)[:200]
        return status != 200, "an error (%s)" % self.want_status

    def _order_is_not_total(self):
        method = self.payload.get("method")
        order = (self.payload.get("order") or "").lower()
        if method == "read_group":
            return True
        if method == "search_read":
            terms = [t.strip().split()[0] for t in order.split(",") if t.strip()]
            return "id" not in terms
        return False


def model_cases(models, uids):
    out = []
    for m in models:
        for uid in uids:
            base = {"model": m, "uid": uid} if uid is not None else {"model": m}
            out.append(
                Case(
                    "%s.search_count uid=%s" % (m, uid),
                    dict(base, method="search_count"),
                )
            )
            out.append(
                Case(
                    "%s.search_read uid=%s" % (m, uid),
                    dict(base, method="search_read", fields=["id"], limit=5),
                )
            )
    return out


def baseline_cases(path, auth, other_uid):
    data = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
    corpus_path = os.environ.get("RUSTORM_CORPUS") or os.path.join(
        pathlib.Path(pathlib.Path(__file__).resolve()).parent, "corpus.json"
    )
    corpus = {
        c["id"]: c
        for c in json.loads(pathlib.Path(corpus_path).read_text(encoding="utf-8"))
    }
    pinned = auth.get("uid") if auth.get("mode") == "pinned" else None
    # a token server refuses `su` unless it was started with
    # RUSTORM_SERVE_ALLOW_SU=1, and /health says which; a case the server
    # would answer 403 by policy is not a comparison
    su_refused = auth.get("mode") == "token" and not auth.get("allow_su", True)
    out, dropped = [], 0
    for exp in data["cases"]:
        case = corpus.get(exp["id"])
        if case is None or exp.get("skipped"):
            continue
        payload = {k: v for k, v in case.items() if k in REQUEST_KEYS}
        uid = payload.get("uid", 2)
        if uid == "other":
            if other_uid is None:
                dropped += 1
                continue
            payload["uid"] = uid = other_uid
        if pinned is not None and (uid != pinned or payload.get("su")):
            dropped += 1
            continue
        if su_refused and payload.get("su"):
            dropped += 1
            continue
        if exp["ok"]:
            out.append(Case("corpus %s" % exp["id"], payload, 200, exp["result"]))
        elif exp.get("error_type") == "AccessError":
            out.append(Case("corpus %s" % exp["id"], payload, 403))
    return out, dropped, data.get("db")


def worker(port, cases, token, eq, latencies, errors, mismatches) -> None:
    rng = random.Random(threading.get_ident())
    while not STOP.is_set():
        case = cases[rng.randrange(len(cases))]
        t0 = time.monotonic()
        try:
            status, text = call(port, case.payload, token)
        except Exception as e:
            errors.append("%s: %s" % (case.label, e))
            continue
        latencies.append(time.monotonic() - t0)
        ok, want = case.check(eq, status, text)
        if not ok:
            mismatches.append(
                "%s\n  want: %.200s\n  got : %d %.200s"
                % (case.label, want, status, text)
            )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8072)
    ap.add_argument("--threads", type=int, default=8)
    ap.add_argument("--seconds", type=float, default=30.0)
    ap.add_argument("--models", default="res.partner,res.country,res.users,res.company")
    ap.add_argument("--uids", default="")
    ap.add_argument(
        "--other-uid",
        type=int,
        default=None,
        help='what a corpus case\'s uid "other" resolves to on this database',
    )
    ap.add_argument(
        "--rss-growth-max",
        type=float,
        default=float(os.environ.get("RUSTORM_SOAK_RSS_GROWTH", "0.20")),
    )
    ap.add_argument(
        "--warmup",
        type=float,
        default=15.0,
        help="upper bound in seconds on the warm-up before RSS is sampled; the "
        "warm-up ends once every pooled connection has seen every case, so "
        "per-connection statement caches filling is not read as growth",
    )
    args = ap.parse_args()
    token = os.environ.get("RUSTORM_SERVE_TOKEN") or None
    expected_path = os.environ.get("RUSTORM_EXPECTED")

    h = health(args.port)
    print("health: %s" % h)
    if h.get("status") != "ok":
        print("server is not healthy; refusing to soak it")
        return 2
    auth = h.get("auth") or {}
    uids = [int(u) for u in args.uids.split(",") if u.strip()]
    if uids and auth.get("mode") == "pinned":
        print(
            "SOAK REFUSED: --uids %s but the server pins every request to uid %s; "
            "the identities named would be silently ignored" % (uids, auth.get("uid"))
        )
        return 2
    if uids and not auth:
        print(
            "SOAK REFUSED: --uids given but /health does not report auth; the server is too old"
        )
        return 2

    eq = load_diff().eq
    cases = model_cases([m for m in args.models.split(",") if m], uids or [None])
    dropped = 0
    if expected_path and pathlib.Path(expected_path).exists():
        corpus_cases, dropped, exp_db = baseline_cases(
            expected_path, auth, args.other_uid
        )
        print(
            "baseline %s (db=%s): %d corpus cases comparable at this auth, %d dropped"
            % (expected_path, exp_db, len(corpus_cases), dropped)
        )
        if not corpus_cases:
            print(
                "SOAK REFUSED: the baseline has no case the server's identity can run"
            )
            return 2
        cases.extend(corpus_cases)

    keep, refused = [], 0
    for c in cases:
        try:
            status, text = call(args.port, c.payload, token)
        except Exception as e:
            print("  skipping %s: %s" % (c.label, e))
            continue
        if c.want_status is None:
            if status != 200:
                print("  skipping %s: %d %.120s" % (c.label, status, text))
                continue
            c.want_text = (status, text)
        elif status == 422:
            # the designed fail-closed path, the same bucket diff.py keeps
            # apart from a wrong answer; it must stay a refusal for the whole
            # soak, so the case is kept with the refusal as its expectation
            refused += 1
            c.want_status, c.want_result = None, None
            c.want_text = (status, text)
        keep.append(c)
    cases = keep
    if refused:
        print("refused by the kernel at baseline (kept as refusals): %d" % refused)
    if not cases:
        print("no case the server can answer; nothing to soak")
        return 2
    print(
        "cases: %d (%d self-consistent, %d against the baseline)"
        % (
            len(cases),
            sum(c.want_text is not None for c in cases),
            sum(c.want_text is None for c in cases),
        )
    )

    latencies, errors, mismatches = [], [], []
    threads = [
        threading.Thread(
            target=worker,
            args=(args.port, cases, token, eq, latencies, errors, mismatches),
            daemon=True,
        )
        for _ in range(args.threads)
    ]
    for t in threads:
        t.start()
    try:
        warm_target = len(cases) * int(h.get("pool") or 1)
        deadline = time.monotonic() + args.warmup
        while len(latencies) < warm_target and time.monotonic() < deadline:
            time.sleep(0.2)
        rss_before = rss_of(args.port)
        warm = len(latencies)
        t0 = time.monotonic()
        time.sleep(args.seconds / 2)
        rss_mid = rss_of(args.port) or rss_before
        time.sleep(args.seconds / 2)
    except KeyboardInterrupt:
        pass
    STOP.set()
    for t in threads:
        t.join(timeout=35)
    elapsed = time.monotonic() - t0
    rss_after = rss_of(args.port)

    n = len(latencies) - warm
    print(
        "\n%d requests in %.1fs on %d threads = %.0f req/s (after %d in a %.0fs warm-up)"
        % (n, elapsed, args.threads, n / elapsed if elapsed else 0, warm, args.warmup)
    )
    if n > 0:
        lat = sorted(latencies[warm:])
        print(
            "latency ms: p50=%.1f p95=%.1f p99=%.1f max=%.1f"
            % (
                p50(lat) * 1000,
                p95(lat) * 1000,
                lat[int(n * 0.99)] * 1000,
                lat[-1] * 1000,
            )
        )
        print("            mean=%.1f" % (statistics.mean(lat) * 1000))
    rss_ok = True
    if rss_before and rss_after:
        growth = (rss_after - rss_mid) / rss_mid
        rss_ok = growth <= args.rss_growth_max
        print(
            "rss: %d KB -> %d KB (%+d KB, %+.1f%% over %d requests; bound %.0f%%) %s"
            % (
                rss_mid,
                rss_after,
                rss_after - rss_mid,
                growth * 100,
                n,
                args.rss_growth_max * 100,
                "ok" if rss_ok else "GREW TOO MUCH",
            )
        )
    else:
        print("rss: not measured (no pid behind port %d?)" % args.port)
    print("errors: %d   mismatches: %d" % (len(errors), len(mismatches)))
    for e in errors[:5]:
        print("  error: %s" % e)
    for m in mismatches[:5]:
        print("  MISMATCH %s" % m)

    try:
        after = health(args.port)
        print("health after: %s" % after)
        healthy = after.get("status") == "ok"
    except Exception as e:
        print("health after: UNREACHABLE (%s)" % e)
        healthy = False

    ok = not errors and not mismatches and healthy and rss_ok and n > 0
    print("\nSOAK %s" % ("OK" if ok else "FAILED"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
