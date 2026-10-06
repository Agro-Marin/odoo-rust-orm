import ast
import importlib.util
import pathlib
import subprocess
import sys
from types import SimpleNamespace
from unittest.mock import Mock

import pytest

from odoo.exceptions import AccessError, UserError


@pytest.fixture
def probe(monkeypatch):
    stats = {"kernel": 10}
    set_mode = Mock()
    shim = SimpleNamespace(STATS=stats, set_mode=set_mode)
    monkeypatch.setitem(sys.modules, "rust_orm_shim", shim)
    spec = importlib.util.spec_from_file_location(
        "read_probe", pathlib.Path(__file__).with_name("read_probe.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    env = SimpleNamespace(cr=SimpleNamespace(rollback=Mock()))
    return env, stats, set_mode, module.run_read_and_rollback


@pytest.mark.parametrize("mode", ["on", "off"])
@pytest.mark.parametrize("delta", [-1, 0, 1])
def test_result_identity_and_routing(probe, mode, delta):
    env, stats, set_mode, run_read_and_rollback = probe
    answer = object()

    def call():
        set_mode.assert_called_once_with(mode)
        env.cr.rollback.assert_not_called()
        stats["kernel"] += delta
        return answer

    result, routed = run_read_and_rollback(env, mode, call)
    assert result is answer
    assert routed is (delta > 0)
    env.cr.rollback.assert_called_once_with()


@pytest.mark.parametrize("error", [AccessError, UserError])
def test_expected_errors_are_comparable_names(probe, error):
    env, stats, _, run_read_and_rollback = probe

    def call():
        stats["kernel"] += 1
        raise error("refused")

    assert run_read_and_rollback(env, "on", call) == (error.__name__, True)
    env.cr.rollback.assert_called_once_with()


def test_unexpected_error_propagates_after_rollback(probe):
    env, _, _, run_read_and_rollback = probe
    error = RuntimeError("unexpected")
    with pytest.raises(RuntimeError) as raised:
        run_read_and_rollback(env, "on", Mock(side_effect=error))
    assert raised.value is error
    env.cr.rollback.assert_called_once_with()


def test_rollback_error_takes_precedence(probe):
    env, _, _, run_read_and_rollback = probe
    error = RuntimeError("rollback failed")
    env.cr.rollback.side_effect = error
    with pytest.raises(RuntimeError) as raised:
        run_read_and_rollback(env, "on", Mock(side_effect=UserError("read failed")))
    assert raised.value is error


def test_mode_failure_does_not_call_or_rollback(probe):
    env, _, set_mode, run_read_and_rollback = probe
    set_mode.side_effect = ValueError("mode failed")
    call = Mock()
    with pytest.raises(ValueError, match="mode failed"):
        run_read_and_rollback(env, "on", call)
    call.assert_not_called()
    env.cr.rollback.assert_not_called()


def test_routing_is_measured_after_rollback(probe):
    env, stats, _, run_read_and_rollback = probe
    env.cr.rollback.side_effect = lambda: stats.update(kernel=11)
    assert run_read_and_rollback(env, "off", lambda: "answer") == ("answer", True)


def test_current_cursor_is_resolved_after_call(probe):
    env, _, _, run_read_and_rollback = probe
    original_cursor = env.cr
    replacement = SimpleNamespace(rollback=Mock())

    def call():
        env.cr = replacement

    run_read_and_rollback(env, "off", call)
    original_cursor.rollback.assert_not_called()
    replacement.rollback.assert_called_once_with()


def test_shell_launcher_exposes_shared_probe(tmp_path):
    root = pathlib.Path(__file__).resolve().parents[1]
    source = (root / "harness/verify.sh").read_text()
    function = source[
        source.index("shell_script() {") : source.index("\npython_script() {")
    ]
    fake_odoo = tmp_path / "odoo-bin"
    fake_odoo.write_text("import sys\nprint(sys.stdin.read())\n")
    script = root / "harness/every_user.py"
    command = f'T=()\nPY="$1"\nODOO="$2"\nROOT="$3"\nRUSTORM_ODOO_CONF=unused\nDB=unused\n{function}\nshell_script "$4"'
    process = subprocess.run(
        [
            "bash",
            "-c",
            command,
            "test",
            sys.executable,
            str(tmp_path),
            str(root),
            str(script),
        ],
        capture_output=True,
        text=True,
        check=True,
    )
    code = ast.parse(process.stdout)
    # Execute the real import bootstrap while stopping before the database script.
    namespace = {}
    exec(  # noqa: S102  execute the launcher bootstrap captured from verify.sh
        compile(ast.Module(body=code.body[:-1], type_ignores=[]), "launcher", "exec"),
        namespace,
    )
    try:
        assert sys.path[0] == str(root / "harness")
        assert namespace["runpy"].run_path.__name__ == "run_path"
    finally:
        sys.path.pop(0)
