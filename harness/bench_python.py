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
import pathlib

from _env import base_env, harness_dir, out_path, p50, p95
from cases import run_case

CORPUS = os.environ.get("RUSTORM_CORPUS") or os.path.join(harness_dir(), "corpus.json")
OUT = out_path("bench_python.json", "RUSTORM_BENCH_OUT")
ITERS = int(os.environ.get("RUSTORM_BENCH_ITERS", "50"))


def main(env) -> None:
    env = base_env(env)
    corpus = json.loads(pathlib.Path(CORPUS).read_text(encoding="utf-8"))
    for case in corpus:
        try:
            run_case(env, case)
        except Exception:
            env.cr.rollback()
    results = {}
    for case in corpus:
        times = []
        for _ in range(ITERS):
            env.invalidate_all()
            t0 = time.perf_counter()
            try:
                run_case(env, case)
            except Exception:
                env.cr.rollback()
                times = []
                break
            times.append((time.perf_counter() - t0) * 1000.0)
        if times:
            results[case["id"]] = {
                "p50": p50(times),
                "p95": p95(times),
                "mean": sum(times) / len(times),
            }
    with pathlib.Path(OUT).open("w", encoding="utf-8") as f:
        json.dump(results, f, indent=1)
    allp50 = sorted(v["p50"] for v in results.values())
    print(
        "wrote %s: %d cases, median-of-p50 %.3fms"
        % (OUT, len(results), allp50[len(allp50) // 2])
    )


main(env)  # noqa: F821
