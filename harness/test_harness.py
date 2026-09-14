import importlib.util
import json
import os
import pathlib
import subprocess
import sys

import pytest

HERE = pathlib.Path(pathlib.Path(__file__).resolve()).parent
sys.path.insert(0, str(HERE))

import pathlib

import _env


def load(name):
    spec = importlib.util.spec_from_file_location(
        name, os.path.join(HERE, name + ".py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


diff = load("diff")


def test_dsn_mirrors_config_rs(monkeypatch) -> None:
    monkeypatch.delenv("RUSTORM_DSN", raising=False)
    monkeypatch.setenv("RUSTORM_PGHOST", "/tmp/pg")
    monkeypatch.setenv("RUSTORM_PGUSER", "odoo")
    assert _env.dsn_for("mydb") == "host=/tmp/pg user=odoo dbname=mydb"
    monkeypatch.setenv("RUSTORM_DSN", "host=db.internal port=6543 user=odoo dbname=old")
    assert _env.dsn_for() == "host=db.internal port=6543 user=odoo dbname=old"
    assert _env.dsn_for("new") == "host=db.internal port=6543 user=odoo dbname=new"
    monkeypatch.setenv("RUSTORM_DSN", "postgres://u@h/olddb")
    assert _env.dsn_for("new") == "postgres://u@h/new"


def test_harness_dir_derives_from_the_file(monkeypatch) -> None:
    monkeypatch.delenv("RUSTORM_HARNESS", raising=False)
    assert _env.harness_dir() == HERE
    assert pathlib.Path(os.path.join(_env.engine_python_dir(), "wire.py")).exists()


def test_out_path_is_per_process(monkeypatch) -> None:
    monkeypatch.delenv("RUSTORM_VERIFY_OUT", raising=False)
    monkeypatch.delenv("RUSTORM_FUZZ_OUT", raising=False)
    path = _env.out_path("fuzz_corpus.json", "RUSTORM_FUZZ_OUT")
    assert str(os.getpid()) in path
    monkeypatch.setenv("RUSTORM_FUZZ_OUT", "/x/y.json")
    assert _env.out_path("fuzz_corpus.json", "RUSTORM_FUZZ_OUT") == "/x/y.json"


def test_percentiles() -> None:
    assert _env.p50([3, 1, 2]) == 2
    assert _env.p95(list(range(100))) == 95


OK = {"ok": True, "result": [1]}


def test_an_internal_kernel_error_is_a_fail_not_a_refusal() -> None:
    internal = {"ok": False, "kind": "internal", "error": "db error: syntax error"}
    refusal = {
        "ok": False,
        "kind": "refusal",
        "error": "res.users overrides the read path",
    }
    _p, f, r, *_ = diff.score({"c": OK}, {"c": internal})
    assert f and not r and "internal" in f[0][1]
    _p, f, r, *_ = diff.score({"c": OK}, {"c": refusal})
    assert r and not f
    untagged_db = {"ok": False, "error": "db error: ERROR: relation does not exist"}
    _p, f, r, *_ = diff.score({"c": OK}, {"c": untagged_db})
    assert f and f[0][0] != "<floor>"
    raised = {"ok": False, "error": "ValueError: x", "error_type": "ValueError"}
    _p, f, r, _s, v, _d = diff.score({"c": raised}, {"c": internal})
    assert f and not v


def test_referenced_paths_recurse_into_any_and_nested_lists() -> None:
    src = pathlib.Path(os.path.join(HERE, "gen_expected.py")).read_text(
        encoding="utf-8"
    )
    ns = {}
    exec(  # noqa: S102  the harness scripts are run as text, the way verify.sh runs them
        src[src.index("def domain_paths") : src.index("CASE_KEYS = ")], ns
    )
    paths = ns["referenced_paths"](
        {
            "domain": [
                "|",
                ["country_id", "any", [["state_ids", "any", [["code", "=", "x"]]]]],
                [["parent_id.name", "=", "z"]],
            ],
            "fields": ["name"],
            "groupby": ["create_date:month"],
            "aggregates": ["__count", "color:sum"],
            "order": "amount:sum desc, __count",
        }
    )
    assert paths == {
        "name",
        "country_id",
        "country_id.state_ids",
        "country_id.state_ids.code",
        "parent_id.name",
        "create_date",
        "color",
        "amount",
    }


def stamped(cases, db="d", fp=1):
    return {"db": db, "data_fingerprint": fp, "has_unaccent": True, "cases": cases}


def test_json_output_carries_every_bucket(tmp_path) -> None:
    exp = tmp_path / "exp.json"
    act = tmp_path / "act.json"
    out = tmp_path / "diff.json"
    exp.write_text(
        json.dumps(
            stamped(
                [
                    {"id": "a", "ok": True, "result": [1]},
                    {"id": "b", "ok": True, "result": [1]},
                    {"id": "c", "ok": False, "skipped": "model x is not installed"},
                    {"id": "d", "ok": True, "result": [1]},
                ]
            )
        )
    )
    act.write_text(
        json.dumps(
            stamped(
                [
                    {"id": "a", "ok": True, "result": [1]},
                    {"id": "b", "ok": True, "result": [2]},
                    {
                        "id": "c",
                        "ok": False,
                        "kind": "refusal",
                        "error": "unknown model",
                    },
                    {"id": "d", "ok": False, "kind": "refusal", "error": "declined"},
                ]
            )
        )
    )
    proc = subprocess.run(  # noqa: PLW1510  the exit code is the assertion below
        [
            sys.executable,
            os.path.join(HERE, "diff.py"),
            str(exp),
            str(act),
            "--json",
            str(out),
        ],
        capture_output=True,
        text=True,
    )
    assert proc.returncode == 1
    assert proc.stdout.startswith("FAIL 1/4")
    got = json.loads(out.read_text())
    assert got["verdict"] == "FAIL"
    assert [f["id"] for f in got["failed"]] == ["b"]
    assert [r["id"] for r in got["refused"]] == ["d"]
    assert [s["id"] for s in got["skipped"]] == ["c"]
    assert got["compared"] == 1


def test_a_run_the_kernel_mostly_declined_is_not_a_pass(tmp_path) -> None:
    # refusals never fail a case, so a kernel that declines nine in ten used
    # to score zero failures; the share of the run it declined is capped
    exp = tmp_path / "exp.json"
    act = tmp_path / "act.json"
    exp.write_text(
        json.dumps(stamped([{"id": c, "ok": True, "result": [1]} for c in "abcd"]))
    )
    act.write_text(
        json.dumps(
            stamped(
                [{"id": "a", "ok": True, "result": [1]}]
                + [
                    {"id": c, "ok": False, "kind": "refusal", "error": "declined"}
                    for c in "bcd"
                ]
            )
        )
    )

    def run(*extra):
        return subprocess.run(  # noqa: PLW1510  the exit code is the assertion
            [sys.executable, os.path.join(HERE, "diff.py"), str(exp), str(act), *extra],
            capture_output=True,
            text=True,
        )

    proc = run()
    assert proc.returncode == 1, proc.stdout
    assert proc.stdout.startswith("FAIL"), proc.stdout
    assert "DECLINED 75%" in proc.stdout
    assert "the kernel declined 3 of the 4 cases" in proc.stdout

    proc = run("--max-refused-share=0.8")
    assert proc.returncode == 0, proc.stdout
    assert proc.stdout.startswith("PASS 1/4"), proc.stdout


def test_fuzz_families_census() -> None:
    src = pathlib.Path(os.path.join(HERE, "fuzz_corpus.py")).read_text(encoding="utf-8")
    ns = {}
    exec(  # noqa: S102  the harness scripts are run as text, the way verify.sh runs them
        src[src.index("def domain_families") : src.index("def main(env)")], ns
    )
    fam = ns["families"]
    assert fam({"domain": [["id", "child_of", 1]], "offset": 3}) == {
        "child_of",
        "offset",
    }
    assert fam({"domain": [["a", "any", [["b", "=?", 1]]]]}) == {"=?"}
    assert fam(
        {
            "groupby": ["d:month", "x"],
            "aggregates": ["__count", "n:sum", "m:count_distinct"],
            "tz": "UTC",
            "order": "n asc nulls first, id",
        }
    ) == {"granularity", "two_level_groupby", "sum", "count_distinct", "tz", "nulls"}


@pytest.mark.parametrize(
    "script",
    [
        "gen_expected.py",
        "sweep_corpus.py",
        "fuzz_corpus.py",
        "load_into_odoo.py",
        "copy_path.py",
        "replay.py",
        "bench_python.py",
    ],
)
def test_shell_scripts_bootstrap_without_file(script) -> None:
    src = pathlib.Path(os.path.join(HERE, script)).read_text(encoding="utf-8")
    assert '"__file__" in globals()' in src, (
        "%s must survive exec() without __file__" % script
    )
