#!/usr/bin/env python3
"""Judge a traffic_bench timing dump: is every routed read shape at least as fast as Python?"""

import argparse
import collections
import hashlib
import json
import math
import pathlib
import statistics
import sys

# kwargs whose value decides the work a call does, kept literally in the shape
LITERAL = {
    "fields",
    "groupby",
    "aggregates",
    "order",
    "limit",
    "offset",
    "count_limit",
    "operator",
    "lazy",
}


def value_kind(value):
    if isinstance(value, list | tuple):
        return "list"
    return type(value).__name__


def skeleton(domain):
    """A domain's operators and field paths, without its values."""
    if not isinstance(domain, list | tuple):
        return domain
    if (
        len(domain) == 3
        and isinstance(domain[0], str)
        and isinstance(domain[1], str)
        and domain[0] not in ("&", "|", "!")
    ):
        return [domain[0], domain[1], value_kind(domain[2])]
    return [skeleton(node) for node in domain]


def spec_shape(spec):
    if not isinstance(spec, dict):
        return value_kind(spec)
    return {
        key: spec_shape(value.get("fields", {})) if isinstance(value, dict) else ""
        for key, value in sorted(spec.items())
    }


def shape_of(call):
    """What a call asks for, with the values that only select rows left out."""
    args = call.get("args") or []
    kwargs = call.get("kwargs") or {}
    shape = {
        "args": [
            "%s:%d" % (value_kind(a), len(a)) if isinstance(a, list) else value_kind(a)
            for a in args
        ],
    }
    for key, value in sorted(kwargs.items()):
        if key == "domain":
            shape[key] = skeleton(value)
        elif key == "specification":
            shape[key] = spec_shape(value)
        elif key in LITERAL:
            shape[key] = value
        elif key == "name":
            shape[key] = bool(value)
        else:
            shape[key] = value_kind(value)
    return shape


def shape_key(call):
    canon = json.dumps(shape_of(call), sort_keys=True, default=str)
    digest = hashlib.sha1(canon.encode(), usedforsecurity=False).hexdigest()[:8]
    return "%s.%s#%s" % (call["model"], call["method"], digest), canon


def median_bounds(values, confidence):
    """Distribution-free interval for the median, from order statistics.

    With n paired ratios the median lies between the k-th smallest and the k-th
    largest with probability at least `confidence`, k taken from Binomial(n, 1/2).
    Too few rounds for that confidence widens it to [min, max].
    """
    ordered = sorted(values)
    n = len(ordered)
    tail = (1 - confidence) / 2
    k, cumulative = 0, 0.0
    for j in range(n):
        p = math.comb(n, j) / 2**n
        if cumulative + p > tail:
            break
        cumulative += p
        k = j + 1
    k = max(k, 1)
    return ordered[k - 1], ordered[n - k]


def judge(off, on, confidence, floor=None):
    """Speedup (python time / routed time) of one group over the paired rounds."""
    ratios = [a / b for a, b in zip(off, on, strict=True) if a > 0 and b > 0]
    if not ratios:
        return None
    ratio = statistics.median(ratios)
    lo, hi = median_bounds(ratios, confidence)
    drift = max(hi / ratio - 1, 1 - lo / ratio)
    if hi < 1.0:
        verdict = "slower"
    elif lo >= 1.0:
        verdict = "faster"
    else:
        verdict = "inconclusive"
    if floor is not None and hi < floor:
        verdict = "regressed"
    return {
        "ratio": round(ratio, 4),
        "lo": round(lo, 4),
        "hi": round(hi, 4),
        "drift": round(drift, 4),
        "off_ms": round(statistics.median(off) * 1000, 3),
        "on_ms": round(statistics.median(on) * 1000, 3),
        "verdict": verdict,
    }


def read_calls(path):
    return [
        json.loads(line)
        for line in pathlib.Path(path).read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


def measure(dump, calls, confidence, floors):
    """Every (shape, path) group of a dump, judged; plus the summary groups."""
    rounds = dump["rounds"]
    groups = collections.defaultdict(list)
    canon = {}
    partial = dump.get("partial") or {}
    for index, routed in dump["routed"].items():
        key, canon_text = shape_key(calls[int(index)])
        if not routed:
            path = "fallback"
        elif partial.get(index):
            path = "partial"
        else:
            path = "routed"
        groups[key, path].append(index)
        canon[key] = canon_text

    def totals(indexes):
        return (
            [sum(r["off"][i] for i in indexes) for r in rounds],
            [sum(r["on"][i] for i in indexes) for r in rounds],
        )

    shapes = {}
    for (key, path), indexes in groups.items():
        # a shape whose calls took different paths is one entry per path
        split = sum((key, p) in groups for p in ("routed", "partial", "fallback")) > 1
        name = "%s@%s" % (key, path) if split else key
        result = judge(*totals(indexes), confidence, floors.get(name))
        if result is None:
            continue
        first = calls[int(indexes[0])]
        shapes[name] = {
            "model": first["model"],
            "method": first["method"],
            "path": path,
            "calls": len(indexes),
            "indexes": [int(i) for i in indexes],
            "shape": canon[key],
            "floor": floors.get(name),
            **result,
        }

    everything = list(dump["routed"])
    routed_only = [i for i in everything if dump["routed"][i]]
    summary = {"all calls": judge(*totals(everything), confidence)}
    if routed_only:
        summary["routed calls"] = judge(*totals(routed_only), confidence)
    by_method = collections.defaultdict(list)
    for i in routed_only:
        by_method[calls[int(i)]["method"]].append(i)
    for method, indexes in sorted(by_method.items()):
        summary[method] = judge(*totals(indexes), confidence)
    return shapes, summary


FAILING = ("slower", "regressed")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("timings", help="RUSTORM_TRAFFIC_JSON written by traffic_bench")
    ap.add_argument("--workload", help="the replayed JSONL (default: the dump's)")
    ap.add_argument("--json", help="write the verdicts here")
    ap.add_argument("--floors", help="shape -> minimum speedup, checked at hi")
    ap.add_argument("--write-floors", help="record every faster shape's lo here")
    ap.add_argument(
        "--suspects", help="write the calls of every failing shape here, as JSONL"
    )
    ap.add_argument(
        "--confirm",
        help="a second dump that re-timed the suspects: a failure it does not "
        "reproduce is 'unconfirmed', not a failure",
    )
    ap.add_argument("--confidence", type=float, default=0.99)
    ap.add_argument("--show", type=int, default=40, help="rows per table")
    args = ap.parse_args()

    dump = json.loads(pathlib.Path(args.timings).read_text(encoding="utf-8"))
    calls = read_calls(args.workload or dump["replay"])
    floors = {}
    if args.floors and pathlib.Path(args.floors).exists():
        floors = json.loads(pathlib.Path(args.floors).read_text(encoding="utf-8"))
    shapes, summary = measure(dump, calls, args.confidence, floors)

    # A partly routed call paid for the attempt as well, so it is gated with
    # the routed ones; a control run routes nothing, so every shape stands in.
    routed = {
        k: s
        for k, s in shapes.items()
        if dump["control"] or s["path"] in ("routed", "partial")
    }
    first_pass = [k for k, s in routed.items() if s["verdict"] in FAILING]
    if args.suspects:
        with pathlib.Path(args.suspects).open("w", encoding="utf-8") as fh:
            fh.writelines(
                json.dumps(calls[i]) + "\n"
                for key in first_pass
                for i in routed[key]["indexes"]
            )
    if args.confirm:
        # Hundreds of shapes at 99% each leave a few false failures per run on
        # a shared machine; an independent re-timing of only the suspects
        # leaves a real one standing and lets a fluke go.
        again = json.loads(pathlib.Path(args.confirm).read_text(encoding="utf-8"))
        second, _ = measure(again, read_calls(again["replay"]), args.confidence, floors)
        for key in first_pass:
            other = second.get(key)
            routed[key]["confirm"] = (
                {k: other[k] for k in ("ratio", "lo", "hi", "drift", "verdict")}
                if other
                else None
            )
            if not other or other["verdict"] not in FAILING:
                routed[key]["verdict"] = "unconfirmed"

    counts = collections.Counter(s["verdict"] for s in routed.values())
    failing = sorted(
        (k for k, s in routed.items() if s["verdict"] in FAILING),
        key=lambda k: routed[k]["ratio"],
    )
    below = sorted(
        (k for k, s in routed.items() if s["ratio"] < 1.0),
        key=lambda k: routed[k]["ratio"],
    )
    total = summary.get("routed calls")
    total_fail = bool(total) and total["hi"] < 1.0

    label = (
        "CONTROL (python against python)" if dump["control"] else "routing on vs off"
    )
    load = dump.get("loadavg") or [[0, 0, 0], [0, 0, 0]]
    print(
        "PERF %s: %d pairs, %d calls, %d routed shapes, confidence %.2f, "
        "loadavg %.2f -> %.2f"
        % (
            label,
            len(dump["rounds"]),
            len(dump["routed"]),
            len(routed),
            args.confidence,
            load[0][0],
            load[1][0],
        )
    )
    print(
        "PERF speedup = python time / routed time (time-weighted); >= 1.0 is the rule"
    )
    for name, s in summary.items():
        print(
            "PERF   %-16s %6.2fx  [%.2f .. %.2f]  drift +-%.0f%%  %s"
            % (name, s["ratio"], s["lo"], s["hi"], s["drift"] * 100, s["verdict"])
        )
    print(
        "PERF routed shapes: %s"
        % ", ".join("%s %d" % (v, counts[v]) for v in sorted(counts))
    )
    paths = collections.Counter(s["path"] for s in shapes.values())
    print(
        "PERF paths: %s (partial = routed, but part of the call fell back to Python)"
        % ", ".join("%s %d" % (p, paths[p]) for p in sorted(paths))
    )
    drifts = sorted(s["drift"] for s in routed.values())
    if drifts:
        print(
            "PERF per-shape drift: median +-%.0f%%, p90 +-%.0f%%"
            % (
                statistics.median(drifts) * 100,
                drifts[int(0.9 * (len(drifts) - 1))] * 100,
            )
        )
    print("PERF routed shapes with a median below 1.0x: %d" % len(below))
    print(
        "PERF   %-55s %-8s %5s %7s %15s %9s %9s  %s"
        % ("shape", "path", "calls", "ratio", "bounds", "off ms", "on ms", "verdict")
    )
    for key in below[: args.show]:
        s = routed[key]
        print(
            "PERF   %-55s %-8s %5d %6.2fx  [%5.2f .. %5.2f] %9.3f %9.3f  %s"
            % (
                key[:55],
                s["path"],
                s["calls"],
                s["ratio"],
                s["lo"],
                s["hi"],
                s["off_ms"],
                s["on_ms"],
                s["verdict"],
            )
        )
    if not dump["control"]:
        fallback = [s for s in shapes.values() if s["path"] == "fallback"]
        print(
            "PERF fallback shapes (refused, not gated) slower than Python: %d of %d"
            % (sum(1 for s in fallback if s["hi"] < 1.0), len(fallback))
        )

    if args.json:
        report = {
            "control": dump["control"],
            "pairs": len(dump["rounds"]),
            "confidence": args.confidence,
            "loadavg": load,
            "summary": summary,
            "shapes": shapes,
        }
        pathlib.Path(args.json).write_text(
            json.dumps(report, indent=1, sort_keys=True), encoding="utf-8"
        )
    if args.write_floors:
        recorded = {k: s["lo"] for k, s in routed.items() if s["verdict"] == "faster"}
        pathlib.Path(args.write_floors).write_text(
            json.dumps(recorded, indent=1, sort_keys=True), encoding="utf-8"
        )
        print("PERF wrote %d floors to %s" % (len(recorded), args.write_floors))

    if dump["control"]:
        # every failure in a control run is a false one: the instrument's rate
        print(
            "PERF CONTROL %d of %d shapes read as slower (%d before confirmation); "
            "that is the false-fail rate" % (len(failing), len(routed), len(first_pass))
        )
        return 0
    if failing or total_fail:
        print(
            "PERF FAILED: %d routed shapes are slower than Python%s; each is "
            "listed to be made faster, routing is not changed"
            % (len(failing), "; so is the routed total" if total_fail else "")
        )
        for key in failing[: args.show]:
            print("PERF   TO IMPROVE %s %s" % (key, routed[key]["shape"][:150]))
        if len(failing) > args.show:
            print("PERF   ... %d more in the JSON" % (len(failing) - args.show))
        return 1
    print("PERF OK: no routed shape is measurably slower than Python")
    return 0


if __name__ == "__main__":
    sys.exit(main())
