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
                # BOTH raised -- and which bucket that belongs in depends on
                # what the KERNEL raised, not only on what Python raised.
                # Checking one side counted 125 cases as "both denied access"
                # where the kernel had in fact DECLINED the case ("account.move
                # overrides the read path in Python"). Nothing was compared in
                # those: Python's AccessError and the kernel's refusal are two
                # unrelated facts, and calling them agreement inflates the pass
                # count with cases the kernel never attempted.
                kernel_error = str(act.get("error", ""))
                if str(exp.get("error_type", "")) != "AccessError":
                    vacuous.append((cid, str(exp.get("error", ""))[:90]))
                elif kernel_error.startswith("access denied"):
                    denied.append(cid)
                    passed.append(cid + " (both denied access)")
                else:
                    # Conservative direction on purpose: a denial the kernel
                    # phrases some new way lands here and UNDERSTATES the pass
                    # count, rather than a refusal landing in `denied` and
                    # overstating it.
                    refused.append((cid, kernel_error))
            continue
        ok, path = eq(exp["result"], act["result"], "$")
        if ok:
            passed.append(cid)
        else:
            failed.append((cid, f"value mismatch at {path}",
                           _dig(exp["result"], path), _dig(act["result"], path)))
    return passed, failed, refused, vacuous, denied


def parse_args(argv):
    """`expected actual [--min-compared N]`.

    The floor is the number of cases the kernel must have ANSWERED and
    matched for the run to count as a pass. Without it, a run where the kernel
    refused every case scored 0 failures and exited 0, and `verify.sh` reported
    `shadow corpus OK` for a comparison that compared nothing.
    """
    floor = os.environ.get("RUSTORM_DIFF_MIN_COMPARED", "1")
    paths = []
    it = iter(argv)
    for arg in it:
        if arg == "--min-compared":
            floor = next(it, None)
            if floor is None:
                print("REFUSING: --min-compared needs a value")
                sys.exit(2)
        else:
            paths.append(arg)
    if len(paths) != 2:
        print("usage: diff.py expected.json actual.json [--min-compared N]")
        sys.exit(2)
    try:
        floor = int(floor)
    except ValueError:
        print(f"REFUSING: --min-compared must be an integer, got {floor!r}")
        sys.exit(2)
    if floor < 0:
        print(f"REFUSING: --min-compared must be >= 0, got {floor}")
        sys.exit(2)
    return paths[0], paths[1], floor


def main():
    exp_path, act_path, floor = parse_args(sys.argv[1:])
    exp_db, exp_cases, exp_fp = load(exp_path)
    act_db, act_cases, act_fp = load(act_path)
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
    # The summary line is what `verify.sh` greps for its verdict, so a run
    # that compared fewer cases than the floor must NOT start with PASS: a
    # kernel that refuses everything is a kernel that verified nothing.
    short = compared < floor
    print((f"SHORT compared {compared} < floor {floor}  (" if short else "")
          + f"PASS {len(passed)}/{len(expected)}"
          + f"  (COMPARED {compared}, FLOOR {floor}"
          + (f", DENIED {len(denied)} — both raised AccessError, which is "
             f"agreement on the access decision" if denied else "")
          + ")"
          + (f"  REFUSED {len(refused)} (kernel declines, shim falls back)" if refused else "")
          + (f"  VACUOUS {len(vacuous)} (neither side could run the case; "
             f"this proves nothing)" if vacuous else "")
          + (")" if short else ""))
    if vacuous and VERBOSE:
        for cid, why in vacuous:
            print(f"  vacuous {cid}: {why}")
    for cid, why in refused:
        print(f"  refused {cid}: {why[:150]}")
    for cid, why, e, a in failed:
        print(f"\nFAIL {cid}: {why}")
        print(f"  expected: {json.dumps(e, default=str)[:300]}")
        print(f"  actual:   {json.dumps(a, default=str)[:300]}")
    sys.exit(1 if failed or short else 0)


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
