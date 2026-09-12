"""What routing is worth on recorded traffic, method by method.

Every other speed figure in this repository is either the kernel alone or a
synthetic corpus. This replays calls Odoo actually served -- the byte-parity
stage's capture, or any `rust_engine_capture` file -- in one process, twice
per round: once with the method shim routing and once with it off, verify
sampling at zero so nothing is answered twice. Rounds interleave, and the best
of each leg is kept.

It is how a routed path that is correct and SLOWER gets found. On 2026-09-12
it read routed `web_read_group` at 3.95x Python, every answer exact: the shim
browsed each many2one group value on its own, so the caller's read of the
groups' names fetched once per record per field.

It also splits every method's calls by whether the routed leg actually
reached the kernel, and names the slowest calls that did not. The first
readings were distorted by exactly that: four `iap.account` calls, whose
`web_read` override does its own slow work, took 2.06 s of a 2.88 s
`web_search_read` total, so the method read 0.89x while the calls the kernel
answered ran at 0.55x.

    RUSTORM_REPLAY=<capture.jsonl> odoo-bin shell ... < traffic_bench.py
"""

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


def replay(mode):
    rust_orm_shim.set_mode(mode)
    spent = collections.defaultdict(float)
    count = collections.Counter()
    for index, call in enumerate(calls):
        cenv = env(user=call.get("uid") or 2, context=dict(call.get("context") or {}))  # noqa: F821
        if call["model"] not in cenv:
            continue
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
            # a call Python refuses is timed as it fails
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
