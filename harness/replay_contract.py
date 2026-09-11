"""Fault controls for replay verdicts, using the configured scratch database."""

import json
import os
import pathlib
import subprocess

from _env import default_db, out_dir, workspace

root = pathlib.Path(workspace())
out = pathlib.Path(out_dir()) / "replay-controls"
out.mkdir(exist_ok=True)
export = os.environ["RUSTORM_EXPORT"]
conf = os.environ["RUSTORM_ODOO_CONF"]
python = os.environ.get("RUSTORM_PYTHON", str(root / "p314o19m/bin/python"))
odoo_root = pathlib.Path(os.environ.get("RUSTORM_ODOO_ROOT", str(root / "odoo")))
replay = pathlib.Path(__file__).with_name("replay.py")
base = {
    "model": "res.country",
    "method": "search_count",
    "uid": 2,
    "args": [],
    "kwargs": {"domain": []},
    "context": {"lang": "en_US"},
}
for name, call, injection, code in [
    ("valid", base, "", 0),
    ("missing_user", dict(base, uid=2147483647), "", 1),
    (
        "gate_only",
        base,
        "import engine_py\nengine_py.install_shims()[1]._policy_allows = lambda _: False\n",
        1,
    ),
    (
        "native_failure",
        base,
        """import engine_py
original = engine_py.RustKernel
class Broken:
 @staticmethod
 def build(*args):
  original.build(*args)
  return Broken()
 def dispatch(self,*args):
  raise engine_py.KernelInternalError("injected regression control")
engine_py.RustKernel=Broken
""",
        1,
    ),
]:
    capture = out / (name + ".jsonl")
    capture.write_text(json.dumps(call) + "\n")
    env = dict(
        os.environ,
        RUSTORM_EXPORT=export,
        RUSTORM_REPLAY=str(capture),
        RUSTORM_HARNESS=str(root / "odoo-rust-orm/harness"),
    )
    script = (
        injection
        + f"\nimport runpy\nrunpy.run_path({str(replay)!r},init_globals={{'env':env}},run_name='__main__')\n"
    )
    result = subprocess.run(
        [
            python,
            str(odoo_root / "odoo-bin"),
            "shell",
            "-c",
            conf,
            "-d",
            default_db(),
            "--no-http",
        ],
        input=script,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        env=env,
        check=False,
        timeout=60,
    )
    (out / (name + ".log")).write_text(result.stdout)
    print(
        name,
        result.returncode,
        [line for line in result.stdout.splitlines() if line.startswith("REPLAY ")][-1],
        flush=True,
    )
    assert result.returncode == code, (name, result.stdout)
