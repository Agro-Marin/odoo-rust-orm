#!/usr/bin/env python3

import collections
import json
import os
import pathlib
import sys

HERE = os.path.dirname(os.path.abspath(__file__))


def stats(path):
    data = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
    cases = data["cases"] if isinstance(data, dict) else data
    return {
        "cases": len(cases),
        "models": len({c["model"] for c in cases}),
        "methods": dict(collections.Counter(c["method"] for c in cases)),
        "identities": {
            "uid": sum(1 for c in cases if c.get("uid") is not None),
            "su": sum(1 for c in cases if c.get("su")),
            "active_test": sum(1 for c in cases if "active_test" in c),
            "lang": sum(1 for c in cases if c.get("lang")),
        },
        "top_models": collections.Counter(c["model"] for c in cases).most_common(5),
    }


def describe(s):
    methods = ", ".join(
        f"{n} {m}" for m, n in sorted(s["methods"].items(), key=lambda x: -x[1])
    )
    return f"{s['cases']} cases across {s['models']} models ({methods})"


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "corpus.json")
    s = stats(path)
    print(describe(s))
    ids = s["identities"]
    print(
        f"  identities: uid on {ids['uid']}, su on {ids['su']}, "
        f"active_test on {ids['active_test']}, lang on {ids['lang']}"
    )
    print("  most cases:", ", ".join(f"{m} {n}" for m, n in s["top_models"]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
