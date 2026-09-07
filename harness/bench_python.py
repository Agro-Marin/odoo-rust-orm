import json
import os
import sys
import time

HARNESS = os.environ.get("RUSTORM_HARNESS", "/home/marin/Odoo/odoo-rust-orm/harness")
CORPUS = os.environ.get("RUSTORM_CORPUS", os.path.join(HARNESS, "corpus.json"))
OUT = os.environ.get("RUSTORM_BENCH_OUT", os.path.join(HARNESS, "bench_python.json"))
ITERS = int(os.environ.get("RUSTORM_BENCH_ITERS", 50))

sys.path.insert(0, HARNESS)
from cases import run_case  # noqa: E402


def _group_label(value):
    if not hasattr(value, "_name"):
        return value
    return [value.id, value.display_name] if value else False


def main(env):
    env = env(user=2, su=False)
    env = env(context=dict(env.context, lang="en_US"))
    corpus = json.load(open(CORPUS))
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
            times.sort()
            results[case["id"]] = {
                "p50": times[len(times) // 2],
                "p95": times[int(len(times) * 0.95) % len(times)],
                "mean": sum(times) / len(times),
            }
    with open(OUT, "w") as f:
        json.dump(results, f, indent=1)
    allp50 = sorted(v["p50"] for v in results.values())
    print(
        "wrote %s: %d cases, median-of-p50 %.3fms"
        % (OUT, len(results), allp50[len(allp50) // 2])
    )


main(env)  # noqa: F821
