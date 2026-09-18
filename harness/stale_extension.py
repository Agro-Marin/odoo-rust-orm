"""Print why the importable `engine_py` must not serve this checkout, or nothing.

The routed leg of a differential refuses to arm an extension built from other
sources than the checkout's and says so only at its end, as "did not route";
two lanes were spent that way on 2026-09-18 after an edit to a shim source.
`orm_tests.sh` asks this first.
"""

import importlib.util
import pathlib
import sys

import engine_py

root = pathlib.Path(sys.argv[1])
spec = importlib.util.spec_from_file_location(
    "rust_engine_addon", root / "addons/rust_engine/__init__.py"
)
addon = importlib.util.module_from_spec(spec)
spec.loader.exec_module(addon)
print(addon.stale_extension(engine_py, root) or "")
