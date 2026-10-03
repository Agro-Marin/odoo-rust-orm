#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PY="${RUSTORM_PYTHON:-$(cd "$ROOT/.." && pwd)/p314o19m/bin/python}"

cargo build --release --manifest-path "$ROOT/Cargo.toml" -p odoo-engine-py
site="$("$PY" -c 'import sysconfig; print(sysconfig.get_paths()["platlib"])')"
cp "$ROOT/target/release/libengine_py.so" "$site/.engine_py.so.new"
mv -f "$site/.engine_py.so.new" "$site/engine_py.so"

"$PY" - "$ROOT" <<'PYEOF'
import importlib.util
import pathlib
import sys

import engine_py

root = pathlib.Path(sys.argv[1])
spec = importlib.util.spec_from_file_location("rust_engine_addon", root / "addons/rust_engine/__init__.py")
addon = importlib.util.module_from_spec(spec)
spec.loader.exec_module(addon)
stale = addon.stale_extension(engine_py, root)
if stale:
    print("ENGINE INSTALL FAILED:", stale)
    sys.exit(1)
print("ENGINE INSTALLED %s (%s build %s)" % (engine_py.__file__, engine_py.__profile__, engine_py.__source_crc__))
PYEOF
