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
