import json
import pathlib
import sys

PREFERRED = "res.partner"

with pathlib.Path(sys.argv[1]).open(encoding="utf-8") as fh:
    models = json.load(fh)["models"]
faultable = sorted(
    name
    for name, model in models.items()
    if "char" in model["types"] and any(row is not None for row in model["rows"])
)
if not faultable:
    sys.exit("the control leg compared no model with a char column to fault")
print(PREFERRED if PREFERRED in faultable else faultable[0])
