#!/usr/bin/env python3
import json
import os
import sys

FLOAT_TOL = 1e-9
VERBOSE = os.environ.get("RUSTORM_DIFF_VERBOSE") == "1"


def eq(a, b, path=""):
    if isinstance(a, bool) or isinstance(b, bool):
        return (a == b, path) if a == b else (False, path)
    if isinstance(a, (int, float)) and isinstance(b, (int, float)):
        if abs(a - b) <= FLOAT_TOL * max(1.0, abs(a), abs(b)):
            return True, path
        return False, path
    if type(a) is not type(b):
        return False, path
    if isinstance(a, list):
        if len(a) != len(b):
            return False, f"{path} (len {len(a)} != {len(b)})"
        for i, (x, y) in enumerate(zip(a, b)):
            ok, p = eq(x, y, f"{path}[{i}]")
            if not ok:
                return False, p
        return True, path
    if isinstance(a, dict):
        if set(a) != set(b):
            return False, f"{path} (keys {sorted(set(a) ^ set(b))})"
        for k in a:
            ok, p = eq(a[k], b[k], f"{path}.{k}")
            if not ok:
                return False, p
        return True, path
    return (a == b), path


def load(path):
    data = json.load(open(path))
    if not isinstance(data, dict) or "db" not in data:
        print(
            f"REFUSING: {path} is unstamped (legacy list format). "
            f"Regenerate it with gen_expected.py against the database you are "
            f"testing."
        )
        sys.exit(2)
    return data["db"], data["cases"], data.get("data_fingerprint")


def score(expected, actual):
    passed, failed, refused, vacuous, denied = [], [], [], [], []
    for cid, exp in expected.items():
        act = actual.get(cid)
        if act is None:
            failed.append((cid, "missing in actual", None, None))
            continue
        if not exp["ok"] or not act["ok"]:
            if exp["ok"] and not act["ok"]:
                refused.append((cid, act.get("error", "")))
            elif not exp["ok"] and act["ok"]:
                failed.append(
                    (cid, "kernel answered where Python raised",
                     exp.get("error", ""), act.get("result"))
                )
            else:
                if str(exp.get("error_type", "")) == "AccessError":
                    denied.append(cid)
                    passed.append(cid + " (both denied access)")
                else:
                    vacuous.append((cid, str(exp.get("error", ""))[:90]))
            continue
        ok, path = eq(exp["result"], act["result"], "$")
        if ok:
            passed.append(cid)
        else:
            failed.append((cid, f"value mismatch at {path}",
                           _dig(exp["result"], path), _dig(act["result"], path)))
    return passed, failed, refused, vacuous, denied


def main():
    exp_db, exp_cases, exp_fp = load(sys.argv[1])
    act_db, act_cases, act_fp = load(sys.argv[2])
    if exp_db != act_db:
        print(f"REFUSING: baseline is from {exp_db!r}, results from {act_db!r}")
        sys.exit(2)
    drifted = (
        exp_fp is not None and act_fp is not None and exp_fp != act_fp
    )
    expected = {c["id"]: c for c in exp_cases}
    actual = {c["id"]: c for c in act_cases}
    passed, failed, refused, vacuous, denied = score(expected, actual)
    if drifted:
        note = (
            f"NOTE: {exp_db!r} was written to between the two runs "
            f"(DML counter {exp_fp} -> {act_fp} on the corpus's tables)."
        )
        if failed:
            print(note)
            print(
                "REFUSING to report these failures: regenerate the baseline "
                "with gen_expected.py and re-run before believing them."
            )
            sys.exit(2)
        print(note + " Everything agreed anyway.")
    compared = len(passed) - len(denied)
    print(f"PASS {len(passed)}/{len(expected)}"
          + f"  (COMPARED {compared}"
          + (f", DENIED {len(denied)} — both raised AccessError, which is "
             f"agreement on the access decision" if denied else "")
          + ")"
          + (f"  REFUSED {len(refused)} (kernel declines, shim falls back)" if refused else "")
          + (f"  VACUOUS {len(vacuous)} (neither side could run the case; "
             f"this proves nothing)" if vacuous else ""))
    if vacuous and VERBOSE:
        for cid, why in vacuous:
            print(f"  vacuous {cid}: {why}")
    for cid, why in refused:
        print(f"  refused {cid}: {why[:150]}")
    for cid, why, e, a in failed:
        print(f"\nFAIL {cid}: {why}")
        print(f"  expected: {json.dumps(e, default=str)[:300]}")
        print(f"  actual:   {json.dumps(a, default=str)[:300]}")
    sys.exit(1 if failed else 0)


def _dig(value, path):
    try:
        cur = value
        for tok in path.replace("$", "").strip(".").split("."):
            while "[" in tok:
                base, _, rest = tok.partition("[")
                idx, _, tok2 = rest.partition("]")
                if base:
                    cur = cur[base]
                cur = cur[int(idx)]
                tok = tok2.lstrip(".")
            if tok:
                cur = cur[tok]
        return cur
    except Exception:
        return value


if __name__ == "__main__":
    main()
