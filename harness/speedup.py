#!/usr/bin/env python3
import argparse
import json
import pathlib
import statistics
import sys

sys.path.insert(0, pathlib.Path(pathlib.Path(__file__).resolve()).parent)
import contextlib
import pathlib

from _env import p50


def load_python(path):
    return {
        k: v["p50"]
        for k, v in json.load(pathlib.Path(path).open(encoding="utf-8")).items()
    }


def load_rust(path):
    out = {}
    for line in (
        pathlib.Path(path).read_text(encoding="utf-8").splitlines(keepends=True)
    ):
        parts = line.split()
        if len(parts) == 4 and parts[0][:1].isalpha():
            with contextlib.suppress(ValueError):
                out[parts[0]] = float(parts[1])
    return out


def per_case_median(runs):
    cases = set().union(*runs) if runs else set()
    return {c: p50([r[c] for r in runs if c in r]) for c in cases}


def drift(runs):
    if len(runs) < 2:
        return None
    spreads = []
    for c in set().union(*runs):
        vals = [r[c] for r in runs if c in r]
        if len(vals) > 1 and min(vals) > 0:
            spreads.append((max(vals) - min(vals)) / min(vals))
    return statistics.median(spreads) if spreads else None


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--python", nargs="+", required=True)
    ap.add_argument("--rust", nargs="+", required=True)
    args = ap.parse_args()

    py_runs = [load_python(p) for p in args.python]
    rs_runs = [load_rust(p) for p in args.rust]
    py, rs = per_case_median(py_runs), per_case_median(rs_runs)

    common = sorted(set(py) & set(rs))
    if not common:
        sys.exit("no cases in common")
    speedups = sorted(((c, py[c] / rs[c]) for c in common), key=lambda x: x[1])
    values = [s for _, s in speedups]

    print(
        f"cases: {len(common)}   python runs: {len(py_runs)}   rust runs: {len(rs_runs)}"
    )
    for label, runs in (("python", py_runs), ("rust", rs_runs)):
        d = drift(runs)
        if d is not None:
            print(f"  {label} cross-run drift (median): {d * 100:.0f}%")
    print(f"median speedup: {statistics.median(values):.2f}x")
    print(f"rust faster in: {sum(1 for v in values if v > 1)}/{len(values)}")
    print(f"range: {values[0]:.2f}x .. {values[-1]:.2f}x")
    print("  slowest:", [(c, round(s, 2)) for c, s in speedups[:5]])
    print("  fastest:", [(c, round(s, 2)) for c, s in speedups[-5:]])


if __name__ == "__main__":
    main()
