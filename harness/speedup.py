#!/usr/bin/env python3
import argparse
import json
import statistics
import sys


def load_python(path):
    return {k: v["p50"] for k, v in json.load(open(path)).items()}


def load_rust(path):
    out = {}
    for line in open(path):
        parts = line.split()
        if len(parts) == 4 and parts[0][:1].isalpha():
            try:
                out[parts[0]] = float(parts[1])
            except ValueError:
                pass
    return out


def per_case_median(runs):
    cases = set().union(*runs) if runs else set()
    return {c: statistics.median([r[c] for r in runs if c in r]) for c in cases}


def drift(runs):
    if len(runs) < 2:
        return None
    spreads = []
    for c in set().union(*runs):
        vals = [r[c] for r in runs if c in r]
        if len(vals) > 1 and min(vals) > 0:
            spreads.append((max(vals) - min(vals)) / min(vals))
    return statistics.median(spreads) if spreads else None


def main():
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

    print(f"cases: {len(common)}   python runs: {len(py_runs)}   rust runs: {len(rs_runs)}")
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
