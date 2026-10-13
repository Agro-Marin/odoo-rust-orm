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


def parse_args():
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
    return ap.parse_args()


def identity_refusal(auth, uids):
    if uids and auth.get("mode") == "pinned":
        return (
            "SOAK REFUSED: --uids %s but the server pins every request to uid %s; "
            "the identities named would be silently ignored" % (uids, auth.get("uid"))
        )
    if uids and not auth:
        return "SOAK REFUSED: --uids given but /health does not report auth; the server is too old"
    return None


def gather_cases(args, auth, uids, expected_path):
    cases = model_cases([m for m in args.models.split(",") if m], uids or [None])
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
            return None
        cases.extend(corpus_cases)
    return cases


def answerable_cases(port, cases, token):
    keep, refused = [], 0
    for c in cases:
        try:
            status, text = call(port, c.payload, token)
        except Exception as e:
            print("  skipping %s: %s" % (c.label, e))
            continue
        if c.want_status is None:
            if status != 200:
                print("  skipping %s: %d %.120s" % (c.label, status, text))
                continue
            c.want_text = (status, text)
        elif status == 422:
            refused += 1
            c.want_status, c.want_result = None, None
            c.want_text = (status, text)
        keep.append(c)
    if refused:
        print("refused by the kernel at baseline (kept as refusals): %d" % refused)
    return keep


class LoadRun:
    def __init__(self) -> None:
        self.latencies = []
        self.errors = []
        self.mismatches = []
        self.warm = 0
        self.elapsed = 0.0
        self.rss_before = None
        self.rss_mid = None
        self.rss_after = None


def run_load(args, cases, token, eq, pool):
    run = LoadRun()
    threads = [
        threading.Thread(
            target=worker,
            args=(
                args.port,
                cases,
                token,
                eq,
                run.latencies,
                run.errors,
                run.mismatches,
            ),
            daemon=True,
        )
        for _ in range(args.threads)
    ]
    for t in threads:
        t.start()
    try:
        warm_target = len(cases) * pool
        deadline = time.monotonic() + args.warmup
        while len(run.latencies) < warm_target and time.monotonic() < deadline:
            time.sleep(0.2)
        run.rss_before = rss_of(args.port)
        run.warm = len(run.latencies)
        t0 = time.monotonic()
        time.sleep(args.seconds / 2)
        run.rss_mid = rss_of(args.port) or run.rss_before
        time.sleep(args.seconds / 2)
    except KeyboardInterrupt:
        pass
    STOP.set()
    for t in threads:
        t.join(timeout=35)
    run.elapsed = time.monotonic() - t0
    run.rss_after = rss_of(args.port)
    return run


def report_throughput(args, run):
    n = len(run.latencies) - run.warm
    print(
        "\n%d requests in %.1fs on %d threads = %.0f req/s (after %d in a %.0fs warm-up)"
        % (
            n,
            run.elapsed,
            args.threads,
            n / run.elapsed if run.elapsed else 0,
            run.warm,
            args.warmup,
        )
    )
    if n > 0:
        lat = sorted(run.latencies[run.warm :])
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
    return n


def report_rss(args, run, n):
    if not (run.rss_before and run.rss_after):
        print("rss: not measured (no pid behind port %d?)" % args.port)
        return True
    growth = (run.rss_after - run.rss_mid) / run.rss_mid
    rss_ok = growth <= args.rss_growth_max
    print(
        "rss: %d KB -> %d KB (%+d KB, %+.1f%% over %d requests; bound %.0f%%) %s"
        % (
            run.rss_mid,
            run.rss_after,
            run.rss_after - run.rss_mid,
            growth * 100,
            n,
            args.rss_growth_max * 100,
            "ok" if rss_ok else "GREW TOO MUCH",
        )
    )
    return rss_ok


def report_failures(run):
    print("errors: %d   mismatches: %d" % (len(run.errors), len(run.mismatches)))
    for e in run.errors[:5]:
        print("  error: %s" % e)
    for m in run.mismatches[:5]:
        print("  MISMATCH %s" % m)


def healthy_after(port):
    try:
        after = health(port)
        print("health after: %s" % after)
        return after.get("status") == "ok"
    except Exception as e:
        print("health after: UNREACHABLE (%s)" % e)
        return False


def main() -> int:
    args = parse_args()
    token = os.environ.get("RUSTORM_SERVE_TOKEN") or None
    expected_path = os.environ.get("RUSTORM_EXPECTED")

    h = health(args.port)
    print("health: %s" % h)
    if h.get("status") != "ok":
        print("server is not healthy; refusing to soak it")
        return 2
    auth = h.get("auth") or {}
    uids = [int(u) for u in args.uids.split(",") if u.strip()]
    refusal = identity_refusal(auth, uids)
    if refusal:
        print(refusal)
        return 2

    eq = load_diff().eq
    cases = gather_cases(args, auth, uids, expected_path)
    if cases is None:
        return 2
    cases = answerable_cases(args.port, cases, token)
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

    run = run_load(args, cases, token, eq, int(h.get("pool") or 1))
    n = report_throughput(args, run)
    rss_ok = report_rss(args, run, n)
    report_failures(run)
    healthy = healthy_after(args.port)

    ok = not run.errors and not run.mismatches and healthy and rss_ok and n > 0
    print("\nSOAK %s" % ("OK" if ok else "FAILED"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
