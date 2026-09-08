#!/usr/bin/env python3
"""`diff.py` must not call a run that compared nothing a pass.

Runs the script as `verify.sh` does -- as a subprocess, reading its SUMMARY
line -- because the verdict `verify.sh` acts on is that line's prefix, not the
exit code, and both have to agree.
"""
import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
DIFF = os.path.join(HERE, "diff.py")
FAILURES = []


def check(name, got, want):
    if got != want:
        FAILURES.append("%s\n    got  %r\n    want %r" % (name, got, want))


def run(expected, actual, *args, env=None):
    with tempfile.TemporaryDirectory() as tmp:
        exp = os.path.join(tmp, "expected.json")
        act = os.path.join(tmp, "actual.json")
        with open(exp, "w") as fp:
            json.dump({"db": "probe", "cases": expected}, fp)
        with open(act, "w") as fp:
            json.dump({"db": "probe", "cases": actual}, fp)
        proc = subprocess.run(
            [sys.executable, DIFF, exp, act, *args],
            capture_output=True, text=True,
            env={**os.environ, **(env or {})},
        )
    summary = next(
        (l for l in proc.stdout.splitlines() if l.startswith(("PASS", "SHORT", "REFUSING"))),
        proc.stdout.strip().splitlines()[-1] if proc.stdout.strip() else "",
    )
    return proc.returncode, summary


def ok(cid, result):
    return {"id": cid, "ok": True, "result": result}


def refused(cid):
    return {"id": cid, "ok": False, "error": "kernel declines"}


def main():
    three_ok = [ok("c1", 1), ok("c2", 2), ok("c3", 3)]

    code, line = run(three_ok, three_ok)
    check("all compared, default floor: exit", code, 0)
    check("all compared, default floor: prefix", line.split()[0], "PASS")
    check("the floor is reported", "FLOOR 1" in line, True)

    all_refused = [refused("c1"), refused("c2"), refused("c3")]
    code, line = run(three_ok, all_refused)
    check("nothing compared, default floor: exit", code, 1)
    check("nothing compared, default floor: prefix", line.split()[0], "SHORT")
    check("nothing compared: says how short", "compared 0 < floor 1" in line, True)

    code, line = run(three_ok, all_refused, "--min-compared", "0")
    check("floor 0 opts out: exit", code, 0)
    check("floor 0 opts out: prefix", line.split()[0], "PASS")

    code, line = run(three_ok, [ok("c1", 1), refused("c2"), refused("c3")], "--min-compared", "2")
    check("one below an explicit floor: exit", code, 1)
    check("one below an explicit floor: prefix", line.split()[0], "SHORT")

    code, line = run(three_ok, three_ok, env={"RUSTORM_DIFF_MIN_COMPARED": "3"})
    check("env floor met: exit", code, 0)
    code, line = run(three_ok, three_ok, env={"RUSTORM_DIFF_MIN_COMPARED": "4"})
    check("env floor missed: exit", code, 1)
    check("env floor missed: prefix", line.split()[0], "SHORT")

    # a wrong answer is still a wrong answer, floor or no floor
    code, line = run(three_ok, [ok("c1", 1), ok("c2", 99), ok("c3", 3)], "--min-compared", "0")
    check("a mismatch still fails: exit", code, 1)
    check("a mismatch still fails: prefix", line.split()[0], "PASS")

    code, line = run(three_ok, three_ok, "--min-compared", "banana")
    check("non-integer floor is refused: exit", code, 2)
    check("non-integer floor is refused: prefix", line.split()[0], "REFUSING:")

    if FAILURES:
        print("DIFF FAIL %d" % len(FAILURES))
        for f in FAILURES:
            print("  " + f)
        return 1
    print("DIFF OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
