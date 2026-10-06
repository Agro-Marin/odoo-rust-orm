"""A prefork master forks with rust_engine armed: what the fork carries.

Set RUSTORM_TEST_DSN to a disposable Odoo database and RUSTORM_ODOO_CONF to a
conf that can serve it. Each case runs in a fresh interpreter, because a
process's thread count is the property under test and pytest's own process
is not ours to count.
"""

import json
import os
import pathlib
import subprocess
import sys
import textwrap

import pytest

ROOT = pathlib.Path(__file__).resolve().parent.parent

pytestmark = pytest.mark.skipif(
    not pathlib.Path("/proc/self/task").is_dir(), reason="counts threads in /proc"
)

PROBE = textwrap.dedent(
    """
    import json, os, sys, warnings

    def threads():
        names = []
        for tid in sorted(os.listdir("/proc/self/task")):
            try:
                with open(f"/proc/self/task/{tid}/comm") as f:
                    names.append(f.read().strip())
            except OSError:
                pass
        return names

    def query(rust_db):
        conn = rust_db.connect()
        try:
            return conn.execute("SELECT 41 + 1", None).rows[0][0]
        finally:
            conn.close()

    def fork(child):
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always")
            r, w = os.pipe()
            pid = os.fork()
            if pid == 0:
                os.close(r)
                try:
                    out = child()
                except BaseException as exc:
                    out = {"error": "%s: %s" % (type(exc).__name__, exc)}
                os.write(w, json.dumps(out).encode())
                os._exit(0)
            os.close(w)
            with os.fdopen(r) as f:
                answer = json.loads(f.read() or "null")
            os.waitpid(pid, 0)
        warned = [str(w.message) for w in caught if "multi-threaded" in str(w.message)]
        return answer, warned
    """
)


def _run(body: str) -> dict:
    env = dict(os.environ)
    out = subprocess.run(
        [sys.executable, "-c", PROBE + textwrap.dedent(body)],
        env=env,
        capture_output=True,
        text=True,
        timeout=300,
        check=False,
    )
    lines = [line for line in out.stdout.splitlines() if line.startswith("{")]
    assert out.returncode == 0 and lines, (out.returncode, out.stdout, out.stderr)
    return json.loads(lines[-1])


@pytest.fixture
def dsn():
    value = os.environ.get("RUSTORM_TEST_DSN")
    if not value:
        pytest.skip("RUSTORM_TEST_DSN must name a disposable database")
    pytest.importorskip("engine_py")
    return value


def test_a_rust_connection_starts_no_thread(dsn, monkeypatch) -> None:
    monkeypatch.setenv("FORK_PROBE_DSN", dsn)
    got = _run(
        """
        import engine_py
        before = threads()
        rust_db = engine_py.RustDb(os.environ["FORK_PROBE_DSN"])
        held = rust_db.connect()
        held.execute("SELECT 1", None)
        print(json.dumps({"before": before, "after": threads()}))
        """
    )
    assert got["after"] == got["before"], got


def test_a_forked_child_queries_and_the_parent_keeps_working(dsn, monkeypatch) -> None:
    monkeypatch.setenv("FORK_PROBE_DSN", dsn)
    got = _run(
        """
        import engine_py
        rust_db = engine_py.RustDb(os.environ["FORK_PROBE_DSN"])
        held = rust_db.connect()
        before = held.execute("SELECT pg_backend_pid()", None).rows[0][0]
        at_fork = threads()
        child, warned = fork(lambda: {"answer": query(rust_db), "pid": os.getpid()})
        print(json.dumps({
            "at_fork": at_fork,
            "warned": warned,
            "child": child,
            "parent_new": query(rust_db),
            "parent_held": held.execute("SELECT pg_backend_pid()", None).rows[0][0]
            == before,
        }))
        """
    )
    assert len(got["at_fork"]) == 1, got
    assert got["warned"] == [], got
    assert got["child"].get("answer") == 42, got
    assert got["parent_new"] == 42, got
    assert got["parent_held"] is True, got


def test_the_armed_addon_forks_single_threaded(dsn, monkeypatch) -> None:
    conf = os.environ.get("RUSTORM_ODOO_CONF")
    if not conf:
        pytest.skip("RUSTORM_ODOO_CONF must name a conf that serves the database")
    import psycopg

    db = psycopg.conninfo.conninfo_to_dict(dsn)["dbname"]
    monkeypatch.setenv("FORK_PROBE_DB", db)
    monkeypatch.setenv("FORK_PROBE_ROOT", str(ROOT))
    got = _run(
        """
        import importlib.util, pathlib
        from odoo.tools import config

        at_fork = []
        # registered first, so it runs after every hook the addon registers
        os.register_at_fork(before=lambda: at_fork.append(threads()))
        config.parse_config([
            "-c", os.environ["RUSTORM_ODOO_CONF"], "-d", os.environ["FORK_PROBE_DB"],
            "--no-http",
        ])
        config["rust_engine_mode"] = "on"
        config["rust_engine_db"] = os.environ["FORK_PROBE_DB"]
        config["rust_engine_report_seconds"] = 3600
        spec = importlib.util.spec_from_file_location(
            "rust_engine_fork_probe",
            pathlib.Path(os.environ["FORK_PROBE_ROOT"]) / "addons/rust_engine/__init__.py",
        )
        addon = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(addon)
        addon.start(config)
        addon._report_here()
        import threading
        armed = [t.name for t in threading.enumerate()]
        addon._read_params()
        rust_db = addon._STATE["rust_db"]

        def child():
            return {
                "answer": query(rust_db),
                "tick": addon._STATE.get("tick") is not None,
                "switch": addon._STATE["switch_conn"] is not None,
                "params": isinstance(addon._read_params(), dict),
            }

        answer, warned = fork(child)
        tick = addon._STATE.get("tick")
        print(json.dumps({
            "armed": armed,
            "at_fork": at_fork,
            "warned": warned,
            "child": answer,
            "parent_tick": tick is not None and tick[0].is_alive(),
            "parent_answer": query(rust_db),
            "parent_params": isinstance(addon._read_params(), dict),
        }))
        """
    )
    assert sorted(got["armed"]) == ["MainThread", "rust_engine.tick"], got
    assert [len(names) for names in got["at_fork"]] == [1], got
    assert got["warned"] == [], got
    assert got["child"] == {
        "answer": 42,
        "tick": False,
        "switch": False,
        "params": True,
    }, got
    assert got["parent_tick"] is True, got
    assert got["parent_answer"] == 42, got
    assert got["parent_params"] is True, got
