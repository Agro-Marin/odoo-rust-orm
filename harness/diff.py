#!/usr/bin/env python3
import json
import os
import pathlib
import sys

FLOAT_TOL = 1e-9
VERBOSE = os.environ.get("RUSTORM_DIFF_VERBOSE") == "1"


ACCESS_DENIED_MARKER = "access denied"
INTERNAL_MARKERS = (
    "db error",
    "error communicating with the server",
    "connection closed",
)


def kernel_kind(act):
    kind = act.get("kind")
    if kind:
        return kind
    text = str(act.get("error", "")).lower()
    return "internal" if text.startswith(INTERNAL_MARKERS) else "refusal"


def eq(a, b, path=""):
    if isinstance(a, bool) or isinstance(b, bool):
        return (type(a) is type(b) and a == b), path
    if isinstance(a, (int, float)) and isinstance(b, (int, float)):
        if abs(a - b) <= FLOAT_TOL * max(1.0, abs(a), abs(b)):
            return True, path
        return False, path
    if type(a) is not type(b):
        return False, path
    if isinstance(a, list):
        if len(a) != len(b):
            return False, f"{path} (len {len(a)} != {len(b)})"
        for i, (x, y) in enumerate(zip(a, b, strict=False)):
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
    data = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
    if not isinstance(data, dict) or "db" not in data:
        print(
            f"REFUSING: {path} is unstamped (legacy list format). "
            f"Regenerate it with gen_expected.py against the database you are "
            f"testing."
        )
        sys.exit(2)
    ids = [c["id"] for c in data["cases"]]
    dupes = sorted({i for i in ids if ids.count(i) > 1})
    if dupes or "" in ids:
        print(f"REFUSING: {path} carries duplicate or empty case ids: {dupes[:10]}")
        sys.exit(2)
    return (
        data["db"],
        data["cases"],
        data.get("data_fingerprint"),
        data.get("has_unaccent"),
    )


def score(expected, actual):
    passed, failed, refused, skipped, rejected, denied = [], [], [], [], [], []
    for cid, exp in expected.items():
        act = actual.get(cid)
        if act is None:
            failed.append((cid, "missing in actual", None, None))
            continue
        if exp.get("skipped"):
            if act["ok"]:
                failed.append(
                    (
                        cid,
                        "kernel answered a case this database cannot run: "
                        + str(exp["skipped"]),
                        None,
                        act.get("result"),
                    )
                )
            else:
                skipped.append((cid, str(exp["skipped"])[:90]))
            continue
        if not exp["ok"] or not act["ok"]:
            if not act["ok"] and kernel_kind(act) == "internal":
                failed.append(
                    (
                        cid,
                        "kernel internal error (not a refusal): "
                        + str(act.get("error", ""))[:200],
                        exp.get("result", exp.get("error")),
                        None,
                    )
                )
            elif exp["ok"] and not act["ok"]:
                refused.append((cid, act.get("error", "")))
            elif not exp["ok"] and act["ok"]:
                failed.append(
                    (
                        cid,
                        "kernel answered where Python raised",
                        exp.get("error", ""),
                        act.get("result"),
                    )
                )
            else:
                kernel_denied = ACCESS_DENIED_MARKER in str(act.get("error", ""))
                if str(exp.get("error_type", "")) == "AccessError" and kernel_denied:
                    denied.append(cid)
                    passed.append(cid + " (both denied access)")
                else:
                    rejected.append((cid, str(exp.get("error", ""))[:90]))
            continue
        ok, path = eq(exp["result"], act["result"], "$")
        if ok:
            passed.append(cid)
        else:
            failed.append(
                (
                    cid,
                    f"value mismatch at {path}",
                    _dig(exp["result"], path),
                    _dig(act["result"], path),
                )
            )
    return passed, failed, refused, skipped, rejected, denied


def main() -> None:
    min_compared = 0
    max_refused_share = float(os.environ.get("RUSTORM_DIFF_MAX_REFUSED_SHARE", "0.5"))
    json_out = None
    argv = sys.argv[1:]
    for i, a in enumerate(argv):
        if a.startswith("--min-compared="):
            min_compared = int(a.split("=", 1)[1])
        elif a.startswith("--max-refused-share="):
            max_refused_share = float(a.split("=", 1)[1])
        elif a.startswith("--json="):
            json_out = a.split("=", 1)[1]
        elif a == "--json" and i + 1 < len(argv):
            json_out = argv[i + 1]
    args = [a for a in argv if not a.startswith("--") and a != json_out]
    exp_db, exp_cases, exp_fp, exp_unaccent = load(args[0])
    act_db, act_cases, act_fp, act_unaccent = load(args[1])
    if exp_db != act_db:
        print(f"REFUSING: baseline is from {exp_db!r}, results from {act_db!r}")
        sys.exit(2)
    if (
        exp_unaccent is not None
        and act_unaccent is not None
        and exp_unaccent != act_unaccent
    ):
        print(
            f"REFUSING: baseline saw unaccent={exp_unaccent!r}, results saw {act_unaccent!r}"
        )
        sys.exit(2)
    unmatched = sorted({c["id"] for c in act_cases} - {c["id"] for c in exp_cases})
    if unmatched:
        print(f"REFUSING: results carry cases the baseline lacks: {unmatched[:10]}")
        sys.exit(2)
    drifted = exp_fp is not None and act_fp is not None and exp_fp != act_fp
    expected = {c["id"]: c for c in exp_cases}
    actual = {c["id"]: c for c in act_cases}
    passed, failed, refused, skipped, rejected, denied = score(expected, actual)
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
            write_json(
                json_out,
                "REFUSING",
                exp_db,
                expected,
                passed,
                failed,
                refused,
                skipped,
                rejected,
                denied,
                drifted,
            )
            sys.exit(2)
        print(note + " Everything agreed anyway.")
    compared = len(passed) - len(denied)
    if compared < min_compared and not failed:
        failed.append(
            (
                "<floor>",
                f"only {compared} values compared, floor is {min_compared}",
                None,
                None,
            )
        )
    ran = len(expected) - len(skipped)
    declined = len(refused) + len(rejected)
    declined_share = declined / ran if ran else 0.0
    if ran and declined_share > max_refused_share and not failed:
        failed.append(
            (
                "<declined>",
                (
                    f"the kernel declined {declined} of the {ran} cases the baseline ran "
                    f"({declined_share:.0%}); the cap is {max_refused_share:.0%}"
                ),
                None,
                None,
            )
        )
    verdict = "FAIL" if failed else "PASS"
    write_json(
        json_out,
        verdict,
        exp_db,
        expected,
        passed,
        failed,
        refused,
        skipped,
        rejected,
        denied,
        drifted,
    )
    print(
        f"{verdict} {len(passed)}/{len(expected)}"
        f"  (COMPARED {compared}, DECLINED {declined_share:.0%} of {ran} run, cap {max_refused_share:.0%}"
        + (
            f", DENIED {len(denied)} — both raised AccessError, which is "
            f"agreement on the access decision"
            if denied
            else ""
        )
        + ")"
        + (
            f"  REFUSED {len(refused)} (kernel declines, shim falls back)"
            if refused
            else ""
        )
        + (
            f"  SKIPPED {len(skipped)} (this database lacks the model or field)"
            if skipped
            else ""
        )
        + (
            f"  REJECTED {len(rejected)} (Python raised, kernel declined; "
            f"no value to compare)"
            if rejected
            else ""
        )
    )
    if VERBOSE:
        for cid, why in skipped:
            print(f"  skipped {cid}: {why}")
        for cid, why in rejected:
            print(f"  rejected {cid}: {why}")
    for cid, why in refused:
        print(f"  refused {cid}: {why[:150]}")
    for cid, why, e, a in failed:
        print(f"\nFAIL {cid}: {why}")
        print(f"  expected: {json.dumps(e, default=str)[:300]}")
        print(f"  actual:   {json.dumps(a, default=str)[:300]}")
    sys.exit(1 if failed else 0)


def write_json(
    path,
    verdict,
    db,
    expected,
    passed,
    failed,
    refused,
    skipped,
    rejected,
    denied,
    drifted,
) -> None:
    if not path:
        return
    payload = {
        "verdict": verdict,
        "db": db,
        "total": len(expected),
        "passed": len(passed),
        "compared": len(passed) - len(denied),
        "denied": denied,
        "failed": [
            {"id": cid, "why": why, "expected": e, "actual": a}
            for cid, why, e, a in failed
        ],
        "refused": [{"id": cid, "why": why[:300]} for cid, why in refused],
        "skipped": [{"id": cid, "why": why} for cid, why in skipped],
        "rejected": [{"id": cid, "why": why} for cid, why in rejected],
        "data_fingerprint_drifted": drifted,
    }
    with pathlib.Path(path).open("w", encoding="utf-8") as f:
        json.dump(payload, f, indent=1, default=str)


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
