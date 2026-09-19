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


mismatches = []
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
    if m.get("scenario") != "copy" and not any(
        k.startswith("create_rows") for k in m["native"]
    ):
        not_native.append(name)
    models += 1
    for k, (ra, rc) in enumerate(zip(m["rows"], c["rows"], strict=True)):
        if (ra is None) != (rc is None):
            mismatches.append((name, k, "row present on one leg only"))
            continue
        if ra is None:
            continue
        rows_n += 1
        for col, t, a, b in zip(m["columns"], m["types"], ra, rc, strict=True):
            if col == "parent_path":
                continue
            compared += 1
            if not same(t, a, b):
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

for n, k, msg in mismatches[:20]:
    print("  MISMATCH %s[%s] %s" % (n, k, msg))
if not_native:
    print("  NOT NATIVE (the port delegated every create): %s" % ", ".join(not_native))
verdict = "OK" if compared and not mismatches and not not_native else "FAILED"
if not compared:
    print("  NOTHING COMPARED: every model was skipped or absent")
print(
    "WRITE DIFF %s: %d models, %d rows, %d cells compared, %d mismatches, %d not native"
    % (verdict, models, rows_n, compared, len(mismatches), len(not_native))
)
sys.exit(0 if verdict == "OK" else 1)
