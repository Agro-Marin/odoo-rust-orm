import collections
import json
import os
import pathlib
import sys
import time

REPLAY = os.environ.get("RUSTORM_REPLAY")
ROUNDS = int(os.environ.get("RUSTORM_TRAFFIC_ROUNDS", "3"))
if not REPLAY or not pathlib.Path(REPLAY).exists():
    print("TRAFFIC SKIP: set RUSTORM_REPLAY to a capture file")
    sys.exit(0)
try:
    import rust_orm_shim
except ImportError:
    print(
        "TRAFFIC SKIP: the method shim is not installed in this process; "
        "rust_engine logs why, a stale engine_py among the reasons"
    )
    sys.exit(0)

from odoo.service.model import call_kw

calls = [
    json.loads(line)
    for line in pathlib.Path(REPLAY).read_text(encoding="utf-8").splitlines()
    if line.strip()
]


ROUTED = {}
SERVED = set()


def serve(index, call):
    cenv = env(user=call.get("uid") or 2, context=dict(call.get("context") or {}))  # noqa: F821
    if call["model"] not in cenv:
        return
    try:
        call_kw(
            cenv[call["model"]],
            call["method"],
            call.get("args") or [],
            call.get("kwargs") or {},
        )
    except Exception:
        cenv.cr.rollback()
    else:
        SERVED.add(index)
    cenv.invalidate_all()


def replay(mode):
    rust_orm_shim.set_mode(mode)
    spent = collections.defaultdict(float)
    count = collections.Counter()
    for index, call in enumerate(calls):
        if index not in SERVED:
            continue
        cenv = env(user=call.get("uid") or 2, context=dict(call.get("context") or {}))  # noqa: F821
        started = time.perf_counter()
        routed_before = rust_orm_shim.STATS["kernel"]
        try:
            call_kw(
                cenv[call["model"]],
                call["method"],
                call.get("args") or [],
                call.get("kwargs") or {},
            )
        except Exception:
            cenv.cr.rollback()
        elapsed = time.perf_counter() - started
        if mode == "on":
            ROUTED[index] = rust_orm_shim.STATS["kernel"] > routed_before
        path = "routed" if ROUTED.get(index) else "fallback"
        spent[call["method"]] += elapsed
        count[call["method"]] += 1
        spent[call["method"], path] += elapsed
        count[call["method"], path] += 1
        spent["call", index] += elapsed
        cenv.invalidate_all()
    return sum(v for k, v in spent.items() if isinstance(k, str)), spent, count


previous_mode, previous_sample = rust_orm_shim.MODE, rust_orm_shim.SAMPLE
rust_orm_shim.set_sample(0.0)
best = {}
try:
    rust_orm_shim.set_mode("off")
    for index, call in enumerate(calls):
        serve(index, call)
    print(
        "TRAFFIC %d of %d captured calls succeed in Python on this database; the "
        "rest name records the recorded run created and are not timed"
        % (len(SERVED), len(calls))
    )
    replay("on")
    replay("off")
    for n in range(ROUNDS):
        for mode in ("off", "on"):
            routed = rust_orm_shim.STATS["kernel"]
            total, spent, count = replay(mode)
            print(
                "TRAFFIC round %d %-3s %.3fs  routed %d"
                % (n, mode, total, rust_orm_shim.STATS["kernel"] - routed)
            )
            if mode not in best or total < best[mode][0]:
                best[mode] = (total, spent, count)
finally:
    rust_orm_shim.set_mode(previous_mode)
    rust_orm_shim.set_sample(previous_sample)
    env.cr.rollback()  # noqa: F821

off, on = best["off"], best["on"]
print(
    "TRAFFIC %d calls: routing off %.3fs, on %.3fs -> on is %.2fx off"
    % (
        sum(v for k, v in off[2].items() if isinstance(k, str)),
        off[0],
        on[0],
        on[0] / off[0],
    )
)
methods = sorted((k for k in off[1] if isinstance(k, str)), key=lambda m: -off[1][m])
for method in methods:
    print(
        "TRAFFIC   %-18s %5d calls  off %.3fs  on %.3fs  %.2fx"
        % (
            method,
            off[2][method],
            off[1][method],
            on[1][method],
            on[1][method] / off[1][method] if off[1][method] else 0,
        )
    )
    for path in ("routed", "fallback"):
        n = off[2][method, path]
        if n:
            print(
                "TRAFFIC     %-8s %5d calls  off %.3fs  on %.3fs  %.2fx"
                % (
                    path,
                    n,
                    off[1][method, path],
                    on[1][method, path],
                    on[1][method, path] / off[1][method, path]
                    if off[1][method, path]
                    else 0,
                )
            )
slowest = sorted(
    (i for i in range(len(calls)) if not ROUTED.get(i) and ("call", i) in off[1]),
    key=lambda i: -off[1]["call", i],
)[:5]
for i in slowest:
    print(
        "TRAFFIC slowest fallback %.3fs  %s.%s"
        % (off[1]["call", i], calls[i]["model"], calls[i]["method"])
    )
