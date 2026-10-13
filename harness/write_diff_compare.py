import datetime
import json
import math
import pathlib
import sys

with pathlib.Path(sys.argv[1]).open(encoding="utf-8") as fh:
    armed = json.load(fh)
with pathlib.Path(sys.argv[2]).open(encoding="utf-8") as fh:
    control = json.load(fh)
expect_fault = sys.argv[3] == "--expect-fault" if len(sys.argv) > 3 else False


def same(t, a, b):
    if a is None or b is None:
        return a is b
    if t in ("float", "monetary"):
        try:
            return math.isclose(float(a), float(b), rel_tol=1e-9, abs_tol=1e-9)
        except TypeError, ValueError:
            return a == b
    return a == b


def leg_clocks(armed, control):
    if "clock" not in armed or "clock" not in control:
        return None
    return [
        [datetime.datetime.fromisoformat(stamp) for stamp in leg["clock"]]
        for leg in (armed, control)
    ]


def clock_shifted(t, a, b, clocks, given):
    if clocks is None or a is None or b is None or given in (a, b):
        return False
    (a0, a1), (c0, c1) = clocks
    if t == "datetime":
        parse = datetime.datetime.fromisoformat
        slack = datetime.timedelta(seconds=1)
        lo, hi = a0 - c1 - slack, a1 - c0 + slack
    elif t == "date":
        parse = datetime.date.fromisoformat
        lo, hi = a0.date() - c1.date(), a1.date() - c0.date()
    else:
        return False
    return lo <= parse(a) - parse(b) <= hi


NATIVE_KEYS = {"copy": None, "update": "update_rows"}
mismatches = []
clock_derived = set()
clocks = leg_clocks(armed, control)
compared = models = rows_n = 0
not_native = []
if not armed.get("armed"):
    print("WRITE DIFF FAILED: the first dump is not from an armed leg")
    sys.exit(1)
if control.get("armed") or any(m["native"] for m in control["models"].values()):
    print("WRITE DIFF FAILED: the control leg wrote natively, so it is no control")
    sys.exit(1)
mismatches.extend(
    (name, "*", "model on the control leg only")
    for name in sorted(set(control["models"]) - set(armed["models"]))
)
for name, m in sorted(armed["models"].items()):
    c = control["models"].get(name)
    if c is None or c["columns"] != m["columns"]:
        mismatches.append(
            (name, "*", "columns differ or model absent on the control leg")
        )
        continue
    native_key = NATIVE_KEYS.get(m.get("scenario"), "create_rows")
    if native_key and not any(k.startswith(native_key) for k in m["native"]):
        not_native.append(name)
    models += 1
    given_rows = m.get("supplied") or [[None] * len(m["columns"])] * len(m["rows"])
    for k, (ra, rc, given_row) in enumerate(
        zip(m["rows"], c["rows"], given_rows, strict=True)
    ):
        if (ra is None) != (rc is None):
            mismatches.append((name, k, "row present on one leg only"))
            continue
        if ra is None:
            continue
        rows_n += 1
        for col, t, a, b, given in zip(
            m["columns"], m["types"], ra, rc, given_row, strict=True
        ):
            if col == "parent_path":
                continue
            compared += 1
            if same(t, a, b):
                continue
            if clock_shifted(t, a, b, clocks, given):
                clock_derived.add("%s.%s" % (name, col))
                continue
            mismatches.append((name, k, "%s: armed %r control %r" % (col, a, b)))

if expect_fault:
    hit = {
        (n, k) for n, k, msg in mismatches if isinstance(k, int) and ": armed" in msg
    }
    want = {
        (n, k)
        for n, m in armed["models"].items()
        if "char" in m["types"]
        for k, r in enumerate(m["rows"])
        if r is not None
    }
    ok = want and want <= hit
    print(
        "WRITE DIFF positive control: %d faulted rows, %d reported, %s"
        % (len(want), len(hit), "OK" if ok else "FAILED")
    )
    sys.exit(0 if ok else 1)

if clock_derived:
    print(
        "  CLOCK-DERIVED (both legs stored their own clock, shifted by the gap between "
        "the legs): %s" % ", ".join(sorted(clock_derived))
    )
for n, k, msg in mismatches[:20]:
    print("  MISMATCH %s[%s] %s" % (n, k, msg))
if not_native:
    print(
        "  NOT NATIVE (the port delegated every write the model was measured on): %s"
        % ", ".join(not_native)
    )
verdict = "OK" if compared and not mismatches and not not_native else "FAILED"
if not compared:
    print("  NOTHING COMPARED: every model was skipped or absent")
print(
    "WRITE DIFF %s: %d models, %d rows, %d cells compared, %d mismatches, %d not native"
    % (verdict, models, rows_n, compared, len(mismatches), len(not_native))
)
sys.exit(0 if verdict == "OK" else 1)
