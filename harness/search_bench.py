import json
import os
import pathlib
import sys
import time

_HERE = (
    str(pathlib.Path(__file__).resolve().parent) if "__file__" in globals() else None
)
sys.path.insert(
    0,
    os.environ.get("RUSTORM_HARNESS")
    or _HERE
    or os.path.join(
        os.environ.get("RUSTORM_WORKSPACE") or pathlib.Path("~/Odoo").expanduser(),
        "odoo-rust-orm",
        "harness",
    ),
)
import engine_py
from _env import base_env, harness_dir
from cases import case_env

CORPUS = os.environ.get("RUSTORM_SWEEP") or os.path.join(harness_dir(), "corpus.json")
ROUNDS = int(os.environ.get("RUSTORM_SEARCH_BENCH_ROUNDS", "3"))
TX_EVERY = int(os.environ.get("RUSTORM_SEARCH_BENCH_TX_EVERY", "0"))

port = engine_py.install_backend()
ORIGINAL = port.RustBackend.NATIVE

with pathlib.Path(CORPUS).open(encoding="utf-8") as fh:
    corpus = json.load(fh)
base = base_env(env)  # noqa: F821
picked, seen, refused = [], set(), 0
for case in corpus:
    key = json.dumps(
        [
            case["model"],
            case.get("domain"),
            case.get("uid"),
            case.get("order"),
            case.get("limit"),
        ],
        sort_keys=True,
    )
    if key in seen:
        continue
    seen.add(key)
    try:
        cenv = case_env(base, case)
        if case["model"] not in cenv:
            continue
        model = cenv[case["model"]]
        args = (case.get("domain") or [], case.get("order"), case.get("limit"))
        with cenv.cr.savepoint():
            model._search(args[0], limit=args[2], order=args[1]).get_result_ids()
        picked.append((model, *args))
    except Exception:
        refused += 1


def run(*, armed):
    port.RustBackend.NATIVE = (ORIGINAL | {"search"}) if armed else ORIGINAL
    build = execute = 0.0
    port.reset_stats()
    for n, (model, domain, order, limit) in enumerate(picked, 1):
        if TX_EVERY and n % TX_EVERY == 0:
            env.cr.rollback()  # noqa: F821
        t0 = time.perf_counter()
        query = model._search(domain, limit=limit, order=order)
        t1 = time.perf_counter()
        query.get_result_ids()
        build += t1 - t0
        execute += time.perf_counter() - t1
    return build, execute, port.stats()["native"]


print(
    "SEARCH BENCH %d searches per round, %d rounds per leg (%d cases python refuses), "
    "a transaction every %s" % (len(picked), ROUNDS, refused, TX_EVERY or "round")
)
totals = {True: [], False: []}
try:
    for n in range(ROUNDS):
        for armed in (False, True):
            build, execute, native = run(armed=armed)
            totals[armed].append(build)
            print(
                "  round %d %-6s search() %.3fs  execute %.3fs  %s"
                % (
                    n,
                    "native" if armed else "python",
                    build,
                    execute,
                    "native=%s online=%s"
                    % (native.get("search", 0), native.get("search.online", 0))
                    if armed
                    else "",
                )
            )
finally:
    port.RustBackend.NATIVE = ORIGINAL
    env.cr.rollback()  # noqa: F821
best = {armed: min(values) for armed, values in totals.items()}
print(
    "SEARCH BENCH best search(): python %.3fs native %.3fs -> native is %.2fx python"
    % (best[False], best[True], best[True] / best[False])
)
