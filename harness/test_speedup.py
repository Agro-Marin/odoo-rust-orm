#!/usr/bin/env python3
"""`speedup.py` reports two aggregates and says what it dropped.

The median of per-case ratios and the time-weighted ratio answer different
questions and can differ by an order of magnitude on a corpus dominated by
tiny cases; a reader given only the median has no way to know. And cases one
side never produced used to vanish from the denominator in silence.
"""
import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
SPEEDUP = os.path.join(HERE, "speedup.py")
FAILURES = []


def check(name, got, want):
    if got != want:
        FAILURES.append("%s\n    got  %r\n    want %r" % (name, got, want))


def run(python_ms, rust_ms):
    with tempfile.TemporaryDirectory() as tmp:
        py = os.path.join(tmp, "py.json")
        with open(py, "w") as fp:
            json.dump({c: {"p50": ms} for c, ms in python_ms.items()}, fp)
        rs = os.path.join(tmp, "rust.txt")
        with open(rs, "w") as fp:
            fp.write("header line that is not a case\n")
            for c, ms in rust_ms.items():
                fp.write("%s %s 0 0\n" % (c, ms))
        proc = subprocess.run(
            [sys.executable, SPEEDUP, "--python", py, "--rust", rs],
            capture_output=True, text=True,
        )
    return proc.returncode, proc.stdout


def line(out, prefix):
    return next((l.strip() for l in out.splitlines() if l.strip().startswith(prefix)), "")


def main():
    # three tiny cases at 10x and one big case at 1x: the median says 10x,
    # the clock says the corpus barely moved
    py = {"c1": 0.10, "c2": 0.10, "c3": 0.10, "c4": 200.0}
    rs = {"c1": 0.01, "c2": 0.01, "c3": 0.01, "c4": 200.0}
    code, out = run(py, rs)
    check("exit", code, 0)
    check("median line", line(out, "median per-case speedup:").split()[3], "10.00x")
    weighted = float(line(out, "time-weighted speedup:").split()[2].rstrip("x"))
    check("weighted is about 1x", 1.0 <= weighted < 1.01, True)
    check("nothing dropped, nothing said", line(out, "dropped before comparing"), "")

    # a case only python produced, and one only rust produced, are counted out loud
    code, out = run({**py, "py_only": 5.0}, {**rs, "rs_only": 5.0})
    check("exit with drops", code, 0)
    check("drops reported", line(out, "dropped before comparing"),
          "dropped before comparing: 1 python-only, 1 rust-only")
    check("cases is the intersection", line(out, "cases:").split()[1], "4")

    if FAILURES:
        print("SPEEDUP FAIL %d" % len(FAILURES))
        for f in FAILURES:
            print("  " + f)
        return 1
    print("SPEEDUP OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
